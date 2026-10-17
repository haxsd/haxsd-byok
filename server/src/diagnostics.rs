//! 失败现场的自包含记录：报错时把关键状态与日志尾部单独落一份文件。
//!
//! 用户遇到断流时就无法再和助手对话，只能事后排查；那时进程内的状态早就没了。
//! 所以这里不缓存任何东西：每次抓取都把环境、Cursor 的代理配置、系统代理与最近
//! 一段应用日志重新读一遍，写成一个独立文件放进 `logs/`。

use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde::Serialize;

use crate::{config, Result};

/// 抓取文件的目录名，与 `tracing_appender` 的滚动日志同目录。
///
/// 这三个名字是**同一个约定的两端**：写日志的一侧（桌面壳 `startup.rs`、独立二进制
/// `bin/cursor-server.rs`）按它们建文件，读日志的一侧（本模块的 `newest_log_file`）按它们
/// 找「最近一份应用日志」。所以它们是 `pub`：两侧各写一份字面量时，任何一侧改名都会让诊断
/// 快照静默丢掉日志尾部，而快照正是「用户只发一个文件给开发者」的那份证据。
pub const LOG_DIRECTORY_NAME: &str = "logs";
/// 滚动日志的文件名前缀，用来找"最近一份应用日志"。
pub const LOG_FILE_PREFIX: &str = "haxsd-byok";
/// 滚动日志的文件名后缀（不含点号，`tracing_appender` 约定）。
pub const LOG_FILE_SUFFIX: &str = "log";
const CAPTURE_FILE_PREFIX: &str = "diagnostics-";
/// 两次抓取之间的最短间隔：一次故障通常会连着触发多个事件，只留第一份。
const CAPTURE_COOLDOWN: Duration = Duration::from_secs(30);
/// 保留的抓取文件数量，超出的按时间从旧到新删。
const RETAINED_CAPTURES: usize = 10;
/// 附带的日志尾部大小。够覆盖一次故障前后的几十秒到几分钟。
const LOG_TAIL_BYTES: u64 = 256 * 1024;

static LAST_CAPTURE: Mutex<Option<Instant>> = Mutex::new(None);
static APP_VERSION: OnceLock<String> = OnceLock::new();

/// 记住桌面壳的版本号，诊断文件里就同时有外壳与服务端的版本。
pub fn set_app_version(version: &str) {
    let _ = APP_VERSION.set(version.to_owned());
}

/// 桌面壳的版本号；诊断快照与 `/api/app-info` 都读它。外壳还没写进来时是 `None`。
pub fn app_version() -> Option<&'static str> {
    APP_VERSION.get().map(String::as_str)
}

/// 单行错误链：`最外层: 原因: 根因`。
///
/// 日志与恢复出来的失败信息都走它。`reqwest` / `sqlx` 的失败原因只在 `source()`
/// 里，只打 `Display` 会丢掉"连不上、TLS 失败、认证失败"这类唯一有用的部分。
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut details = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        details.push_str(": ");
        details.push_str(&cause.to_string());
        source = cause.source();
    }
    details
}

/// 多行错误链，留给弹窗与日志里需要跨行阅读的地方。
pub fn error_chain_multiline(error: &(dyn std::error::Error + 'static)) -> String {
    let mut details = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        details.push_str("\nCaused by: ");
        details.push_str(&cause.to_string());
        source = cause.source();
    }
    details
}

/// 抓取一次失败现场：`logs/diagnostics-<UTC 时间戳>.log`。
///
/// 文件里有：抓取原因、调用方给的事实、环境快照、最近一份应用日志的尾部。
/// 永不 panic、永不上抛：它总是在别的失败路径里被调用。
pub fn capture(reason: &str, facts: serde_json::Value) {
    let now = Instant::now();
    {
        let mut last = LAST_CAPTURE.lock();
        if last.is_some_and(|previous| now.duration_since(previous) < CAPTURE_COOLDOWN) {
            return;
        }
        *last = Some(now);
    }
    let Ok(directory) = log_directory() else {
        return;
    };
    let path = directory.join(format!(
        "{CAPTURE_FILE_PREFIX}{}.log",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
    ));
    let mut body = String::new();
    body.push_str("=== haxsd byok diagnostics ===\n");
    body.push_str(&format!("reason: {reason}\n"));
    body.push_str(&format!(
        "captured_at: {}\n",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    ));
    body.push_str("\n--- environment ---\n");
    body.push_str(&serde_json::to_string_pretty(&environment()).unwrap_or_default());
    body.push_str("\n\n--- facts ---\n");
    body.push_str(&serde_json::to_string_pretty(&facts).unwrap_or_default());
    match newest_log_file(&directory)
        .as_deref()
        .and_then(|file| read_tail(file, LOG_TAIL_BYTES))
    {
        Some(tail) => {
            body.push_str("\n\n--- application log tail ---\n");
            body.push_str(&tail);
            body.push('\n');
        }
        None => body.push_str("\n\n--- application log tail: unavailable ---\n"),
    }
    if let Err(error) = std::fs::write(&path, body) {
        tracing::warn!(
            error = %error_chain(&error),
            path = %path.display(),
            "failed to write diagnostics capture"
        );
        return;
    }
    prune_captures(&directory);
    tracing::warn!(capture = %path.display(), reason, "captured diagnostics snapshot");
}

/// panic 的抓取入口：桌面壳的 panic hook 用它，省得那边也要拼 JSON。
pub fn capture_panic(payload: &str, location: &str) {
    capture(
        "panic",
        serde_json::json!({ "payload": payload, "location": location }),
    );
}

/// 环境快照：本进程能自证的事实，加上 Cursor 与系统两处代理状态。
fn environment() -> serde_json::Value {
    let data_dir = config::managed_data_dir()
        .map(|directory| directory.display().to_string())
        .unwrap_or_else(|error| format!("unavailable: {error}"));
    serde_json::json!({
        "app_version": app_version().unwrap_or("unknown"),
        "server_version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "process_id": std::process::id(),
        "data_dir": data_dir,
        // 谁在管 Cursor 的代理配置：这是"报错时到底谁写的配置"的第一现场。
        "cursor_settings": crate::local_app::proxy_settings_snapshot()
            .unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() })),
        // 指纹变化就等于系统代理被开关过；`Default` 模式下它决定出网走哪里。
        "system_proxy": crate::network::system_proxy_fingerprint(),
    })
}

/// 滚动日志目录（`<数据目录>/logs`），不存在就建出来。
///
/// 写日志的一侧也要用它：目录与文件名的约定只在这里定义一次（见上面的常量）。
pub fn log_directory() -> Result<PathBuf> {
    let directory = config::managed_data_dir()?.join(LOG_DIRECTORY_NAME);
    std::fs::create_dir_all(&directory)?;
    Ok(directory)
}

/// 滚动日志的保留期。
///
/// 日志只用于事后排查（真正的故障现场由 `diagnostics-*` 快照单独留一份），但按天滚动
/// 的日志不删就只增不减：实测用户机器上 17 天累计 226 MB，开发者模式下一天十几到几十 MB。
/// 保留一周足够覆盖"昨天还好好的，今天不行了"这类排查，也不会把数据目录喂大。
const LOG_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 抓取结果缓存的目录名：`<数据目录>/cache/web`。
///
/// 写入方是 `search::cache::WebCache::managed()`（同一段拼接），这里只负责只读统计与清空：
/// 设置页一挂载就会读统计，所以**读路径不建目录**——读操作不该有副作用。
pub const CACHE_DIRECTORY_NAME: &str = "cache";
pub const WEB_CACHE_DIRECTORY_NAME: &str = "web";

/// 抓取结果缓存目录；解析不出来（数据目录不可用）是错误。
pub fn web_cache_directory() -> Result<PathBuf> {
    Ok(config::managed_data_dir()?
        .join(CACHE_DIRECTORY_NAME)
        .join(WEB_CACHE_DIRECTORY_NAME))
}

/// 一个目录的当前占用：直接子文件的个数与字节数（缓存是平铺一层，不递归）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DirectoryUsage {
    pub files: u64,
    pub bytes: u64,
}

/// 抓取结果缓存的当前占用。目录不可用或读不动时是 0：统计接口不该因此失败。
pub fn web_cache_usage() -> DirectoryUsage {
    match web_cache_directory() {
        Ok(directory) => directory_usage_in(&directory),
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the web cache directory");
            DirectoryUsage::default()
        }
    }
}

/// 清空抓取结果缓存，返回删掉的量。
pub fn clear_web_cache() -> Result<DirectoryUsage> {
    clear_web_cache_in(&web_cache_directory()?)
}

fn directory_usage_in(directory: &Path) -> DirectoryUsage {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return DirectoryUsage::default();
    };
    let mut usage = DirectoryUsage::default();
    for entry in entries.filter_map(std::result::Result::ok) {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        usage.files += 1;
        usage.bytes += metadata.len();
    }
    usage
}

/// 删掉目录里的文件、保留目录本身；返回删掉的量。
///
/// 目录不存在不算失败（从没抓过东西就没有缓存）；单个文件删不掉只记警告并留在原地——
/// 用户点完"清空缓存"看到的数字就是真实剩下的量，不假装清干净了。
///
/// 目录作为参数是为了让 `WebCache` 作用域与用例共用同一条实现（同 `prune_logs_in`）：
/// 用例必须对着临时目录跑，不能去删用户的真实缓存。
pub(crate) fn clear_web_cache_in(directory: &Path) -> Result<DirectoryUsage> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DirectoryUsage::default())
        }
        Err(error) => return Err(error.into()),
    };
    let mut cleared = DirectoryUsage::default();
    for entry in entries.filter_map(std::result::Result::ok) {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {
                cleared.files += 1;
                cleared.bytes += metadata.len();
            }
            Err(error) => tracing::warn!(
                path = %entry.path().display(),
                %error,
                "cannot remove a cached web page"
            ),
        }
    }
    Ok(cleared)
}

/// 删掉超过保留期的滚动日志与故障快照；返回删掉的文件数。
///
/// 只按修改时间判断，不解析文件名里的日期：重命名过的日志、手工拷进来的日志同样适用。
/// 目录不存在或个别文件删不掉都不算失败——这是清理，不该影响任何主流程。
pub fn prune_logs() -> Result<usize> {
    let directory = log_directory()?;
    prune_logs_in(&directory, LOG_RETENTION)
}

fn prune_logs_in(directory: &Path, retention: Duration) -> Result<usize> {
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(retention) else {
        return Ok(0);
    };
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut removed = 0;
    for entry in entries.filter_map(std::result::Result::ok) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_ours = (name.starts_with(LOG_FILE_PREFIX) || name.starts_with(CAPTURE_FILE_PREFIX))
            && name.ends_with(".log");
        if !is_ours {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified >= cutoff {
            continue;
        }
        if let Err(error) = std::fs::remove_file(entry.path()) {
            tracing::warn!(%error, path = %entry.path().display(), "old log file could not be removed");
            continue;
        }
        removed += 1;
    }
    Ok(removed)
}

/// 「最近一份」的年龄上界：超过它的日志不再是这次故障的现场。
///
/// 单跑 `cursor-server`（或桌面壳还没写过日志、日志目录里只有上次运行的旧文件）时，把旧
/// 日志贴进快照会让读的人以为是这次的后台记录——比「unavailable」更误导。宁可如实说没有。
const LOG_FRESHNESS: Duration = Duration::from_secs(10 * 60);

fn newest_log_file(directory: &Path) -> Option<PathBuf> {
    newest_log_file_at(directory, std::time::SystemTime::now())
}

fn newest_log_file_at(directory: &Path, now: std::time::SystemTime) -> Option<PathBuf> {
    let cutoff = now.checked_sub(LOG_FRESHNESS)?;
    let entries = std::fs::read_dir(directory).ok()?;
    entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(LOG_FILE_PREFIX) && name.ends_with(".log")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .filter(|(modified, _)| *modified >= cutoff)
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

fn read_tail(path: &Path, bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buffer = Vec::with_capacity((length - start) as usize);
    file.read_to_end(&mut buffer).ok()?;
    let mut text = String::from_utf8_lossy(&buffer).into_owned();
    if start > 0 {
        // 截断处可能是半行，明确标出来，免得读的人以为日志就长这样。
        text.insert_str(0, &format!("...<{start} bytes dropped>\n"));
    }
    Some(text)
}

fn prune_captures(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut captures: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(CAPTURE_FILE_PREFIX)
        })
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    if captures.len() <= RETAINED_CAPTURES {
        return;
    }
    captures.sort_by_key(|(modified, _)| *modified);
    let excess = captures.len() - RETAINED_CAPTURES;
    for (_, path) in captures.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    #[derive(Debug)]
    struct OuterError(std::io::Error);

    impl fmt::Display for OuterError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("database initialization failed")
        }
    }

    impl std::error::Error for OuterError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// 缓存占用要能被读到：`cache/web` 以前既不进统计也不在界面上，用户只能自己翻目录
    /// 才知道有多少东西堆在那儿。
    #[test]
    fn the_web_cache_usage_counts_files_and_bytes() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("one.txt"), b"12345").unwrap();
        std::fs::write(directory.path().join("two.txt"), b"123").unwrap();
        std::fs::create_dir(directory.path().join("nested")).unwrap();

        let usage = directory_usage_in(directory.path());

        assert_eq!(usage.files, 2, "子目录不算缓存文件");
        assert_eq!(usage.bytes, 8);
        assert_eq!(
            directory_usage_in(&directory.path().join("missing")),
            DirectoryUsage::default()
        );
    }

    /// 清空缓存：删文件、留目录，返回删掉的量；再清一次是 0（幂等）。
    #[test]
    fn clearing_the_web_cache_removes_files_and_keeps_the_directory() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("page.txt"), b"abcdefg").unwrap();
        std::fs::create_dir(directory.path().join("nested")).unwrap();

        let cleared = clear_web_cache_in(directory.path()).unwrap();

        assert_eq!(cleared.files, 1);
        assert_eq!(cleared.bytes, 7);
        assert!(
            directory.path().is_dir(),
            "目录要留着：服务端还要往里写新的抓取结果"
        );
        assert!(directory.path().join("nested").is_dir(), "子目录不碰");
        assert_eq!(
            clear_web_cache_in(directory.path()).unwrap(),
            DirectoryUsage::default(),
            "再清一次没有东西可删，也不该报错"
        );
    }

    /// 目录不存在不是错误：没抓过东西的用户点"清空缓存"不该看到失败。
    #[test]
    fn clearing_a_missing_web_cache_is_not_an_error() {
        let directory = tempfile::tempdir().unwrap();

        let cleared = clear_web_cache_in(&directory.path().join("never-created")).unwrap();

        assert_eq!(cleared, DirectoryUsage::default());
    }

    /// 缓存目录必须落在 `<数据目录>/cache/web`：写入方（`search::cache::WebCache::managed()`）
    /// 用的是同一段拼接，任何一侧改名都会让统计和"清空缓存"静默指向一个空目录。
    ///
    /// 这里只钉住自己这一侧的拼接结果与数据目录的关系；把写入方也改成读
    /// `web_cache_directory()`（一处约定）属于 `search/**` 的改动，不在本批写域。
    #[test]
    fn the_web_cache_directory_sits_under_the_data_directory() {
        let directory = web_cache_directory().unwrap();

        assert_eq!(
            directory,
            config::managed_data_dir()
                .unwrap()
                .join("cache")
                .join("web")
        );
        assert!(!directory.exists() || directory.is_dir());
    }

    #[test]
    fn error_chain_keeps_every_cause_on_one_line() {
        let error = OuterError(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "database file is read-only",
        ));

        assert_eq!(
            error_chain(&error),
            "database initialization failed: database file is read-only"
        );
        assert_eq!(
            error_chain_multiline(&error),
            "database initialization failed\nCaused by: database file is read-only"
        );
    }

    #[test]
    fn log_tail_keeps_the_end_of_the_file_and_marks_the_dropped_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("haxsd-byok.2026-09-28.log");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();

        let tail = read_tail(&path, 7).expect("read tail");
        assert!(tail.starts_with("...<12 bytes dropped>\n"), "{tail}");
        assert!(tail.ends_with("third\n"), "{tail}");

        assert_eq!(
            read_tail(&path, 1024).expect("read whole file"),
            "first\nsecond\nthird\n"
        );
    }

    #[test]
    fn newest_log_file_picks_the_latest_matching_file() {
        let directory = tempfile::tempdir().unwrap();
        let older = directory.path().join("haxsd-byok.2026-09-27.log");
        let newer = directory.path().join("haxsd-byok.2026-09-28.log");
        let ignored = directory.path().join("diagnostics-2026-09-28.log");
        write_with_modified(&older, "older", 1);
        write_with_modified(&ignored, "capture", 3);
        write_with_modified(&newer, "newer", 2);

        let now = std::time::UNIX_EPOCH + Duration::from_secs(30);
        assert_eq!(newest_log_file_at(directory.path(), now), Some(newer));
    }

    /// 只有「新鲜」的日志才是这次故障的现场：旧日志宁可如实写 unavailable。
    ///
    /// 单跑 `cursor-server` 时数据目录里往往只有桌面应用留下的旧日志，贴进快照会让读的人
    /// 以为是这次的后台记录。
    #[test]
    fn newest_log_file_ignores_stale_logs() {
        let directory = tempfile::tempdir().unwrap();
        let stale = directory.path().join("haxsd-byok.2026-09-27.log");
        let fresh = directory.path().join("haxsd-byok.2026-09-28.log");
        let now = std::time::UNIX_EPOCH + Duration::from_secs(10_000);
        write_with_modified(&stale, "stale", 10_000 - 601);
        write_with_modified(&fresh, "fresh", 10_000 - 599);

        assert_eq!(newest_log_file_at(directory.path(), now), Some(fresh));

        // 两份都过期：回 None（快照里写 unavailable），而不是挑一份旧日志。
        let later = now + Duration::from_secs(60 * 60);
        assert_eq!(newest_log_file_at(directory.path(), later), None);
    }

    /// 抓取文件与滚动日志同名时也要能挑出后者，所以时间戳直接写死，别靠文件系统的时钟。
    fn write_with_modified(path: &Path, contents: &str, seconds: u64) {
        std::fs::write(path, contents).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds))
            .unwrap();
    }

    /// 日志按天滚动、不删就只增不减：实测 17 天 226 MB。保留期只按修改时间判断。
    #[test]
    fn logs_older_than_the_retention_window_are_removed() {
        let directory = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let day = 24 * 60 * 60;
        let retention = Duration::from_secs(7 * day);

        write_with_modified(
            &directory.path().join("haxsd-byok.2026-10-01.log"),
            "old",
            now - 8 * day,
        );
        write_with_modified(
            &directory
                .path()
                .join("diagnostics-20261001T000000.000Z.log"),
            "old",
            now - 30 * day,
        );
        write_with_modified(
            &directory.path().join("haxsd-byok.2026-10-15.log"),
            "recent",
            now - day,
        );
        write_with_modified(
            &directory.path().join("haxsd-byok.2026-10-16.log"),
            "today",
            now,
        );
        // 别人的文件不碰。
        write_with_modified(
            &directory.path().join("other.log"),
            "not ours",
            now - 30 * day,
        );
        write_with_modified(
            &directory.path().join("notes.txt"),
            "not a log",
            now - 30 * day,
        );

        assert_eq!(prune_logs_in(directory.path(), retention).unwrap(), 2);
        let left = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            left.contains(&"haxsd-byok.2026-10-16.log".to_owned()),
            "{left:?}"
        );
        assert!(left.contains(&"other.log".to_owned()), "{left:?}");
        assert!(left.contains(&"notes.txt".to_owned()), "{left:?}");
        assert!(
            !left.iter().any(|name| name.contains("2026-10-01")),
            "{left:?}"
        );
        assert!(
            !left.iter().any(|name| name.starts_with("diagnostics-")),
            "{left:?}"
        );
    }

    /// 目录不存在时不是错误：清理不该影响任何主流程。
    #[test]
    fn pruning_a_missing_directory_reports_nothing_removed() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("no-such-logs");
        assert_eq!(prune_logs_in(&missing, Duration::from_secs(60)).unwrap(), 0);
    }
}
