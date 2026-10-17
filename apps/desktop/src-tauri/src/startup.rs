//! Captures startup diagnostics before the database and desktop runtime initialize.
use std::{
    error::Error,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use cursor_server::diagnostics::{LOG_FILE_PREFIX, LOG_FILE_SUFFIX};
use rfd::{MessageButtons, MessageDialog, MessageLevel};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// 日志目录与文件名取自 `cursor_server::diagnostics`：那是同一个约定的两端（桌面壳在这里
/// 建文件，服务端在那边按名字找「最近一份应用日志」）。两侧各写一份字面量时，任何一侧改名
/// 都会让诊断快照静默丢掉日志尾部。
const RETAINED_LOG_FILES: usize = 15;

/// 默认过滤：自家两条线都是 `info`，第三方只放 `warn` 以上。
///
/// 第三方必须显式列出来：`EnvFilter` 一旦给出具体目标，没提到的目标就是关闭的。
/// 之前 `hudsucker`（本地代理的 MITM 实现）整条线被丢掉，而"连不上代理/CONNECT
/// 失败/TLS 握手失败"恰恰只由它记录——报错时日志里什么都没有，就是这么来的。
/// `tauri_plugin_updater` 也必须在列表里：更新失败（尤其被系统策略拦下）过去在文件
/// 日志里一行都不留，用户报"更新失败"时无从查起。
/// 需要更细的现场时用 `RUST_LOG=hudsucker=debug,cursor_server=debug` 启动，
/// 或在设置里打开「开发者模式」。
const DEFAULT_LOG_FILTER: &str = "haxsd_byok_desktop=info,cursor_server=info,tauri_plugin_updater=info,hudsucker=warn,hyper=warn,hyper_util=warn,rustls=warn,reqwest=warn,sqlx=warn";

/// 开发者模式下的过滤：自家两条线与本地代理都放开到 `debug`。
///
/// 开关存在数据目录的 `dev-mode.json` 镜像里（数据库还没打开，只能读文件），
/// 由设置页写、`store::update_desktop_settings` 落盘，改完重启生效。
const DEVELOPER_LOG_FILTER: &str = "haxsd_byok_desktop=debug,cursor_server=debug,hudsucker=debug,tauri_plugin_updater=info,hyper=warn,hyper_util=warn,rustls=warn,reqwest=warn,sqlx=warn";

type BoxError = Box<dyn Error + Send + Sync>;

pub(crate) struct StartupDiagnostics {
    log_directory: PathBuf,
    log_flush: Arc<LogFlush>,
}

/// 进程结束前必须显式落盘的日志写入器。
///
/// `non_blocking` 把每行交给后台线程写文件，`WorkerGuard` 是"排空队列 + 结束线程"的把手。
/// 应用结束时 tauri 直接 `std::process::exit`（`app.rs:1346` "This function never returns"），
/// 栈上的 `WorkerGuard` 不会被 drop，队列里剩下的行就没了——而"退出时收尾跑到哪一步"恰好
/// 只由最后几行回答（`cleared Cursor proxy configuration` / `graceful shutdown timed out`）。
/// 退出路径必须在 `process::exit` 之前调 `flush()`。
pub(crate) struct LogFlush(Mutex<Option<WorkerGuard>>);

impl LogFlush {
    fn new(guard: WorkerGuard) -> Self {
        Self(Mutex::new(Some(guard)))
    }

    /// 没有写入器的落盘把手：给"只关心报告/结束进程顺序"的用例用。
    #[cfg(test)]
    pub(crate) fn inert() -> Self {
        Self(Mutex::new(None))
    }

    /// 排空队列并结束后台写线程；重复调用是空操作。
    pub(crate) fn flush(&self) {
        let guard = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(guard);
    }
}

/// 启动失败要用的最小信息：日志目录。
///
/// `StartupDiagnostics` 自身不能进 setup 闭包：它持有的 `WorkerGuard` 里面有
/// `mpsc::Sender`，因此 `!Sync`，装进 `Arc` 也不再是 `Send`，而 tauri 的 setup 闭包要求
/// `Send`。对话框真正需要的只有"日志在哪"这一行，所以单独给它一个可移动的出口。
#[derive(Clone)]
pub(crate) struct FatalReporter {
    log_directory: PathBuf,
}

impl StartupDiagnostics {
    pub(crate) fn initialize() -> Result<Self, BoxError> {
        let log_directory = cursor_server::diagnostics::log_directory()?;

        let file_appender = RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix(LOG_FILE_PREFIX)
            .filename_suffix(LOG_FILE_SUFFIX)
            .max_log_files(RETAINED_LOG_FILES)
            .build(&log_directory)?;
        let (file_writer, writer_guard) = tracing_appender::non_blocking(file_appender);
        // 开发者模式只能从数据目录的镜像读（数据库还没打开）；缺失或损坏按关闭处理。
        let developer_mode = cursor_server::config::dev_mode_enabled();
        let default_filter = if developer_mode {
            DEVELOPER_LOG_FILTER
        } else {
            DEFAULT_LOG_FILTER
        };
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| default_filter.into());

        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(file_writer),
            )
            .try_init()?;

        tracing::info!(developer_mode, default_filter, "log filtering configured");

        cursor_server::diagnostics::set_app_version(env!("CARGO_PKG_VERSION"));
        install_panic_hook();

        Ok(Self {
            log_directory,
            log_flush: Arc::new(LogFlush::new(writer_guard)),
        })
    }

    pub(crate) fn log_directory(&self) -> &std::path::Path {
        &self.log_directory
    }

    /// 退出路径用的落盘把手（见 `LogFlush`）。
    pub(crate) fn log_flush(&self) -> Arc<LogFlush> {
        self.log_flush.clone()
    }

    /// 给 setup 闭包用的失败出口（见 `FatalReporter`）。
    pub(crate) fn fatal_reporter(&self) -> FatalReporter {
        FatalReporter {
            log_directory: self.log_directory.clone(),
        }
    }

    pub(crate) fn report_fatal(&self, error: &(dyn Error + 'static)) {
        self.fatal_reporter().report_fatal(error);
    }
}

impl FatalReporter {
    /// 启动失败的统一出口：日志 + 原生对话框（含"把最新日志发给开发者"与日志目录）。
    pub(crate) fn report_fatal(&self, error: &(dyn Error + 'static)) {
        let details = cursor_server::diagnostics::error_chain_multiline(error);
        tracing::error!(
            error = %details,
            log_directory = %self.log_directory.display(),
            "desktop failed to start"
        );
        show_fatal_dialog(&details, Some(&self.log_directory));
    }
}

/// panic 也要进日志并抓一份现场：默认 hook 只写 stderr，而打包后的应用没有控制台。
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".to_owned());
        let location = info
            .location()
            .map(ToString::to_string)
            .unwrap_or_else(|| "unknown location".to_owned());
        tracing::error!(%payload, %location, "panic");
        cursor_server::diagnostics::capture_panic(&payload, &location);
        previous(info);
    }));
}

pub(crate) fn report_logging_failure(error: &(dyn Error + 'static)) {
    let details = cursor_server::diagnostics::error_chain_multiline(error);
    eprintln!("haxsd byok failed to initialize logging: {details}");
    show_fatal_dialog(&details, None);
}

fn show_fatal_dialog(details: &str, log_directory: Option<&std::path::Path>) {
    let log_guidance = match log_directory {
        Some(directory) => format!(
            "日志目录 / Log directory:\n{}\n\n请将最新的日志文件发送给开发者。\nPlease send the latest log file to the developer.",
            directory.display()
        ),
        None => "日志系统也未能启动，因此没有生成日志文件。\nLogging also failed to initialize, so no log file was created."
            .to_owned(),
    };
    let description = format!(
        "haxsd byok 无法启动 / failed to start.\n\n错误 / Error:\n{details}\n\n{log_guidance}"
    );

    let _ = MessageDialog::new()
        .set_level(MessageLevel::Error)
        .set_title("haxsd byok 启动失败 / Startup Error")
        .set_description(description)
        .set_buttons(MessageButtons::Ok)
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 退出前 `flush()` 必须把队列里最后一行写进文件，并真正结束写入线程。
    ///
    /// 应用结束时 tauri 直接 `std::process::exit`（不跑析构），`WorkerGuard` 不会被 drop：
    /// 不显式落盘，"退出收尾跑到哪一步"的最后几行就只留在内存里。
    ///
    /// 两个断言缺一不可：只断言"最后一行在文件里"拦不住空实现的 `flush()`——后台线程通常
    /// 早就写完了；"flush 之后的写入进不去"才是"写入器已经被拿走并结束"的确定性证据。
    #[test]
    fn flushing_the_log_writer_lands_the_last_line() {
        let directory = std::env::temp_dir().join(format!("haxsd-log-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("fixture directory");

        let appender = RollingFileAppender::builder()
            .rotation(Rotation::NEVER)
            .filename_prefix(LOG_FILE_PREFIX)
            .filename_suffix(LOG_FILE_SUFFIX)
            .build(&directory)
            .expect("file appender");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let flush = LogFlush::new(guard);

        // 只影响当前线程：不装全局订阅器（进程里可能已经有一个）。
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("the last line before exit");
            flush.flush();
            // 重复调用是空操作（退出路径可能被走两次）。
            flush.flush();
            tracing::info!("a line after the flush");
        });

        let file = std::fs::read_dir(&directory)
            .expect("log directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.is_file())
            .expect("a log file");
        let contents = std::fs::read_to_string(&file).expect("log contents");
        std::fs::remove_dir_all(&directory).ok();

        assert!(
            contents.contains("the last line before exit"),
            "flush() 之后最后一行必须已经在文件里: {file:?} = {contents:?}"
        );
        assert!(
            !contents.contains("a line after the flush"),
            "flush() 必须真的拿走写入器（退出时没有第二次机会）: {contents:?}"
        );
    }
}
