//! Claude Desktop 的配置接管与共享文件操作。
//!
//! 网关本身不写用户文件；只有显式应用/撤销才改 Claude Desktop 的 configLibrary
//! entry/profile。首次写入前留整份备份，收据放在应用数据目录；撤销按收据逐项还原，
//! 用户改过的值保留并报告，不静默覆盖。
pub mod desktop;
pub mod ownership;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{Error, Result};

/// 用户手写文件的备份后缀：`_meta.json` → `_meta.json.haxsd-byok.bak`。
pub const BACKUP_SUFFIX: &str = "haxsd-byok.bak";
pub const RECEIPT_VERSION: u32 = 1;
/// `_meta.json` 里我们的条目名。
pub const DESKTOP_ENTRY_NAME: &str = "haxsd byok";

/// 模型家族：只用于 Desktop profile 的 `anthropicFamilyTier`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    Sonnet,
    Opus,
    Haiku,
}

impl Family {
    pub const ALL: [Self; 3] = [Self::Sonnet, Self::Opus, Self::Haiku];

    pub const fn tier(self) -> &'static str {
        match self {
            Self::Sonnet => "sonnet",
            Self::Opus => "opus",
            Self::Haiku => "haiku",
        }
    }

    /// 从显示名或厂商标识里认家族；`opus` 优先，避免名字里同时出现多个家族时误判。
    pub fn from_hint(text: &str) -> Option<Self> {
        let text = text.to_ascii_lowercase();
        if text.contains("opus") {
            Some(Self::Opus)
        } else if text.contains("sonnet") {
            Some(Self::Sonnet)
        } else if text.contains("haiku") {
            Some(Self::Haiku)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeoverState {
    Missing,
    Ready,
    /// 文件与收据一致，且收据仍是当前网关的期望值。
    Applied,
    /// 文件与收据一致，但收据已经不是当前网关的期望值（换令牌、换端口、换模型、
    /// 网关停用或模型缺失）：客户端配置有效却过期，需要重新应用。
    Stale,
    Conflict,
    Managed,
    Running,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    None,
    Local,
    UserPolicy,
    MachinePolicy,
}

/// 一条冲突或提示。`code` 是给界面分支用的稳定标识，`message` 是给人看的说明。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TakeoverConflict {
    pub code: &'static str,
    pub key: Option<String>,
    pub message: String,
    /// `true`：写入会覆盖别人的配置，必须拒绝；`false`：只提示，不挡路。
    pub blocking: bool,
}

impl TakeoverConflict {
    pub fn new(code: &'static str, message: impl Into<String>, blocking: bool) -> Self {
        Self {
            code,
            key: None,
            message: message.into(),
            blocking,
        }
    }

    pub fn keyed(
        code: &'static str,
        key: impl Into<String>,
        message: impl Into<String>,
        blocking: bool,
    ) -> Self {
        Self {
            code,
            key: Some(key.into()),
            message: message.into(),
            blocking,
        }
    }
}

/// 状态检查时已知的网关值：判断客户端 profile 是否仍指向当前网关。
#[derive(Clone, Copy, Debug)]
pub struct GatewayValues<'a> {
    pub base_url: &'a str,
    pub token: &'a str,
    /// 网关已启用、正在监听、端口一致、默认模型存在，并且没有缺失模型。
    pub available: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DesktopTakeoverStatus {
    pub state: TakeoverState,
    pub config_source: ConfigSource,
    pub profile_path: String,
    pub backup_path: String,
    /// 收据属于当前安装目录，撤销才有依据。
    pub restorable: bool,
    pub conflicts: Vec<TakeoverConflict>,
    /// 机器/用户策略存在时只给引导：v1 不写 HKCU 策略，也不覆盖 HKLM 策略。
    pub guided: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct TakeoverStatus {
    pub desktop: DesktopTakeoverStatus,
}

/// 收据目录：`<应用数据目录>/claude-takeover/`。只解析路径，读状态时不创建目录；
/// 真正的写入由 `atomic_write` 创建父目录。
pub fn state_dir() -> Result<PathBuf> {
    Ok(crate::config::managed_data_dir()?.join("claude-takeover"))
}

/// 用户手写文件的备份路径。
pub(crate) fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".");
    name.push(BACKUP_SUFFIX);
    path.with_file_name(name)
}

/// 第一次改写文件之前留一份整份备份；已有备份不覆盖，保证它始终是首次接管前的那一份。
pub(crate) fn backup_once(path: &Path) -> Result<Option<PathBuf>> {
    if !path.is_file() {
        return Ok(None);
    }
    let backup = backup_path(path);
    if backup.exists() {
        return Ok(Some(backup));
    }
    if let Some(parent) = backup.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(path, &backup)?;
    tracing::info!(path = %path.display(), backup = %backup.display(), "backed up a Claude configuration file");
    Ok(Some(backup))
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config(format!("{} has no parent directory", path.display())))?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| Error::Io(error.error))?;
    Ok(())
}

pub(crate) fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// 严格 JSON：注释、尾逗号都会报错。文件带 UTF-8 BOM 时先剥掉，避免把记事本存过的
/// 文件误判成坏配置。
pub(crate) fn parse_json(text: &str) -> Result<serde_json::Value> {
    // `char::from_u32` 而不是字面量：BOM 在源码里不可见，写出来容易被编辑器吃掉。
    let text = text.trim_start_matches(char::from_u32(0xFEFF).unwrap_or('\0'));
    Ok(serde_json::from_str(text)?)
}

pub(crate) fn content_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))
}

pub(crate) fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Windows 的路径比较不区分大小写；收据里的路径与当前解析结果要按同一口径比较。
pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 桌面端解析器读的是同一份 JSON：服务端字段一改，Rust 与 Node 两个测试都会失败，
    /// 而不是让界面静默丢掉冲突或撤销依据。
    const TAKEOVER_FIXTURE: &str =
        include_str!("../../../apps/desktop/tests/fixtures/claude-takeover-status.json");

    #[test]
    fn the_desktop_fixture_matches_the_server_takeover_status() {
        let status = TakeoverStatus {
            desktop: DesktopTakeoverStatus {
                state: TakeoverState::Applied,
                config_source: ConfigSource::Local,
                profile_path: "C:\\Users\\me\\AppData\\Local\\Claude-3p\\configLibrary\\8f0c2a4e-1d3b-4f6e-9a7c-2b5d8e1f4a6c.json".into(),
                backup_path: String::new(),
                restorable: true,
                conflicts: Vec::new(),
                guided: false,
            },
        };
        let actual = serde_json::to_value(&status).unwrap();
        let expected: serde_json::Value = serde_json::from_str(TAKEOVER_FIXTURE).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn families_are_recognised_from_display_names() {
        assert_eq!(Family::from_hint("Claude Opus 4"), Some(Family::Opus));
        assert_eq!(Family::from_hint("claude-sonnet-4-5"), Some(Family::Sonnet));
        assert_eq!(Family::from_hint("haiku"), Some(Family::Haiku));
        assert_eq!(Family::from_hint("deepseek"), None);
    }
}
