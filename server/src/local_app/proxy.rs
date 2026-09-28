//! Configures the local application proxy.
use std::{net::SocketAddr, sync::Arc, time::Duration};

use hudsucker::{
    certificate_authority::RcgenAuthority,
    hyper::{Request, Uri},
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

    pub async fn stop(&mut self) {
        let port = self.port;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
        self.url = None;
        self.port = None;
        if port.is_some() {
            tracing::info!(?port, "Cursor proxy stopped");
        }
    }
}

/// 配置端口被占用时的重试窗口。
///
/// 占用者多半就是"上一次的我们"：进程正在退出、监听端口还没释放。立刻退到随机
/// 端口会把 Cursor 指向一个马上要消失的地址（HANDOFF 记过 17:24–17:26 那批报错），
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

impl HttpHandler for CursorRelay {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        mut request: Request<Body>,
    ) -> RequestOrResponse {
        let original = request.uri().clone();
        let locally_routed = should_route_locally(original.path(), *self.tab_mode.read());
        if is_cursor_host(original.host().unwrap_or_default()) && locally_routed {
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
