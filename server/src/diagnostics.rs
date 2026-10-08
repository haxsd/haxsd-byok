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

use crate::{config, Result};

/// 抓取文件的目录名，与 `tracing_appender` 的滚动日志同目录。
const LOG_DIRECTORY_NAME: &str = "logs";
/// 滚动日志的文件名前缀，用来找"最近一份应用日志"。
const LOG_FILE_PREFIX: &str = "haxsd-byok";
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

fn log_directory() -> Result<PathBuf> {
    let directory = config::managed_data_dir()?.join(LOG_DIRECTORY_NAME);
    std::fs::create_dir_all(&directory)?;
    Ok(directory)
}

fn newest_log_file(directory: &Path) -> Option<PathBuf> {
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

        assert_eq!(newest_log_file(directory.path()), Some(newer));
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
}
