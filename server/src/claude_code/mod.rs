//! Claude Code CLI 与 Claude Desktop 的配置接管。
//!
//! 两条接管路径都只写本应用负责的键：CLI 只改 `settings.json` 的 `env`，Desktop 只写
//! configLibrary 里自己的 entry/profile。首次写入前留整份备份，收据放在应用数据目录；
//! 撤销按收据逐键还原，用户改过的值保留并报告，不静默覆盖。
pub mod desktop;
pub mod ownership;
pub mod settings;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{Error, Result};

/// Claude Code 读的入口地址；不带 `/v1`。
pub const BASE_URL_KEY: &str = "ANTHROPIC_BASE_URL";
pub const AUTH_TOKEN_KEY: &str = "ANTHROPIC_AUTH_TOKEN";
pub const ATTRIBUTION_HEADER_KEY: &str = "CLAUDE_CODE_ATTRIBUTION_HEADER";
pub const ATTRIBUTION_VALUE: &str = "0";
/// 接管必须让位的键：已有非我们的值时拒绝覆盖。
pub const API_KEY_KEY: &str = "ANTHROPIC_API_KEY";
pub const BEDROCK_KEY: &str = "CLAUDE_CODE_USE_BEDROCK";
pub const VERTEX_KEY: &str = "CLAUDE_CODE_USE_VERTEX";
pub const FOUNDRY_KEY: &str = "CLAUDE_CODE_USE_FOUNDRY";
/// 顶层键，不在 `env` 里；它会盖过 `ANTHROPIC_AUTH_TOKEN`。
pub const API_KEY_HELPER_KEY: &str = "apiKeyHelper";

/// 用户手写文件的备份后缀：`settings.json` → `settings.json.haxsd-byok.bak`。
pub const BACKUP_SUFFIX: &str = "haxsd-byok.bak";
pub const RECEIPT_VERSION: u32 = 1;
/// `_meta.json` 里我们的条目名。
pub const DESKTOP_ENTRY_NAME: &str = "haxsd byok";

/// 模型家族：决定 CLI 的 `ANTHROPIC_DEFAULT_*` 键与 Desktop 的 `anthropicFamilyTier`。
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

    pub const fn model_env(self) -> &'static str {
        match self {
            Self::Sonnet => "ANTHROPIC_DEFAULT_SONNET_MODEL",
            Self::Opus => "ANTHROPIC_DEFAULT_OPUS_MODEL",
            Self::Haiku => "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        }
    }

    pub const fn name_env(self) -> &'static str {
        match self {
            Self::Sonnet => "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            Self::Opus => "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            Self::Haiku => "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
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

/// 我们可能写进 CLI `env` 的全部键。`ANTHROPIC_MODEL`、`ANTHROPIC_API_KEY`、
/// `CLAUDE_CODE_OAUTH_TOKEN` 都不在列表里：它们属于用户或客户端自己的选择。
pub const MANAGED_ENV_KEYS: [&str; 9] = [
    BASE_URL_KEY,
    AUTH_TOKEN_KEY,
    ATTRIBUTION_HEADER_KEY,
    Family::Sonnet.model_env(),
    Family::Sonnet.name_env(),
    Family::Opus.model_env(),
    Family::Opus.name_env(),
    Family::Haiku.model_env(),
    Family::Haiku.name_env(),
];

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

#[derive(Clone, Debug, Serialize)]
pub struct CliTakeoverStatus {
    pub state: TakeoverState,
    pub settings_path: String,
    pub backup_path: String,
    /// 收据属于当前配置目录，撤销才有依据；备份文件本身不足以还原。
    pub restorable: bool,
    pub conflicts: Vec<TakeoverConflict>,
    /// 设置里选中了、但模型库里已经不存在的哈希。
    pub missing_models: Vec<String>,
    /// 生效的受管策略文件；任意一个存在就说明 `settings.json` 不会被读取。
    pub managed_sources: Vec<String>,
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
    pub cli: CliTakeoverStatus,
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
    use std::collections::BTreeSet;

    use super::*;

    /// 桌面端解析器读的是同一份 JSON：服务端字段一改，Rust 与 Node 两个测试都会失败，
    /// 而不是让界面静默丢掉冲突或撤销依据。
    const TAKEOVER_FIXTURE: &str =
        include_str!("../../../apps/desktop/tests/fixtures/claude-takeover-status.json");

    #[test]
    fn the_desktop_fixture_matches_the_server_takeover_status() {
        let status = TakeoverStatus {
            cli: CliTakeoverStatus {
                state: TakeoverState::Conflict,
                settings_path: "C:\\Users\\me\\.claude\\settings.json".into(),
                backup_path: "C:\\Users\\me\\.claude\\settings.json.haxsd-byok.bak".into(),
                restorable: true,
                conflicts: vec![
                    TakeoverConflict::keyed(
                        "env_base_url",
                        "ANTHROPIC_BASE_URL",
                        "env.ANTHROPIC_BASE_URL is already set to https://api.anthropic.com; applying would replace it",
                        true,
                    ),
                    TakeoverConflict::keyed(
                        "user_modified",
                        "ANTHROPIC_DEFAULT_SONNET_MODEL",
                        "env.ANTHROPIC_DEFAULT_SONNET_MODEL changed after the takeover; undo will keep the current value",
                        true,
                    ),
                    TakeoverConflict::keyed(
                        "shell_env",
                        "ANTHROPIC_AUTH_TOKEN",
                        "ANTHROPIC_AUTH_TOKEN=[redacted] is also set in the user environment; Claude Code started outside these settings may use it",
                        false,
                    ),
                ],
                missing_models: vec!["gone3333".into()],
                managed_sources: vec![
                    "C:\\Program Files\\ClaudeCode\\managed-settings.d\\10-company.json".into(),
                ],
            },
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

    /// 9 个键：入口地址、凭据、归属头，以及三个家族各自的模型与显示名。
    #[test]
    fn managed_env_keys_name_the_model_and_its_display_name_for_every_family() {
        assert_eq!(MANAGED_ENV_KEYS.len(), 9);
        let unique = MANAGED_ENV_KEYS.iter().collect::<BTreeSet<_>>();
        assert_eq!(
            unique.len(),
            MANAGED_ENV_KEYS.len(),
            "duplicate managed key"
        );
        for family in Family::ALL {
            assert!(MANAGED_ENV_KEYS.contains(&family.model_env()));
            assert!(MANAGED_ENV_KEYS.contains(&family.name_env()));
            assert_eq!(
                family.name_env(),
                format!(
                    "ANTHROPIC_DEFAULT_{}_MODEL_NAME",
                    family.tier().to_uppercase()
                )
            );
        }
        assert!(MANAGED_ENV_KEYS.contains(&ATTRIBUTION_HEADER_KEY));
    }
}
