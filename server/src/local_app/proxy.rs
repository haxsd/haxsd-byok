//! Configures the local application proxy.
use std::{net::SocketAddr, sync::Arc, time::Duration};

use hudsucker::{
    certificate_authority::RcgenAuthority,
    hyper::{Request, Response, StatusCode, Uri},
    rustls::crypto::aws_lc_rs,
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle, time::Instant};

use parking_lot::RwLock;

use crate::{
    api::cursor::proxy::UPSTREAM_URL_HEADER, cursor::services::tab::is_tab_path, store::TabMode,
    Error, Result,
};

use super::ca::LoadedCa;

#[derive(Default)]
pub struct ProxyRuntime {
    url: Option<String>,
    port: Option<u16>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl ProxyRuntime {
    pub fn running(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }
    pub fn url(&self) -> Option<String> {
        self.running().then(|| self.url.clone()).flatten()
    }
    pub fn port(&self) -> Option<u16> {
        if self.running() {
            self.port
        } else {
            None
        }
    }

    pub async fn start(
        &mut self,
        backend: SocketAddr,
        ca: LoadedCa,
        requested_port: u16,
        tab_mode: Arc<RwLock<TabMode>>,
    ) -> Result<(String, u16)> {
        if let Some(url) = self.url() {
            return Ok((url, self.port.unwrap_or_default()));
        }
        let listener = bind_proxy_listener(requested_port).await?;
        let address = listener.local_addr()?;
        let (stop, done) = oneshot::channel();
        let authority = RcgenAuthority::new(ca.issuer, 1_000, aws_lc_rs::default_provider());
        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(authority)
            .with_rustls_connector(aws_lc_rs::default_provider())
            .with_http_handler(CursorRelay { backend, tab_mode })
            .with_graceful_shutdown(async move {
                let _ = done.await;
            })
            .build()
            .map_err(|error| Error::Store(format!("build Cursor proxy: {error}")))?;
        self.stop = Some(stop);
        self.url = Some(format!("http://{address}"));
        self.port = Some(address.port());
        tracing::info!(
            requested_port,
            address = %address,
            fallback = address.port() != requested_port,
            "Cursor proxy listening"
        );
        self.task = Some(tokio::spawn(async move {
            if let Err(error) = proxy.start().await {
                let error = error.to_string();
                tracing::error!(%error, "Cursor proxy stopped unexpectedly");
                // 代理自己死了：Cursor 之后每个请求都会失败，而它的 settings.json 还
                // 指着这个端口。先把自己写的代理配置撤掉，让 Cursor 回到直连（还能用），
                // 再把现场抓下来。
                if std::env::var_os("HAXSD_BYOK_DATA_DIR").is_none() {
                    if let Err(error) = super::settings::clear_proxy_settings() {
                        tracing::warn!(
                            error = %crate::diagnostics::error_chain(&error),
                            "failed to clear Cursor proxy configuration after the proxy stopped"
                        );
                    }
                }
                crate::diagnostics::capture(
                    "cursor_proxy_stopped",
                    serde_json::json!({ "address": address.to_string(), "error": error }),
                );
            }
        }));
        Ok((self.url.clone().unwrap(), address.port()))
    }

    /// 停代理；返回是否**优雅**结束（`false` = 宽限期内没结束、被 `abort` 掉了）。
    ///
    /// `JoinHandle` 被 drop 是 **detach**，不是取消。旧实现「`timeout` 超时 → 丢掉句柄 →
    /// 清空 `url/port`」的后果是：任务还绑着监听端口在排空在途连接，而 `running()` 已经变
    /// `false`、集成状态报 `Disabled`。下一次接管启动会发现配置端口被"上一次的我们"占着，
    /// 重试 5 秒后仍失败 → 退到随机端口 → 把随机端口写回用户的端口设置（`set_proxy_port`），
    /// 用户设置里的端口被无声改掉。
    ///
    /// 现在超时后显式 `abort()` 再 `await`：监听端口随任务里的 listener 一起释放，
    /// `stop()` 返回时"已停止"一定是事实。宁可多花一次 abort，也不谎报状态。
    pub async fn stop(&mut self) -> bool {
        let port = self.port;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let mut graceful = true;
        if let Some(mut task) = self.task.take() {
            if tokio::time::timeout(STOP_GRACE, &mut task).await.is_err() {
                // 在途连接还没收尾（上游超时是小时级的，这是常态而不是异常）。
                tracing::warn!(
                    ?port,
                    "Cursor proxy still has requests in flight; aborting it to release the port"
                );
                task.abort();
                let _ = task.await;
                graceful = false;
            }
        }
        self.url = None;
        self.port = None;
        if port.is_some() {
            tracing::info!(?port, graceful, "Cursor proxy stopped");
        }
        graceful
    }
}

/// 优雅关停的宽限期：超时之后 `abort()`，保证端口真的被释放。
const STOP_GRACE: Duration = Duration::from_secs(5);

/// 配置端口被占用时的重试窗口。
///
/// 占用者多半就是"上一次的我们"：进程正在退出、监听端口还没释放。立刻退到随机
/// 端口会把 Cursor 指向一个马上要消失的地址（实测会连续报连接错误），
/// 所以先在这个窗口里重试；实在拿不到才退到随机端口。
const PORT_RETRY_WINDOW: Duration = Duration::from_secs(5);
const PORT_RETRY_INTERVAL: Duration = Duration::from_millis(250);

async fn bind_proxy_listener(requested_port: u16) -> Result<TcpListener> {
    bind_proxy_listener_with(requested_port, PORT_RETRY_WINDOW).await
}

async fn bind_proxy_listener_with(requested_port: u16, window: Duration) -> Result<TcpListener> {
    let requested = SocketAddr::from(([127, 0, 0, 1], requested_port));
    if requested_port == 0 {
        return Ok(TcpListener::bind(requested).await?);
    }
    let deadline = Instant::now() + window;
    let mut last_error = None;
    loop {
        match TcpListener::bind(requested).await {
            Ok(listener) => {
                if last_error.is_some() {
                    tracing::info!(%requested, "configured proxy port became available again");
                }
                return Ok(listener);
            }
            Err(error) => last_error = Some(error),
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(PORT_RETRY_INTERVAL).await;
    }
    let error = last_error.expect("the configured port was attempted at least once");
    tracing::warn!(
        %requested,
        %error,
        window_ms = window.as_millis() as u64,
        "configured proxy port is still taken after retrying; selecting a random port"
    );
    Ok(TcpListener::bind("127.0.0.1:0").await?)
}

#[derive(Clone)]
struct CursorRelay {
    backend: SocketAddr,
    tab_mode: Arc<RwLock<TabMode>>,
}

impl CursorRelay {
    /// `handle_request` 的全部逻辑，和 hudsucker 的 `HttpContext` 解耦：入口判据要能用例直接
    /// 驱动（`HttpContext` 是 `#[non_exhaustive]`，外部构造不出来）。
    fn route(&mut self, mut request: Request<Body>) -> RequestOrResponse {
        let original = request.uri().clone();
        let locally_routed = should_route_locally(original.path(), *self.tab_mode.read());
        if is_cursor_host(original.host().unwrap_or_default()) && locally_routed {
            // 本机入口只服务 Cursor 客户端本身：匿名调用不进后端（判据与后果见
            // `local_app::local_entry_caller`）。
            let Some(caller) = super::local_entry_caller(request.headers()) else {
                tracing::warn!(
                    path = original.path(),
                    "rejected an anonymous request to a locally served Cursor route"
                );
                return unauthorized_response().into();
            };
            tracing::debug!(path = original.path(), ?caller, "local Cursor route");
            if let Ok(value) = original.to_string().parse() {
                request.headers_mut().insert(UPSTREAM_URL_HEADER, value);
            }
            let path = original
                .path_and_query()
                .map(|value| value.as_str())
                .unwrap_or("/");
            if let Ok(uri) = format!("http://{}{}", self.backend, path).parse::<Uri>() {
                *request.uri_mut() = uri;
            }
        }
        request.into()
    }
}

/// 匿名调用本机入口时的答复：说清这个入口只服务 Cursor 客户端。
fn unauthorized_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(
            hudsucker::hyper::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(Body::from(
            "the local BYOK gateway only serves the Cursor client; this request carries no caller \
             identity\n",
        ))
        .expect("the static unauthorized response is well formed")
}

impl HttpHandler for CursorRelay {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        request: Request<Body>,
    ) -> RequestOrResponse {
        self.route(request)
    }

    async fn should_intercept_connect(
        &mut self,
        _ctx: &HttpContext,
        request: &Request<Body>,
    ) -> bool {
        request
            .uri()
            .authority()
            .is_some_and(|authority| is_cursor_host(authority.host()))
    }

    async fn should_intercept_tls(
        &mut self,
        _ctx: &HttpContext,
        hello: hudsucker::rustls::server::ClientHello<'_>,
    ) -> bool {
        hello.server_name().is_some_and(is_cursor_host)
    }
}

pub fn is_cursor_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    matches!(host.as_str(), "api2.cursor.sh" | "api3.cursor.sh") || host.ends_with(".cursor.sh")
}

fn is_local_path(path: &str) -> bool {
    matches!(
        path,
        "/agent.v1.AgentService/RunSSE"
            | "/aiserver.v1.BidiService/BidiAppend"
            | "/aiserver.v1.AiService/AvailableDocs"
            | "/aiserver.v1.DashboardService/GetEffectiveUserPlugins"
            | "/aiserver.v1.DashboardService/GetUserPrivacyMode"
            | "/agent.v1.AgentService/UpdateConversationMetadata"
            | "/aiserver.v1.AiService/GetServerConfig"
            | "/aiserver.v1.ServerConfigService/GetServerConfig"
            | "/aiserver.v1.AiService/AvailableModels"
            | "/agent.v1.AgentService/GetUsableModels"
            | "/aiserver.v1.AiService/GetUsableModels"
            | "/agent.v1.AgentService/GetDefaultModelForCli"
            | "/aiserver.v1.AiService/GetDefaultModelForCli"
            | "/aiserver.v1.AiService/GetDefaultModel"
            | "/aiserver.v1.AiService/GetDefaultModelNudgeData"
            | "/aiserver.v1.AuthService/GetEmail"
            | "/aiserver.v1.AuthService/GetUserMeta"
            | "/aiserver.v1.DashboardService/GetMe"
            | "/aiserver.v1.DashboardService/GetTeams"
            | "/aiserver.v1.DashboardService/GetUserProfile"
            | "/aiserver.v1.DashboardService/GetCurrentPeriodUsage"
            | "/aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants"
            | "/aiserver.v1.AiService/KnowledgeBaseAdd"
            | "/aiserver.v1.AiService/KnowledgeBaseList"
            | "/aiserver.v1.AiService/KnowledgeBaseUpdate"
            | "/aiserver.v1.AiService/KnowledgeBaseRemove"
            | "/aiserver.v1.AiService/WriteGitCommitMessage"
            | "/aiserver.v1.NetworkService/IsConnected"
            // Cursor 的控制面请求：它们由后端按 Cursor 角色的出网设置转发（用户直连
            // 模型时仍走系统代理），避免 hudsucker 直连失败把 502 抛给会话。
            | "/aiserver.v1.AnalyticsService/SubmitLogs"
            | "/aiserver.v1.AnalyticsService/Batch"
            | "/aiserver.v1.DashboardService/GetCliDownloadUrl"
            | "/aiserver.v1.DashboardService/GetTeamAdminSettingsOrNotInTeam"
            | "/aiserver.v1.DashboardService/GetManagedSkills"
            | "/aiserver.v1.BackgroundComposerService/GetBackgroundComposerUserSettings"
            | "/aiserver.v1.RepositoryService/FastRepoInitHandshakeV2"
            | "/aiserver.v1.AiService/ServerTime"
            | "/aiserver.v1.AnalyticsService/BootstrapStatsig"
            | "/auth/full_stripe_profile"
            | "/auth/stripe_profile"
    )
}

fn should_route_locally(path: &str, tab_mode: TabMode) -> bool {
    is_local_path(path) || (is_tab_path(path) && tab_mode != TabMode::Direct)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_with(task: JoinHandle<()>) -> ProxyRuntime {
        ProxyRuntime {
            url: Some("http://127.0.0.1:1634".into()),
            port: Some(1634),
            stop: None,
            task: Some(task),
        }
    }

    fn cursor_relay(tab_mode: TabMode) -> CursorRelay {
        CursorRelay {
            backend: "127.0.0.1:3000".parse().unwrap(),
            tab_mode: Arc::new(RwLock::new(tab_mode)),
        }
    }

    fn cursor_request(path: &str, authorization: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri(format!("https://api2.cursor.sh{path}"));
        if let Some(value) = authorization {
            builder = builder.header(hudsucker::hyper::header::AUTHORIZATION, value);
        }
        builder.body(Body::empty()).unwrap()
    }

    /// 本机入口只服务 Cursor 客户端：匿名调用被就地拒绝，不进后端。
    #[test]
    fn the_local_entry_rejects_an_anonymous_caller() {
        let rejected = cursor_relay(TabMode::Direct).route(cursor_request(
            "/aiserver.v1.AiService/AvailableModels",
            None,
        ));
        let RequestOrResponse::Response(response) = rejected else {
            panic!("匿名调用必须被拒绝，而不是转发给后端");
        };
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        // 空 Bearer 与别的鉴权方式同样算匿名。
        for headers in [Some("Bearer "), Some("Basic dXNlcjpwYXNz"), Some("token x")] {
            assert!(
                matches!(
                    cursor_relay(TabMode::Direct).route(cursor_request(
                        "/aiserver.v1.AiService/AvailableModels",
                        headers
                    )),
                    RequestOrResponse::Response(_)
                ),
                "{headers:?} 不是客户端身份"
            );
        }
    }

    /// 认出的两种调用方都放行：我们注入的本地账号 token，以及混合模式下客户端自己的账号。
    #[test]
    fn the_local_entry_accepts_the_cursor_client() {
        for authorization in [
            // 本地模式：`inject_if_missing` 写进 state.vscdb 的那一份。
            crate::local_app::local_cursor_authorization(),
            // 混合模式：token 由 Cursor 自己签发，本地验证不了，只确认带着凭证。
            "Bearer official-cursor-account-token".into(),
        ] {
            let routed = cursor_relay(TabMode::Direct).route(cursor_request(
                "/aiserver.v1.AiService/AvailableModels",
                Some(&authorization),
            ));
            let RequestOrResponse::Request(request) = routed else {
                panic!("客户端自己的请求必须被转发到后端: {authorization}");
            };
            // 确实是"转发到本机后端"的那条路：URI 指向 backend，并带上原始地址。
            assert_eq!(
                request.uri().to_string(),
                "http://127.0.0.1:3000/aiserver.v1.AiService/AvailableModels"
            );
            assert!(request.headers().contains_key(UPSTREAM_URL_HEADER));
        }
    }

    /// 不归本地管的请求（官方上游路径）一根手指都不碰：匿名也不拦。
    #[test]
    fn upstream_traffic_is_left_alone() {
        let routed = cursor_relay(TabMode::Direct)
            .route(cursor_request("/aiserver.v1.AiService/StreamChat", None));
        let RequestOrResponse::Request(request) = routed else {
            panic!("官方上游路径不该被本地入口拦下");
        };
        assert_eq!(
            request.uri().to_string(),
            "https://api2.cursor.sh/aiserver.v1.AiService/StreamChat"
        );
    }

    /// 宽限期内停不下来的任务必须被真的取消，而不是被 detach 掉继续占着端口。
    ///
    /// 旧实现 `timeout(...)` 丢掉的是返回值，句柄一 drop 任务就 detach 了：`running()` 变
    /// `false`、`url/port` 清空，而那个任务还绑着监听端口——状态与事实相反。
    #[tokio::test(start_paused = true)]
    async fn a_task_that_will_not_stop_gracefully_is_aborted() {
        let mut proxy = runtime_with(tokio::spawn(std::future::pending::<()>()));
        // 暂停时钟下 `timeout` 会立刻到点：等价于"有在途长连接，5 秒内没停完"。
        let graceful = proxy.stop().await;

        assert!(!graceful, "被迫取消时要如实报告，不能声称优雅结束");
        assert!(!proxy.running(), "返回之后就真的没在跑了");
        assert_eq!(proxy.url(), None);
        assert_eq!(proxy.port(), None);
    }

    /// 能正常结束的任务按优雅路径走。
    #[tokio::test]
    async fn a_finished_task_reports_a_graceful_stop() {
        let mut proxy = runtime_with(tokio::spawn(async {}));
        assert!(proxy.stop().await);
        assert!(!proxy.running());
        assert_eq!(proxy.port(), None);
    }

    #[test]
    fn cursor_cli_transport_and_model_metadata_routes_stay_local() {
        for path in [
            "/aiserver.v1.AiService/GetServerConfig",
            "/aiserver.v1.ServerConfigService/GetServerConfig",
            "/agent.v1.AgentService/GetDefaultModelForCli",
            "/aiserver.v1.AiService/GetDefaultModelForCli",
            "/aiserver.v1.AiService/GetDefaultModel",
            "/aiserver.v1.AiService/GetDefaultModelNudgeData",
            "/aiserver.v1.AiService/AvailableDocs",
            "/aiserver.v1.DashboardService/GetEffectiveUserPlugins",
            "/aiserver.v1.DashboardService/GetUserPrivacyMode",
            "/aiserver.v1.AuthService/GetUserMeta",
            "/agent.v1.AgentService/UpdateConversationMetadata",
            "/aiserver.v1.AnalyticsService/SubmitLogs",
            "/aiserver.v1.AnalyticsService/Batch",
            "/aiserver.v1.DashboardService/GetCliDownloadUrl",
            "/aiserver.v1.DashboardService/GetTeamAdminSettingsOrNotInTeam",
            "/aiserver.v1.DashboardService/GetManagedSkills",
            "/aiserver.v1.BackgroundComposerService/GetBackgroundComposerUserSettings",
            "/aiserver.v1.RepositoryService/FastRepoInitHandshakeV2",
            "/aiserver.v1.AiService/ServerTime",
            "/auth/full_stripe_profile",
            "/auth/stripe_profile",
        ] {
            assert!(is_local_path(path), "{path} must not reach Cursor upstream");
        }
    }

    /// 端口被"上一次的我们"占着时，进程退出会释放监听；重试窗口内应当拿回来，
    /// 而不是立刻退到随机端口、把 Cursor 指向一个马上消失的地址。
    #[tokio::test]
    async fn a_port_released_during_the_retry_window_is_taken_over() {
        let window = Duration::from_millis(600);
        let occupant = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = occupant.local_addr().unwrap().port();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(occupant);
        });

        let listener = bind_proxy_listener_with(port, window).await.unwrap();

        assert_eq!(listener.local_addr().unwrap().port(), port);
        release.await.unwrap();
    }

    /// 拿不到配置端口时必须仍能起来（退到随机端口），而不是把整个应用钉死在启动失败。
    #[tokio::test]
    async fn a_port_held_for_the_whole_window_falls_back_to_a_random_port() {
        let window = Duration::from_millis(50);
        let occupant = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = occupant.local_addr().unwrap().port();

        let listener = bind_proxy_listener_with(port, window).await.unwrap();

        assert_ne!(listener.local_addr().unwrap().port(), port);
    }
}
