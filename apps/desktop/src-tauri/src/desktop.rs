use std::{
    error::Error,
    ffi::OsString,
    process::{Command, ExitCode},
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Arc, Mutex,
    },
};

use axum::{
    extract::{Extension, Json},
    http::StatusCode,
    routing::post,
    Router,
};
use tauri::{
    async_runtime::JoinHandle, webview::Color, AppHandle, Manager, RunEvent, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_opener::OpenerExt;
use tokio_util::sync::CancellationToken;

#[cfg(dev)]
use cursor_server::config::ConsoleSource;
use cursor_server::{App, Config, Result};

#[cfg(not(dev))]
use crate::frontend;
use crate::startup::{self, LogFlush, StartupDiagnostics};
use crate::tray;

pub(crate) const MAIN_WINDOW_LABEL: &str = "main";
const AUTOSTART_ARG: &str = "--autostart";
/// 收尾助手的启动参数：见 `cursor_server::local_app::cleanup_after_exit`。
const EXIT_CLEANUP_ARG: &str = "--cleanup-after-exit";
/// setup 失败时的退出码。不是 0：脚本与系统事件日志要能看出这次启动没成功。
const SETUP_FAILURE_EXIT_CODE: i32 = 1;

/// setup 的错误类型：与 tauri `SetupHook` 的签名一致。
type SetupError = Box<dyn Error>;

struct DesktopRuntime {
    shutdown: CancellationToken,
    server: Mutex<Option<JoinHandle<Result<()>>>>,
    exiting: AtomicBool,
    /// 收尾已经跑完、正在用记下的码结束进程（见 `begin_exit`）。
    finishing: AtomicBool,
    /// 这次退出最终交给操作系统的退出码（见 `DesktopRuntime::begin_exit`）。
    exit_code: AtomicI32,
    /// 退出前把日志队列落盘（见 `LogFlush`）：tauri 结束时是 `process::exit`，
    /// 没人 drop `WorkerGuard`。
    log_flush: Arc<LogFlush>,
    server_addr: std::net::SocketAddr,
}

impl DesktopRuntime {
    /// 记下第一次退出请求，并回答"该由这次调用负责收尾吗"。
    ///
    /// 退出码只在第一次写入：服务端异常结束走 `app.exit(1)`，托盘菜单走 `app.exit(0)`，
    /// 收尾结束后的 `app.exit(exit_code())` 是同一次退出的第二次事件。以前收尾那一步写死
    /// `app.exit(0)`，于是"服务端崩了"在外部脚本、系统事件日志、支持流程里与"用户正常
    /// 退出"长得一模一样——日志之外没有任何机器可读的失败信号。
    fn begin_exit(&self, code: i32) -> bool {
        if self.exiting.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.exit_code.store(code, Ordering::Release);
        true
    }

    fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::Acquire)
    }

    /// 收尾结束、准备结束进程：交出第一次退出请求记下的码，并把 `finishing` 置位。
    ///
    /// 当前实现不再用 `app.exit` 结束进程（见 `finish_process`：那条路在
    /// tauri-runtime-wry 2.11.4 上会丢退出码），状态机契约不变。
    ///
    /// 与"收尾途中用户又点了一次托盘退出"区分开：后者不能放行（见 `run()` 的
    /// `ExitRequested` 分支），否则撤回 Cursor 配置那一步会被砍掉，留下指向死端口的代理配置。
    fn finish_exit(&self) -> i32 {
        self.finishing.store(true, Ordering::Release);
        self.exit_code()
    }

    fn finishing(&self) -> bool {
        self.finishing.load(Ordering::Acquire)
    }
}

/// 收尾结束：先把日志落盘，再带着第一次退出请求记下的码结束进程。
///
/// 不能用 `app.exit(code)` 带码退出：`tauri-runtime-wry` **2.11.4** 的
/// `Message::RequestExit` 只把 `control_flow` 置成 `ControlFlow::Exit`
/// （上游 `src/lib.rs:4354-4366`，`Cargo.lock` 里 tauri-runtime-wry = 2.11.4），退出码在这一步
/// 被丢掉，进程恒 0 退出——"服务端崩了"在启动器、安装器与系统事件日志里与"用户正常退出"
/// 长得一模一样。收尾在这之前已经跑完（服务端任务已 await 或超时），这里直接带码
/// `std::process::exit`。`finish_exit()` 仍然要调：它交出既定的码并把 `finishing` 置位，
/// 将来 tauri 修好带码退出、或这里改回 `app.exit` 时，"退出码先到先得"那条接线继续生效。
fn finish_process(
    code_of: impl FnOnce() -> i32,
    flush: impl FnOnce(),
    exit: impl FnOnce(i32),
) -> ! {
    flush();
    exit(code_of());
    unreachable!("the process is gone after exiting with the remembered code")
}

/// 打开日志目录（`<数据目录>\logs`）。
///
/// 只认这一个路径，不接受调用方传入任何字符串——前端拿到的能力就是"打开日志目录"，
/// 不是通用的文件系统访问。前端优先用 `tauri-plugin-opener`；它的路径权限没有开
/// （capabilities 里没有 `opener:allow-open-path`），所以这里给一个窄命令。
#[tauri::command]
fn open_log_directory(app: AppHandle) -> std::result::Result<(), String> {
    let directory = cursor_server::config::managed_data_dir()
        .map_err(|error| error.to_string())?
        .join("logs");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    // `open_path` 收的是 `impl Into<String>`（插件 2.5.4），路径要先变成字符串。
    app.opener()
        .open_path(directory.display().to_string(), None::<&str>)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn open_cursor_ca_install_terminal() -> std::result::Result<(), String> {
    tokio::task::spawn_blocking(crate::ca_install::open_install_terminal)
        .await
        .map_err(|error| format!("CA installation task failed: {error}"))?
}

#[derive(serde::Deserialize)]
struct OpenExternalUrlRequest {
    url: String,
}

fn open_external_url(app: &AppHandle, url: &str) -> std::result::Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("only absolute HTTP and HTTPS URLs are allowed".into());
    }

    app.opener()
        .open_url(parsed.as_str(), None::<&str>)
        .map_err(|error| format!("failed to open URL: {error}"))
}

async fn open_external_url_handler(
    Extension(app): Extension<AppHandle>,
    Json(request): Json<OpenExternalUrlRequest>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    open_external_url(&app, &request.url)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

fn desktop_api_router(app: AppHandle) -> Router {
    Router::new()
        .route(
            "/__byok-api__/api/desktop/open-external-url",
            post(open_external_url_handler),
        )
        .layer(Extension(app))
}

/// 窗口默认尺寸：按显示器可用区域算，而不是写死一个固定值。
///
/// 之前固定 820×558（逻辑像素），在 1536×960 的桌面上只占半屏：导航条右侧的状态区与
/// 搜索入口被裁掉，概览的四个磁贴挤成两列，调用表格要横向滚动。现在取可用区域减去
/// 边距，并设上限，避免在超宽屏上铺满整屏。
///
/// 边距从每边 96 收到 48：在 1536×960 的桌面上原来只能开到 1320×824，正文两侧的
/// 留白比窗口本身还显眼。
fn default_window_size(app: &AppHandle) -> (f64, f64) {
    /// 每边留出的边距，让窗口看起来是放在桌面上而不是贴满。
    const MARGIN: f64 = 48.0;
    const MAX_WIDTH: f64 = 1560.0;
    const MAX_HEIGHT: f64 = 980.0;
    const FALLBACK: (f64, f64) = (1100.0, 720.0);

    let Some(monitor) = app.primary_monitor().ok().flatten() else {
        return FALLBACK;
    };
    // work_area 是物理像素，inner_size 收逻辑像素。
    let scale = monitor.scale_factor();
    let work_area = monitor.work_area();
    let width = work_area.size.width as f64 / scale - MARGIN;
    let height = work_area.size.height as f64 / scale - MARGIN;
    (
        width.clamp(880.0, MAX_WIDTH),
        height.clamp(600.0, MAX_HEIGHT),
    )
}

fn create_main_window(
    app: &AppHandle,
    address: std::net::SocketAddr,
) -> tauri::Result<WebviewWindow> {
    let url = format!("http://{address}/__byok-api__/")
        .parse()
        .expect("local frontend URL");
    let (width, height) = default_window_size(app);
    let builder = WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::External(url))
        .title("haxsd byok")
        .inner_size(width, height)
        .min_inner_size(880.0, 600.0)
        .center()
        .background_color(Color(20, 20, 20, 255))
        .decorations(cfg!(target_os = "macos"))
        .shadow(true)
        .resizable(true)
        .visible(false);

    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true);

    builder.build()
}

/// 按需打开主窗口：首次需要界面时创建；之后关闭按钮只隐藏窗口，页面状态保留。
pub(crate) fn open_main_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }
    let address = app.state::<DesktopRuntime>().server_addr;
    let window = create_main_window(app, address)?;
    window.show()?;
    window.set_focus()?;
    Ok(())
}

/// 主窗口的关闭按钮只收起到托盘；托盘里的显式退出与进程内退出不受影响。
fn close_hides_to_tray(label: &str, explicit_exit: bool) -> bool {
    label == MAIN_WINDOW_LABEL && !explicit_exit
}

/// setup 的**唯一**出口：装配失败时先让人看见，再把日志落盘，最后用非零码结束进程。
///
/// **不能把 `Err` 交回 tauri**：`tauri-2.11.5/src/app.rs:1425` 收到 `Err` 会
/// `panic!("Failed to setup app: {e}")`，而 release 版是 `windows_subsystem = "windows"`
/// （没有控制台），panic 文本无处可见。仓库为"给人看的启动失败"专门写了 `report_fatal`，
/// 每个 `?` 都必须走到它。
///
/// 装配动作与结束进程都参数化，只为一件事：让回归用例跑**同一条出口**——用例传一个必定
/// 失败的装配（`Config::desktop()` 在数据目录不可用时就是真的会失败）和一个记录用的
/// `exit`，生产传 `setup_desktop` 与 `std::process::exit`。把这里改回 `setup()?`，
/// `a_failed_setup_is_reported_and_never_handed_back_to_tauri` 立刻变红。
fn finish_setup<T>(
    setup: impl FnOnce() -> Result<(), SetupError>,
    report: impl FnOnce(&(dyn Error + 'static)),
    log_flush: &LogFlush,
    exit: impl FnOnce(i32) -> T,
) -> Result<(), SetupError> {
    match setup() {
        Ok(()) => Ok(()),
        Err(error) => {
            report(error.as_ref());
            // 结束进程前落盘：`report_fatal` 那一行以及收尾路径的最后几行都还在队列里。
            log_flush.flush();
            // `exit` 在真实调用点上是 `std::process::exit`（返回 `!`，永不返回）。
            let _: T = exit(SETUP_FAILURE_EXIT_CODE);
            unreachable!("the setup failure exit must not return")
        }
    }
}

/// 桌面壳的启动装配：实例锁、数据库、监听端口、窗口与托盘。
///
/// 抽成独立函数只为一件事：给这一串可能失败的初始化一个统一出口（`finish_setup`）。
/// 保留 setup 内执行的时序——`tauri-plugin-single-instance` 的插件 setup 在
/// `Builder::build` 里跑，第二次启动会在那里聚焦已有窗口并 `exit(0)`；把实例锁搬到
/// `Builder::build` 之前会让"重复双击"变成一次吓人的启动失败提示。
fn setup_desktop(
    app: &mut tauri::App,
    started_by_autostart: bool,
    log_flush: &Arc<LogFlush>,
) -> Result<(), SetupError> {
    app.handle().plugin(tauri_plugin_autostart::init(
        tauri_plugin_autostart::MacosLauncher::LaunchAgent,
        Some(vec![AUTOSTART_ARG]),
    ))?;
    let config = {
        let mut config = Config::desktop()?;
        // 插件的 minAppVersion 按桌面应用版本判定,而不是内嵌 server 库的版本。
        config.app_version = env!("CARGO_PKG_VERSION").into();
        config
    };
    #[cfg(dev)]
    let config = {
        let mut config = config;
        config.console = Some(ConsoleSource::Proxy(
            "http://127.0.0.1:1420"
                .parse()
                .expect("Vite development URL"),
        ));
        config
    };
    let server = tauri::async_runtime::block_on(App::new(config))?
        .merge_router(desktop_api_router(app.handle().clone()));
    #[cfg(not(dev))]
    let server = server.merge_router(frontend::router(app.handle().clone()));
    let listener = tauri::async_runtime::block_on(server.bind())?;
    let address = listener.local_addr()?;
    tauri::async_runtime::block_on(server.harness().cleanup_stale_settings())?;
    let desktop_settings = match tauri::async_runtime::block_on(server.store().desktop_settings()) {
        Ok(settings) => {
            // 开发者模式的镜像给**下一次**启动的日志过滤读（设置页保存时也会写），
            // 这里兜住"升级后第一次启动、设置行已有但镜像还不存在"的情况。
            if let Err(error) =
                cursor_server::config::write_dev_mode_mirror(settings.developer_mode)
            {
                tracing::warn!(%error, "failed to sync the developer mode mirror");
            }
            settings
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "desktop settings unavailable at startup; using defaults"
            );
            cursor_server::store::DesktopSettings::default()
        }
    };
    #[cfg(target_os = "macos")]
    app.handle()
        .set_dock_visibility(desktop_settings.show_dock_icon)?;
    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let app_handle = app.handle().clone();
    let task = tauri::async_runtime::spawn(async move {
        let result = server.serve_on(listener, server_shutdown).await;
        if let Err(error) = &result {
            tracing::error!(%error, "desktop server stopped unexpectedly");
            app_handle.exit(1);
        }
        result
    });
    app.manage(DesktopRuntime {
        shutdown,
        server: Mutex::new(Some(task)),
        exiting: AtomicBool::new(false),
        finishing: AtomicBool::new(false),
        exit_code: AtomicI32::new(0),
        log_flush: log_flush.clone(),
        server_addr: address,
    });
    // 强杀/崩溃时没人走 disable()，留下的代理配置会让 Cursor 一直报"连不上代理"。
    // 助手进程专门负责那一刻的收尾。
    spawn_exit_cleanup_helper();
    if desktop_settings.silent_start && started_by_autostart {
        tracing::info!("silent autostart enabled; starting without the main window");
    } else {
        open_main_window(app.handle())?;
    }
    tray::create(app)?;
    Ok(())
}

/// 命令行里有没有这个开关。
///
/// 用 `args_os()` 而不是 `std::env::args()`：后者遇到非 UTF-8 参数（Windows 上是不成对的
/// 代理项，更新器/单实例插件转发参数时真的有）会在迭代时 panic。这一行还在
/// `StartupDiagnostics::initialize()` 之前，release 版又是 `windows_subsystem = "windows"`，
/// 于是"应用毫无痕迹地不启动"、连一条日志都不留。
fn has_argument(arguments: impl IntoIterator<Item = OsString>, flag: &str) -> bool {
    arguments.into_iter().any(|argument| argument == flag)
}

pub fn run() -> ExitCode {
    if has_argument(std::env::args_os(), EXIT_CLEANUP_ARG) {
        return run_exit_cleanup();
    }
    let diagnostics = match StartupDiagnostics::initialize() {
        Ok(diagnostics) => diagnostics,
        Err(error) => {
            startup::report_logging_failure(error.as_ref());
            return ExitCode::FAILURE;
        }
    };
    #[cfg(unix)]
    {
        let open_file_limit = match crate::resource_limits::raise_open_file_limit() {
            Ok(limit) => limit,
            Err(error) => {
                diagnostics.report_fatal(&error);
                return ExitCode::FAILURE;
            }
        };
        tracing::info!(
            requested = crate::resource_limits::REQUESTED_OPEN_FILE_LIMIT,
            previous = open_file_limit.previous,
            effective = open_file_limit.effective,
            hard = open_file_limit.hard,
            "open file limit configured"
        );
    }
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        architecture = std::env::consts::ARCH,
        log_directory = %diagnostics.log_directory().display(),
        "desktop starting"
    );

    let started_by_autostart = has_argument(std::env::args_os(), AUTOSTART_ARG);
    // setup 里的失败只能从这里出去：`StartupDiagnostics` 本身不是 `Send`（见 `FatalReporter`）。
    let fatal = diagnostics.fatal_reporter();
    let log_flush = diagnostics.log_flush();

    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            open_cursor_ca_install_terminal,
            open_log_directory,
        ])
        .plugin(tauri_plugin_single_instance::init(|app, args, _| {
            if !args.iter().any(|arg| arg == AUTOSTART_ARG) {
                let _ = open_main_window(app);
            }
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .on_window_event(|window, event| {
            let WindowEvent::CloseRequested { api, .. } = event else {
                return;
            };
            let explicit_exit = window
                .app_handle()
                .try_state::<DesktopRuntime>()
                .is_some_and(|runtime| runtime.exiting.load(Ordering::Acquire));
            if !close_hides_to_tray(window.label(), explicit_exit) {
                return;
            }
            api.prevent_close();
            if let Err(error) = window.hide() {
                tracing::warn!(%error, "failed to hide the main window to the tray");
            }
        })
        .setup(move |app| {
            finish_setup(
                || setup_desktop(app, started_by_autostart, &log_flush),
                |error| fatal.report_fatal(error),
                &log_flush,
                |code| std::process::exit(code),
            )
        })
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            diagnostics.report_fatal(&error);
            return ExitCode::FAILURE;
        }
    };

    app.run(|app, event| match event {
        // code 为 None 是窗口被非关闭按钮路径销毁时的兜底：托盘与转发服务继续运行。
        // code 为 Some 时是显式退出请求。
        RunEvent::ExitRequested { code, api, .. } => match code {
            None => {
                // 关窗不是退出：进程与本地网关留在托盘里（Cursor 的接管依赖网关在线）。
                tracing::info!("all windows closed; exit prevented, the gateway stays in the tray");
                api.prevent_exit();
            }
            Some(code) => {
                // 真正退出只来自托盘菜单「退出」或应用自己调 exit；记下是哪一种，
                // 用户报"自己退了/没退"时要能从日志回答。
                tracing::info!(code, "explicit exit requested; shutting down the gateway");
                let runtime = app.state::<DesktopRuntime>();
                if !runtime.begin_exit(code) {
                    // 第二次退出请求：收尾还在跑（用户又点了一次托盘退出、或其他组件又调了
                    // exit）。放行会把撤回 Cursor 配置那一步砍掉，留下指向死端口的代理配置
                    // ——和"预算太小被硬结束"是同一类后果，所以继续阻止退出，由收尾那一步
                    // 用第一次的码结束进程。收尾结束时不再调 `app.exit`（见 `finish_process`，
                    // 那条路丢退出码），但这条放行判断保留：它描述的是状态机契约，改回
                    // `app.exit` 时仍然需要它。
                    if !runtime.finishing() {
                        tracing::warn!("exit requested again while shutting down; ignored");
                        api.prevent_exit();
                    }
                    return;
                }
                api.prevent_exit();
                runtime.shutdown.cancel();
                let server = runtime.server.lock().expect("server lock poisoned").take();
                let log_flush = runtime.log_flush.clone();
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(server) = server {
                        // 预算由服务端给出：它盖住"撤回写进用户文件的路由"与"排水"两段。
                        // 超时之后进程就硬结束（`process::exit` 不 drop 任何任务），所以
                        // 这个数比服务端最坏耗时小一截就等于在收尾中途硬结束进程。
                        match tokio::time::timeout(cursor_server::app::SHUTDOWN_BUDGET, server)
                            .await
                        {
                            Ok(Ok(Ok(()))) => {}
                            Ok(Ok(Err(error))) => {
                                tracing::error!(%error, "desktop server shutdown failed")
                            }
                            Ok(Err(error)) => {
                                tracing::error!(%error, "desktop server task failed")
                            }
                            Err(_) => tracing::warn!("desktop server shutdown timed out"),
                        }
                    }
                    // 落盘放在最后：收尾那几行（收尾失败/超时）是排查退出问题的唯一线索，
                    // 而进程马上就要结束，之后写不进去。
                    // `finish_exit()` 交出第一次请求记下的码；结束进程用 `process::exit`
                    // ——`app.exit(code)` 在 tauri-runtime-wry 2.11.4 上会丢码（见
                    // `finish_process`）。
                    finish_process(
                        || app.state::<DesktopRuntime>().finish_exit(),
                        || log_flush.flush(),
                        |code| std::process::exit(code),
                    );
                });
            }
        },
        #[cfg(target_os = "macos")]
        RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => {
            let _ = open_main_window(app);
        }
        _ => {}
    });

    // 不可达：`App::run` 结束时会直接 `std::process::exit`（tauri-2.11.5 app.rs:1346
    // "This function never returns"），这里的 `ExitCode::SUCCESS` 过去是死代码，却让人
    // 以为"退出码 0 是从这里出去的"。真的返回了就说明 tauri 换了收尾方式，那要立刻炸出来。
    unreachable!("tauri exits the process instead of returning from App::run")
}

/// 收尾助手进程的入口：应用已经被强杀，替它撤掉留在 Cursor 里的代理配置。
///
/// "我守的实例"从 `app.lock.owner.json` 读取（`pid` + 启动时刻），不靠自己的 PID 猜：
/// 助手是独立进程，编号与主应用本来就不同。没有窗口、没有托盘，跑完就退。日志照旧写
/// 文件：这条路径出问题时不能完全无声。
fn run_exit_cleanup() -> ExitCode {
    let Ok(_diagnostics) = StartupDiagnostics::initialize() else {
        // 连日志都起不来时不要弹窗：这是个后台助手，没人看着它。
        return ExitCode::FAILURE;
    };
    match tauri::async_runtime::block_on(cursor_server::local_app::cleanup_after_exit()) {
        Ok(true) => {
            tracing::info!("exit cleanup removed the leftover Cursor proxy configuration");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            tracing::info!("exit cleanup had nothing to do");
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::warn!(
                error = %cursor_server::diagnostics::error_chain(&error),
                "exit cleanup failed"
            );
            ExitCode::FAILURE
        }
    }
}

/// 起一个独立进程，在自己被强杀后收拾 Cursor 的代理配置。
///
/// 正常退出走 `CursorHarness::disable()`；`taskkill /F`、崩溃、断电都没有那个机会，
/// 留下的配置会让 Cursor 一直对着一个没人监听的端口报"连不上代理"。助手是同一个可执行
/// 文件加 `--cleanup-after-exit`：Windows 上用 `DETACHED_PROCESS`，不跟着宿主一起被
/// 结束，也不弹控制台窗口。
fn spawn_exit_cleanup_helper() {
    let Ok(executable) = std::env::current_exe() else {
        tracing::warn!("cannot resolve the executable path; the exit cleanup helper is skipped");
        return;
    };
    let mut command = Command::new(executable);
    command
        .arg(EXIT_CLEANUP_ARG)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    match command.spawn() {
        Ok(child) => tracing::info!(pid = child.id(), "exit cleanup helper started"),
        Err(error) => tracing::warn!(
            error = %cursor_server::diagnostics::error_chain(&error),
            "failed to start the exit cleanup helper"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;

    /// setup 失败必须"先让人看见、把日志落盘、再用非零码结束进程"，绝不能把 `Err` 交回
    /// tauri（交回去 = `panic!("Failed to setup app: {e}")`，release 无控制台 → 用户双击后
    /// 什么都看不到）。
    ///
    /// 用例跑的是 `.setup()` 闭包调用的**同一个** `finish_setup`：装配动作、报告出口与结束
    /// 进程都在参数里（生产传 `setup_desktop`、`fatal.report_fatal`、`std::process::exit`），
    /// 装配用真会失败的那一步（数据目录的位置被一个普通文件占着，与 `setup_desktop` 里
    /// `Config::desktop()?` 同一条路径——但不会碰数据库、端口、窗口和托盘）。
    ///
    /// 回退检查：把 `finish_setup` 改回 `setup()?`（旧的 panic 路径）后，`outcome` 会是
    /// `Ok(Err(..))`，本用例在 `expect_err` 上变红。
    #[test]
    fn a_failed_setup_is_reported_and_never_handed_back_to_tauri() {
        let blocked = std::env::temp_dir().join(format!("haxsd-setup-{}", std::process::id()));
        std::fs::create_dir_all(&blocked).expect("fixture directory");
        // 数据目录指向一个普通文件：`managed_data_dir()` 的 `create_dir_all` 必然失败。
        let blocked = blocked.join("data");
        std::fs::write(&blocked, b"not a directory").expect("fixture file");
        let previous = std::env::var_os("HAXSD_BYOK_DATA_DIR");
        std::env::set_var("HAXSD_BYOK_DATA_DIR", &blocked);

        let events = Mutex::new(Vec::new());
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            finish_setup(
                || {
                    Config::desktop()?;
                    Ok(())
                },
                |error| events.lock().unwrap().push(format!("report: {error}")),
                &LogFlush::inert(),
                |code| {
                    events.lock().unwrap().push(format!("exit: {code}"));
                    panic!("the process exit was reached")
                },
            )
        }));

        match previous {
            Some(value) => std::env::set_var("HAXSD_BYOK_DATA_DIR", value),
            None => std::env::remove_var("HAXSD_BYOK_DATA_DIR"),
        }
        std::fs::remove_dir_all(blocked.parent().expect("fixture parent")).ok();

        // 走到结束进程（哨兵 panic）才算对：把 `Err` 交回 tauri 的结局是 tauri 自己 panic。
        let payload = outcome.expect_err("setup 失败绝不能被交回 tauri");
        assert_eq!(
            payload.downcast_ref::<&str>().copied(),
            Some("the process exit was reached"),
            "失败路径必须在报告之后结束进程"
        );
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2, "先报告，再结束进程: {events:?}");
        assert!(
            events[0].starts_with("report: "),
            "启动失败必须先让人看见: {events:?}"
        );
        assert_eq!(
            events[1],
            format!("exit: {SETUP_FAILURE_EXIT_CODE}"),
            "结束进程的退出码不能是 0"
        );
    }

    fn desktop_runtime() -> DesktopRuntime {
        DesktopRuntime {
            shutdown: CancellationToken::new(),
            server: Mutex::new(None),
            exiting: AtomicBool::new(false),
            finishing: AtomicBool::new(false),
            exit_code: AtomicI32::new(0),
            log_flush: Arc::new(LogFlush::inert()),
            server_addr: "127.0.0.1:0".parse().expect("static address"),
        }
    }

    #[test]
    fn closing_the_main_window_hides_it_instead_of_quitting() {
        assert!(close_hides_to_tray(MAIN_WINDOW_LABEL, false));
        assert!(
            !close_hides_to_tray(MAIN_WINDOW_LABEL, true),
            "显式退出不能被关窗逻辑拦住"
        );
        assert!(
            !close_hides_to_tray("other", false),
            "只接管主窗口的关闭按钮"
        );
    }

    /// 退出码由第一次退出请求决定，收尾结束后的第二次事件不能把它改回 0。
    ///
    /// 服务端异常结束是 `app.exit(1)`，托盘菜单是 `app.exit(0)`；收尾那一步以前写死
    /// `app.exit(0)`，于是"服务端崩了"在外部脚本/系统事件日志里与"用户正常退出"一样。
    #[test]
    fn the_first_exit_request_decides_the_process_exit_code() {
        // 服务端异常结束：1 必须留到最后。
        let runtime = desktop_runtime();
        assert!(runtime.begin_exit(1), "第一次退出请求负责收尾");
        assert_eq!(runtime.exit_code(), 1);
        assert!(!runtime.finishing(), "收尾还没跑完");
        // 收尾途中用户又点了一次托盘退出（第二次事件带 0）：不能改写退出码，也不放行
        // （放行会把撤回 Cursor 配置那一步砍掉）。
        assert!(!runtime.begin_exit(0));
        assert_eq!(runtime.exit_code(), 1);
        assert!(!runtime.finishing());
        // 收尾跑完：这一次 `app.exit` 必须放行，用的是第一次的码。
        assert_eq!(runtime.finish_exit(), 1);
        assert!(runtime.finishing());
        assert_eq!(runtime.exit_code(), 1);

        // 托盘菜单的正常退出。
        let runtime = desktop_runtime();
        assert!(runtime.begin_exit(0));
        assert_eq!(runtime.exit_code(), 0);
        assert_eq!(runtime.finish_exit(), 0);
    }

    /// 收尾结束时的顺序与退出码：先把日志落盘、再带着第一次请求记下的码结束进程。
    ///
    /// `app.exit(code)` 在 tauri-runtime-wry 2.11.4 上会丢码（`Message::RequestExit` 只置
    /// `ControlFlow::Exit`），所以生产路径改成 `std::process::exit(code)`；退出在这里注入成
    /// 会 panic 的哨兵，才能断言"先落盘、后用哪个码"这两件事。
    #[test]
    fn the_shutdown_flushes_logs_and_exits_with_the_remembered_code() {
        let runtime = desktop_runtime();
        assert!(runtime.begin_exit(1), "服务端异常结束先到");
        assert!(!runtime.begin_exit(0), "第二次请求不能改写退出码");

        let flushed = std::cell::Cell::new(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            finish_process(
                || runtime.finish_exit(),
                || flushed.set(true),
                |code| panic!("exit code: {code}"),
            );
        }));

        assert!(flushed.get(), "结束进程之前必须先把日志落盘");
        let payload = result.expect_err("finish_process 必须结束进程");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .unwrap_or_default();
        assert!(
            message.contains("exit code: 1"),
            "必须用第一次请求记下的码退出，实际: {message}"
        );
    }

    /// 命令行参数不是合法 UTF-8 时不能 panic。
    ///
    /// `std::env::args()` 遇到这种参数（Windows 上是不成对的代理项）会在迭代时 panic，而
    /// 这一行在 `StartupDiagnostics::initialize()` 之前：release 没有控制台，应用就"毫无
    /// 痕迹地不启动"。收尾助手与自启两个开关都走 `args_os()` 比较。
    #[test]
    fn a_non_unicode_argument_does_not_stop_the_startup() {
        #[cfg(windows)]
        let broken = {
            use std::os::windows::ffi::OsStringExt;
            // 单独一个高位代理项：Windows 上合法的 OsString，但不是合法 UTF-8。
            OsString::from_wide(&[0xD800])
        };
        #[cfg(not(windows))]
        let broken = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(vec![0xFF, 0xFE])
        };

        assert!(!has_argument([broken], EXIT_CLEANUP_ARG));
        assert!(!has_argument([OsString::from("--other")], EXIT_CLEANUP_ARG));
        assert!(has_argument(
            [OsString::from("--other"), OsString::from(EXIT_CLEANUP_ARG)],
            EXIT_CLEANUP_ARG
        ));
        assert!(has_argument([OsString::from(AUTOSTART_ARG)], AUTOSTART_ARG));
    }
}
