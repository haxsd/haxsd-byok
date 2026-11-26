//! Codex 桌面界面的 Statsig 本地缓存备份与恢复。
//!
//! Codex 的侧栏、语言和 Work/chat 布局由 Statsig 远程开关决定；启动时没有网络又没有
//! 本地缓存时会回退到旧版界面。这里只处理 Codex 自己的 `Local Storage` 和 stable ID，
//! 不伪造远程配置，也不在 Codex 运行时替换它的 LevelDB 目录。

use crate::{config, Error, Result};
use serde::Serialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::warn;

const CACHE_KEY: &[u8] = b"statsig.cached.evaluations.";
const LOCAL_STORAGE: &str = "Default/Local Storage";
const STATSIG_STATE: &str = "statsig-state.json";
const BACKUP_DIR: &str = "codex-ui-cache";
const MAX_SCAN_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiCacheState {
    /// 当前 Codex 配置里已经有可用缓存。
    Ready,
    /// 当前缓存缺失，但本机保存了可用备份；Codex 未运行时可以恢复。
    Restorable,
    /// 有备份，但 Codex 正在运行，不能安全替换它打开的 LevelDB。
    Blocked,
    /// 既没有当前缓存，也没有可恢复备份，需要一次正常联网启动。
    Missing,
    /// 找不到 Codex 用户数据目录（非 Windows 或尚未安装）。
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UiCacheStatus {
    pub state: UiCacheState,
    pub cache_present: bool,
    pub backup_present: bool,
    pub codex_running: bool,
}

struct Paths {
    profile: PathBuf,
    backup: PathBuf,
}

/// 页面读取时顺手做一次备份/恢复：正常界面的缓存会被保存；缓存缺失且 Codex 未运行
/// 时自动恢复。失败只记录日志，页面仍能拿到真实状态。
pub fn ensure() -> UiCacheStatus {
    let Some(paths) = paths() else {
        return unsupported();
    };
    let before = inspect_paths(&paths.profile, &paths.backup, codex_running());
    if before.cache_present {
        if let Err(error) = backup_cache(&paths.profile, &paths.backup) {
            warn!(%error, "failed to back up Codex UI cache");
        }
    } else if before.backup_present && !before.codex_running {
        if let Err(error) = restore_cache(&paths.profile, &paths.backup) {
            warn!(%error, "failed to restore Codex UI cache");
        }
    }
    inspect_paths(&paths.profile, &paths.backup, codex_running())
}

/// 显式修复按钮：只做一次用户要求的恢复，失败会返回给管理 API。
pub fn repair() -> Result<UiCacheStatus> {
    let Some(paths) = paths() else {
        return Ok(unsupported());
    };
    let before = inspect_paths(&paths.profile, &paths.backup, codex_running());
    if before.cache_present {
        backup_cache(&paths.profile, &paths.backup)?;
    } else if before.backup_present && !before.codex_running {
        restore_cache(&paths.profile, &paths.backup)?;
    }
    Ok(inspect_paths(
        &paths.profile,
        &paths.backup,
        codex_running(),
    ))
}

pub fn inspect() -> UiCacheStatus {
    let Some(paths) = paths() else {
        return unsupported();
    };
    inspect_paths(&paths.profile, &paths.backup, codex_running())
}

fn unsupported() -> UiCacheStatus {
    UiCacheStatus {
        state: UiCacheState::Unsupported,
        cache_present: false,
        backup_present: false,
        codex_running: false,
    }
}

fn paths() -> Option<Paths> {
    let profile = codex_profile()?;
    let backup = config::managed_data_dir().ok()?.join(BACKUP_DIR);
    Some(Paths { profile, backup })
}

fn inspect_paths(profile: &Path, backup: &Path, running: bool) -> UiCacheStatus {
    let cache_present = has_cache(&profile.join(LOCAL_STORAGE));
    let backup_present =
        has_cache(&backup.join(LOCAL_STORAGE)) && backup.join(STATSIG_STATE).is_file();
    let state = if cache_present {
        UiCacheState::Ready
    } else if backup_present && running {
        UiCacheState::Blocked
    } else if backup_present {
        UiCacheState::Restorable
    } else {
        UiCacheState::Missing
    };
    UiCacheStatus {
        state,
        cache_present,
        backup_present,
        codex_running: running,
    }
}

/// 从 Local Storage 的 LevelDB 文件里找 Statsig 评估缓存键。
///
/// 这里只读文件字节，不解析 LevelDB；缓存键是固定前缀，Codex 自己会在启动时做
/// stable ID 和值校验。误把没有缓存键的目录当成可用，比报缓存缺失更危险。
fn has_cache(local_storage: &Path) -> bool {
    let leveldb = local_storage.join("leveldb");
    let Ok(entries) = fs::read_dir(leveldb) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_SCAN_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        if bytes
            .windows(CACHE_KEY.len())
            .any(|window| window == CACHE_KEY)
        {
            return true;
        }
    }
    false
}

fn backup_cache(profile: &Path, backup: &Path) -> Result<()> {
    let source = profile.join(LOCAL_STORAGE);
    if !has_cache(&source) {
        return Err(Error::Config(
            "Codex UI cache is not present; start Codex once with network access".into(),
        ));
    }
    let parent = backup
        .parent()
        .ok_or_else(|| Error::Config("Codex UI cache backup path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".{BACKUP_DIR}-{}", unique_suffix()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    copy_tree(&source, &staging.join(LOCAL_STORAGE))?;
    let state = profile.join(STATSIG_STATE);
    if state.is_file() {
        fs::copy(&state, staging.join(STATSIG_STATE))?;
    }
    if backup.exists() {
        fs::remove_dir_all(backup)?;
    }
    fs::rename(&staging, backup)?;
    Ok(())
}

fn restore_cache(profile: &Path, backup: &Path) -> Result<()> {
    restore_cache_if_stopped(profile, backup, codex_running())
}

fn restore_cache_if_stopped(profile: &Path, backup: &Path, running: bool) -> Result<()> {
    let source = backup.join(LOCAL_STORAGE);
    if !has_cache(&source) {
        return Err(Error::Config(
            "no Codex UI cache backup is available".into(),
        ));
    }
    // 调用者已经确认 Codex 没有运行；这里再检查一次，避免按钮和进程状态之间的竞态。
    if running {
        return Err(Error::Config(
            "quit Codex completely before restoring its UI cache".into(),
        ));
    }
    let staging = profile.join(format!(".Local Storage.restore-{}", unique_suffix()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    copy_tree(&source, &staging)?;
    let current = profile.join(LOCAL_STORAGE);
    let previous = profile.join(format!(".Local Storage.previous-{}", unique_suffix()));
    if current.exists() {
        fs::rename(&current, &previous)?;
    }
    if let Err(error) = fs::rename(&staging, &current) {
        if previous.exists() {
            let _ = fs::rename(&previous, &current);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(error.into());
    }
    if previous.exists() {
        fs::remove_dir_all(previous)?;
    }
    let state = backup.join(STATSIG_STATE);
    if state.is_file() {
        fs::copy(state, profile.join(STATSIG_STATE))?;
    }
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        // Codex 打开 LevelDB 时会持锁；备份不需要把锁文件带走。
        if name == "LOCK" || name == "lockfile" {
            continue;
        }
        let from = entry.path();
        let to = target.join(&name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

#[cfg(windows)]
fn codex_profile() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)?;
    let packages = local.join("Packages");
    if let Ok(entries) = fs::read_dir(&packages) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("OpenAI.Codex_") {
                continue;
            }
            let profile = entry
                .path()
                .join("LocalCache")
                .join("Roaming")
                .join("Codex")
                .join("web")
                .join("Codex");
            if profile.is_dir() {
                return Some(profile);
            }
        }
    }
    // 少数安装方式不会把 Codex 的 Electron 数据放进 MSIX 的 LocalCache。
    let fallback = dirs::data_dir()?.join("Codex").join("web").join("Codex");
    fallback.is_dir().then_some(fallback)
}

#[cfg(not(windows))]
fn codex_profile() -> Option<PathBuf> {
    None
}

#[cfg(windows)]
pub(crate) fn codex_running() -> bool {
    use std::os::windows::process::CommandExt;
    let mut command = std::process::Command::new("tasklist");
    // tasklist 会弹控制台窗口；页面每次读取缓存状态都会调用它，必须隐藏。
    command.creation_flags(0x0800_0000);
    let output = command
        .args(["/FI", "IMAGENAME eq ChatGPT.exe", "/FO", "CSV", "/NH"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            tasklist_contains_codex(&String::from_utf8_lossy(&output.stdout))
        }
        _ => true,
    }
}

#[cfg(not(windows))]
pub(crate) fn codex_running() -> bool {
    false
}

#[cfg(windows)]
fn tasklist_contains_codex(output: &str) -> bool {
    output.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        line.contains("\"chatgpt.exe\"") && !line.contains("no tasks")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with_cache(root: &Path) -> PathBuf {
        let profile = root.join("profile");
        let leveldb = profile.join(LOCAL_STORAGE).join("leveldb");
        fs::create_dir_all(&leveldb).unwrap();
        fs::write(
            leveldb.join("000001.ldb"),
            b"prefix\0statsig.cached.evaluations.1242878314\0suffix",
        )
        .unwrap();
        fs::write(
            profile.join(STATSIG_STATE),
            br#"{"stableID":"test-stable-id"}"#,
        )
        .unwrap();
        profile
    }

    #[test]
    fn backs_up_cache_and_restores_it_after_local_storage_disappears() {
        let root = tempfile::tempdir().unwrap();
        let profile = profile_with_cache(root.path());
        let backup = root.path().join("backup");
        assert_eq!(
            inspect_paths(&profile, &backup, false).state,
            UiCacheState::Ready
        );

        backup_cache(&profile, &backup).unwrap();
        fs::remove_dir_all(profile.join(LOCAL_STORAGE)).unwrap();
        let missing = inspect_paths(&profile, &backup, false);
        assert_eq!(missing.state, UiCacheState::Restorable);
        assert!(!missing.cache_present);
        assert!(missing.backup_present);

        restore_cache_if_stopped(&profile, &backup, false).unwrap();
        let restored = inspect_paths(&profile, &backup, false);
        assert_eq!(restored.state, UiCacheState::Ready);
        assert!(profile.join(LOCAL_STORAGE).join("leveldb").exists());
        assert_eq!(
            fs::read(profile.join(STATSIG_STATE)).unwrap(),
            br#"{"stableID":"test-stable-id"}"#
        );
    }

    #[test]
    fn running_codex_blocks_restore_even_when_backup_exists() {
        let root = tempfile::tempdir().unwrap();
        let profile = profile_with_cache(root.path());
        let backup = root.path().join("backup");
        backup_cache(&profile, &backup).unwrap();
        fs::remove_dir_all(profile.join(LOCAL_STORAGE)).unwrap();
        let blocked = inspect_paths(&profile, &backup, true);
        assert_eq!(blocked.state, UiCacheState::Blocked);
        assert!(blocked.codex_running);
        assert!(restore_cache_if_stopped(&profile, &backup, true).is_err());
        assert!(!profile.join(LOCAL_STORAGE).exists());
    }

    #[test]
    fn a_leveldb_without_statsig_key_is_not_accepted_as_cache() {
        let root = tempfile::tempdir().unwrap();
        let local = root.path().join(LOCAL_STORAGE);
        fs::create_dir_all(local.join("leveldb")).unwrap();
        fs::write(local.join("leveldb").join("000001.ldb"), b"unrelated").unwrap();
        assert!(!has_cache(&local));
    }

    #[cfg(windows)]
    #[test]
    fn tasklist_output_is_parsed_without_matching_the_no_tasks_message() {
        assert!(tasklist_contains_codex(
            "\"ChatGPT.exe\",\"12824\",\"Console\",\"1\",\"123,456 K\"\r\n"
        ));
        assert!(!tasklist_contains_codex(
            "INFO: No tasks are running which match the specified criteria.\r\n"
        ));
    }
}
