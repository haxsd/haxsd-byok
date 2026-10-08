//! Captures startup diagnostics before the database and desktop runtime initialize.
use std::{error::Error, path::PathBuf};

use rfd::{MessageButtons, MessageDialog, MessageLevel};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const LOG_DIRECTORY_NAME: &str = "logs";
const LOG_FILE_PREFIX: &str = "haxsd-byok";
const LOG_FILE_SUFFIX: &str = "log";
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
/// 由设置页写、`store::set_desktop_settings` 落盘，改完重启生效。
const DEVELOPER_LOG_FILTER: &str = "haxsd_byok_desktop=debug,cursor_server=debug,hudsucker=debug,tauri_plugin_updater=info,hyper=warn,hyper_util=warn,rustls=warn,reqwest=warn,sqlx=warn";

type BoxError = Box<dyn Error + Send + Sync>;

pub(crate) struct StartupDiagnostics {
    log_directory: PathBuf,
    _writer_guard: WorkerGuard,
}

impl StartupDiagnostics {
    pub(crate) fn initialize() -> Result<Self, BoxError> {
        let log_directory = cursor_server::config::managed_data_dir()?.join(LOG_DIRECTORY_NAME);
        std::fs::create_dir_all(&log_directory)?;

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
            _writer_guard: writer_guard,
        })
    }

    pub(crate) fn log_directory(&self) -> &std::path::Path {
        &self.log_directory
    }

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
