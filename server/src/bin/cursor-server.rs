//! Starts the haxsd byok server executable.
use cursor_server::{diagnostics, App, Config, Result};
use tracing_appender::{
    non_blocking::{NonBlocking, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    // 名字不能是 `_`：`WorkerGuard` 一被 drop 就结束写入线程并落盘，`_` 会立刻吃掉它。
    let _log_guard = install_logging();
    App::new(Config::from_env()?).await?.serve().await
}

/// 装 stdout + 文件两条日志线，文件那条与桌面壳同源（目录与文件名都取自 `diagnostics`）。
///
/// 以前这里只有 stdout：`diagnostics::capture` 仍会去数据目录找 `haxsd-byok*.log`，于是独立
/// 跑服务端时快照会挂上桌面应用留下的旧日志尾部，现场对不上；而现在它至少能找到本进程
/// 写的那一份（10 分钟内新鲜度判断由 `newest_log_file` 负责）。
///
/// 文件日志起不来不挡启动：stdout 照旧，原因写 stderr（这时还没有订阅器可写日志）。
fn install_logging() -> Option<WorkerGuard> {
    let (writer, guard) = file_writer()?;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cursor_server=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer),
        )
        .init();
    Some(guard)
}

fn file_writer() -> Option<(NonBlocking, WorkerGuard)> {
    let directory = match diagnostics::log_directory() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("haxsd byok: cannot open the log directory: {error}");
            return None;
        }
    };
    let appender = match RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(diagnostics::LOG_FILE_PREFIX)
        .filename_suffix(diagnostics::LOG_FILE_SUFFIX)
        .build(&directory)
    {
        Ok(appender) => appender,
        Err(error) => {
            eprintln!(
                "haxsd byok: cannot open the log file in {}: {error}",
                directory.display()
            );
            return None;
        }
    };
    // 非阻塞：日志不能挡住请求路径。保留期由 `diagnostics::prune_logs()` 按时间清理（服务端
    // 会定期调用），所以这里不再按文件数封顶。
    Some(tracing_appender::non_blocking(appender))
}
