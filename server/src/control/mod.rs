//! Exposes the local control API.
mod app_info;
mod calls;
pub mod codex;
mod devin;
mod harness;
mod models;
mod overview;
mod plugins;
mod ports;
mod service;
mod settings;

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, header::CONTENT_TYPE, HeaderValue, Method, Request, Response, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{any, get, post, put},
    Router,
};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    services::ServeDir,
};
use url::{Host, Url};

use crate::devin::gateway::DevinListening;

pub use service::{
    CallDetail, CallSummary, ControlService, DevinStatus, DiscoveredModels,
    LegacyModelImportPreview, LegacyModelImportResult, ModelConnectivityResult,
    ModelDiscoveryInput, ObservabilitySettings,
};

pub fn web_router(service: ControlService, assets: impl AsRef<std::path::Path>) -> Router {
    Router::new()
        .nest_service(
            "/__byok-api__",
            ServeDir::new(assets).append_index_html_on_directories(true),
        )
        .merge(api_router(service))
}

pub fn proxy_web_router(service: ControlService, target: Url) -> Router {
    frontend_proxy_router(target).merge(api_router(service))
}

/// 开发期前端反代的请求体上限。
///
/// 只为挡住"把开发代理当成上传通道"这一种情况；超限的错误文案要说清这是开发代理，
/// 免得用户以为是产品行为。
const FRONTEND_PROXY_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// 开发期把前端路径转发给 Vite（`CURSOR_CONSOLE_PROXY` / `ConsoleSource::Proxy`）。
///
/// **仅开发用**：目标地址来自环境变量，请求头（含 `authorization` / `cookie`）原样转发，
/// 所以别把它指向不受信任的地址。响应侧按上游的分片流式转发，不再整段读进内存。
fn frontend_proxy_router(target: Url) -> Router {
    let state = FrontendProxy {
        client: reqwest::Client::new(),
        target: target.as_str().trim_end_matches('/').to_string(),
    };
    Router::new()
        .route("/__byok-api__/", any(proxy_frontend))
        .route("/__byok-api__/{*path}", any(proxy_frontend))
        .with_state(state)
}

#[derive(Clone)]
struct FrontendProxy {
    client: reqwest::Client,
    target: String,
}

async fn proxy_frontend(
    State(proxy): State<FrontendProxy>,
    request: Request<Body>,
) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/__byok-api__/");
    let mut upstream = proxy
        .client
        .request(parts.method, format!("{}{path}", proxy.target));
    for (name, value) in &parts.headers {
        if name != header::HOST && name != header::CONNECTION {
            upstream = upstream.header(name, value);
        }
    }
    let body = match to_bytes(body, FRONTEND_PROXY_BODY_LIMIT).await {
        Ok(body) => body,
        Err(error) => {
            return proxy_error(format_args!(
                "development proxy request body exceeds {} bytes: {error}",
                FRONTEND_PROXY_BODY_LIMIT
            ))
        }
    };
    let upstream = match upstream.body(body).send().await {
        Ok(response) => response,
        Err(error) => return proxy_error(error),
    };
    let status = upstream.status();
    let headers = upstream.headers().clone();
    // 响应体流式转发：开发期的资源（大 bundle、chunked 的 HMR 流）不再先整段落进
    // 服务端内存——上游分片到一段就往下传一段。少了 `Content-Length` 由 hyper 用
    // chunked 收尾。
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    for (name, value) in &headers {
        if name != header::CONNECTION
            && name != header::TRANSFER_ENCODING
            && name != header::CONTENT_LENGTH
        {
            // 同名多值头（多个 `Set-Cookie` 最典型）必须逐条 append：insert 只留最后一条。
            response.headers_mut().append(name, value.clone());
        }
    }
    response
}

fn proxy_error(error: impl std::fmt::Display) -> Response<Body> {
    tracing::warn!(%error, "frontend development proxy failed");
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .body(Body::from("frontend development server is unavailable"))
        .expect("static proxy error response")
}

pub fn api_router(service: ControlService) -> Router {
    Router::new()
        .route("/__byok-api__/api/app-info", get(app_info::get))
        .route(
            "/__byok-api__/api/models",
            get(models::list).post(models::create),
        )
        .route(
            "/__byok-api__/api/models/{model_hash}/duplicate",
            post(models::duplicate),
        )
        .route("/__byok-api__/api/models/discover", post(models::discover))
        .route(
            "/__byok-api__/api/models/import-v0049",
            get(models::preview_v0049).post(models::import_v0049),
        )
        .route("/__byok-api__/api/models/order", put(models::reorder))
        .route("/__byok-api__/api/overview", get(overview::get))
        .route(
            "/__byok-api__/api/models/{model_hash}",
            put(models::update).delete(models::remove),
        )
        .route(
            "/__byok-api__/api/models/{model_hash}/test/{test_id}",
            post(models::test).delete(models::cancel),
        )
        .route("/__byok-api__/api/llm-calls", get(calls::list))
        .route("/__byok-api__/api/llm-calls/{call_id}", get(calls::detail))
        .route("/__byok-api__/api/plugins", get(plugins::list))
        .route(
            "/__byok-api__/api/plugins/runtime",
            get(plugins::runtime_status)
                .post(plugins::initialize_runtime)
                .delete(plugins::cancel_runtime_initialization),
        )
        .route(
            "/__byok-api__/api/plugins/oauth/{session_id}/poll",
            post(plugins::oauth_poll),
        )
        .route(
            "/__byok-api__/api/plugins/{plugin_id}",
            axum::routing::delete(plugins::remove),
        )
        .route(
            "/__byok-api__/api/plugins/{plugin_id}/resources/{resource_type}/add/{method_id}/begin",
            post(plugins::oauth_begin),
        )
        .route(
            "/__byok-api__/api/plugins/{plugin_id}/resources/{resource_type}/import",
            post(plugins::import),
        )
        .route(
            "/__byok-api__/api/plugins/{plugin_id}/resources/{resource_type}/export",
            get(plugins::export_resources),
        )
        .route(
            "/__byok-api__/api/plugins/{plugin_id}/resources/{resource_type}/{resource_id}",
            axum::routing::delete(plugins::delete_resource),
        )
        .route(
            "/__byok-api__/api/plugins/{plugin_id}/resources/{resource_type}/{resource_id}/actions/{action_id}",
            post(plugins::action),
        )
        .route(
            "/__byok-api__/api/plugins/{plugin_id}/resources/{resource_type}/{resource_id}/refresh",
            post(plugins::refresh_resource),
        )
        .route(
            "/__byok-api__/api/plugins/{plugin_id}/providers/{provider_id}/models/sync",
            post(plugins::sync_models),
        )
        .route(
            "/__byok-api__/api/plugins/{plugin_id}/providers/{provider_id}/models/enabled",
            put(plugins::set_model_enabled),
        )
        .route(
            "/__byok-api__/api/settings/observability",
            get(settings::get).put(settings::update),
        )
        .route(
            "/__byok-api__/api/settings/ports",
            get(settings::get_ports).put(settings::update_ports),
        )
        .route(
            "/__byok-api__/api/settings/storage/statistics",
            get(settings::get_storage).delete(settings::clear_storage),
        )
        .route(
            "/__byok-api__/api/settings/proxy",
            get(settings::get_proxy).put(settings::update_proxy),
        )
        .route(
            "/__byok-api__/api/settings/outbound",
            get(settings::get_outbound),
        )
        .route(
            "/__byok-api__/api/settings/tab",
            get(settings::get_tab).put(settings::update_tab),
        )
        .route(
            "/__byok-api__/api/settings/desktop",
            get(settings::get_desktop).put(settings::update_desktop),
        )
        .route(
            "/__byok-api__/api/settings/commit",
            get(settings::get_commit).put(settings::update_commit),
        )
        .route(
            "/__byok-api__/api/settings/compaction",
            get(settings::get_compaction).put(settings::update_compaction),
        )
        .route(
            "/__byok-api__/api/settings/pricing",
            get(settings::get_pricing_settings),
        )
        .route(
            "/__byok-api__/api/settings/pricing/{model_hash}",
            put(settings::update_model_pricing).delete(settings::delete_model_pricing),
        )
        .route(
            "/__byok-api__/api/devin/settings",
            get(devin::get).put(devin::update),
        )
        .route(
            "/__byok-api__/api/harness/cursor/status",
            get(harness::status),
        )
        .route(
            "/__byok-api__/api/harness/cursor/ca/initialize",
            post(harness::initialize_ca),
        )
        .route(
            "/__byok-api__/api/harness/cursor/mode",
            put(harness::set_mode),
        )
        .route(
            "/__byok-api__/api/harness/devin/host/status",
            get(devin::host_status),
        )
        .route(
            "/__byok-api__/api/harness/devin/model-uids",
            get(devin::model_uids),
        )
        .route(
            "/__byok-api__/api/harness/devin/host/apply",
            post(devin::host_apply),
        )
        .route(
            "/__byok-api__/api/harness/devin/host/restore",
            post(devin::host_restore),
        )
        .with_state(service)
        .layer(desktop_cors())
        .layer(middleware::from_fn(reject_cross_site))
}

/// Whether Devin can already work, answered by the server because the gateway
/// ports belong to another origin and cannot be probed from the page.
pub fn devin_status_router(service: ControlService, listening: DevinListening) -> Router {
    Router::new()
        .route("/__byok-api__/api/devin/status", get(devin::status))
        .with_state(Arc::new(devin::DevinStatusState { service, listening }))
        .layer(desktop_cors())
        .layer(middleware::from_fn(reject_cross_site))
}

/// 管理面的来源准入。CORS 只在**响应**上把关:跨站页面发一个"简单请求"(表单 POST、
/// 无 body)时请求照样会执行,只是读不到响应——对 `ca/initialize`、`host/apply` 这类
/// 有副作用的接口,这等于任意网页都能触发本机管理动作。所以进 handler 之前就先拒掉
/// 非本机来源的浏览器请求。不带 Origin / Sec-Fetch-Site 的是本机其它客户端(curl、
/// 桌面壳的原生调用),照常放行。
async fn reject_cross_site(request: Request<Body>, next: Next) -> Response<Body> {
    if cross_site_request(request.headers()) {
        return (StatusCode::FORBIDDEN, "cross-site request rejected").into_response();
    }
    next.run(request).await
}

fn cross_site_request(headers: &axum::http::HeaderMap) -> bool {
    if headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("cross-site"))
    {
        return true;
    }
    // 带 Origin 但不是本机页面:跨站表单、跨站 fetch、被沙箱化的 iframe(`Origin: null`)
    // 都属于这种。
    headers
        .get(header::ORIGIN)
        .is_some_and(|origin| !local_origin(origin))
}

fn desktop_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, request| {
            local_origin(origin) && same_service_origin(origin, request.headers.get(header::HOST))
        }))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([
            CONTENT_TYPE,
            header::ACCEPT_LANGUAGE,
            header::HeaderName::from_static("disable-ad-ids"),
        ])
}

/// 管理页面和 API 由同一 HTTP 服务提供;其它本地端口也不属于这个来源。
fn same_service_origin(origin: &HeaderValue, host: Option<&HeaderValue>) -> bool {
    let Some(host) = host.and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let Some(origin) = origin
        .to_str()
        .ok()
        .and_then(|value| Url::parse(value).ok())
    else {
        return false;
    };
    let Ok(service) = Url::parse(&format!("http://{host}")) else {
        return false;
    };
    service.username().is_empty()
        && service.password().is_none()
        && service.path() == "/"
        && service.query().is_none()
        && service.fragment().is_none()
        && origin.origin() == service.origin()
}

fn local_origin(origin: &HeaderValue) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if origin.eq_ignore_ascii_case("tauri://localhost") {
        return true;
    }
    let Ok(origin) = Url::parse(origin) else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return false;
    }
    match origin.host() {
        Some(Host::Domain(host)) => {
            host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case("tauri.localhost")
        }
        // 前端固定由 `http://127.0.0.1:<端口>` 提供，所以只有回环地址是本机的页面。
        // 私网与链路本地地址不是：这个 API 没有鉴权，放行它们等于让同局域网里托管的
        // 页面也能调用本机的管理接口。
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tower::ServiceExt;

    /// 跨站页面(或沙箱 iframe)的请求必须在进 handler 之前被拒:否则一个表单 POST 就能
    /// 触发本机管理动作,CORS 拦不住副作用。
    #[tokio::test]
    async fn cross_site_requests_are_refused_before_the_handler_runs() {
        let router = Router::new()
            .route("/write", post(|| async { "done" }))
            .layer(middleware::from_fn(reject_cross_site));

        for (name, value) in [
            ("origin", "https://evil.example"),
            ("origin", "null"),
            ("origin", "http://192.168.1.10"),
            ("sec-fetch-site", "cross-site"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/write")
                        .header(name, value)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{name}: {value} 不是本机来源"
            );
        }
    }

    /// 本机页面(curl 式无来源请求、`tauri://localhost`、动态端口的回环页面)照常放行。
    #[tokio::test]
    async fn local_callers_keep_management_access() {
        let router = Router::new()
            .route("/write", post(|| async { "done" }))
            .layer(middleware::from_fn(reject_cross_site));

        let without_origin = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/write")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(without_origin.status(), StatusCode::OK);

        for value in ["tauri://localhost", "http://127.0.0.1:49234"] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/write")
                        .header(header::ORIGIN, value)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{value} 是本机来源");
        }
    }

    #[tokio::test]
    async fn another_loopback_origin_cannot_read_credentials_from_this_service() {
        let response = Router::new()
            .route("/credentials", get(|| async { "fixture-only" }))
            .layer(desktop_cors())
            .oneshot(
                Request::builder()
                    .uri("/credentials")
                    .header(header::HOST, "127.0.0.1:3000")
                    .header(header::ORIGIN, "http://127.0.0.1:9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    #[tokio::test]
    async fn current_origin_with_a_dynamic_port_keeps_management_access() {
        let response = Router::new()
            .route("/credentials", get(|| async { "fixture-only" }))
            .layer(desktop_cors())
            .oneshot(
                Request::builder()
                    .uri("/credentials")
                    .header(header::HOST, "127.0.0.1:49234")
                    .header(header::ORIGIN, "http://127.0.0.1:49234")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "http://127.0.0.1:49234"
        );
    }

    #[tokio::test]
    async fn browser_origins_must_match_the_service_host_and_scheme() {
        for (host, origin) in [
            ("127.0.0.1:3000", "https://127.0.0.1:3000"),
            ("127.0.0.1:3000", "http://localhost:3000"),
            ("127.0.0.1:3000", "http://[::1]:3000"),
            ("127.0.0.1:3000/path", "http://127.0.0.1:3000"),
            ("user@127.0.0.1:3000", "http://127.0.0.1:3000"),
            ("attacker.example:3000", "http://127.0.0.1:3000"),
        ] {
            let response = Router::new()
                .route("/credentials", get(|| async { "fixture-only" }))
                .layer(desktop_cors())
                .oneshot(
                    Request::builder()
                        .uri("/credentials")
                        .header(header::HOST, host)
                        .header(header::ORIGIN, origin)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                response
                    .headers()
                    .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .is_none(),
                "unexpected permission for origin {origin} against host {host}"
            );
        }
    }

    fn origin(value: &'static str) -> HeaderValue {
        HeaderValue::from_static(value)
    }

    /// 开发反代的端到端夹具：一个真的监听回环端口的"Vite"，带两条路由——
    /// `/__byok-api__/cookies` 回两个同名 `Set-Cookie`，`/__byok-api__/stream`
    /// 先发第一段、等测试放行、再发第二段。
    async fn frontend_proxy_fixture() -> (Url, tokio::sync::oneshot::Sender<()>) {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        // handler 必须是 `Fn`：把接收端放进共享槽位，每次调用取一次（测试只会请求一次）。
        let released = std::sync::Arc::new(parking_lot::Mutex::new(Some(released)));
        let app = Router::new()
            .route(
                "/__byok-api__/cookies",
                get(|| async {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::SET_COOKIE, "first=1; Path=/")
                        .header(header::SET_COOKIE, "second=2; Path=/")
                        .body(Body::from("cookies"))
                        .unwrap()
                }),
            )
            .route(
                "/__byok-api__/stream",
                get(move || {
                    let released = released.clone();
                    async move {
                        let receiver = released.lock().take();
                        let chunks = futures_util::stream::once(async { "first " })
                            .chain(futures_util::stream::once(async move {
                                if let Some(receiver) = receiver {
                                    let _ = receiver.await;
                                }
                                "second"
                            }))
                            .map(|chunk| Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk)));
                        Response::new(Body::from_stream(chunks))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (Url::parse(&format!("http://{address}")).unwrap(), release)
    }

    /// 同名多值响应头（多个 `Set-Cookie`）在反代之后必须都还在。
    #[tokio::test]
    async fn the_development_proxy_keeps_repeated_response_headers() {
        let (target, _release) = frontend_proxy_fixture().await;
        let response = frontend_proxy_router(target)
            .oneshot(
                Request::builder()
                    .uri("/__byok-api__/cookies")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let cookies = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            cookies,
            vec!["first=1; Path=/".to_owned(), "second=2; Path=/".to_owned()],
            "insert 只会留下最后一条 Set-Cookie"
        );
    }

    /// 响应是流式转发的：上游第二段还在等放行时，第一段必须已经到达客户端——
    /// 整段缓冲的实现会在这里挂到超时。
    #[tokio::test]
    async fn the_development_proxy_forwards_the_response_as_a_stream() {
        let (target, release) = frontend_proxy_fixture().await;
        // 这一层超时是断言的一部分：缓冲实现下 handler 要等上游整段结束才返回，而上游
        // 正等着测试放行——没有它，用例会挂住而不是变红。
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            frontend_proxy_router(target).oneshot(
                Request::builder()
                    .uri("/__byok-api__/stream")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("整段缓冲的实现会在这里挂住：上游第二段还在等放行")
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let mut frames = response.into_body().into_data_stream();
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), frames.next())
            .await
            .expect("第一段必须在放行之前到达")
            .expect("body 还有第一段")
            .unwrap();
        assert_eq!(first, "first ");

        release.send(()).unwrap();
        let mut rest = Vec::new();
        while let Some(frame) =
            tokio::time::timeout(std::time::Duration::from_secs(5), frames.next())
                .await
                .expect("放行之后 body 必须收尾")
        {
            rest.extend_from_slice(&frame.unwrap());
        }
        assert_eq!(String::from_utf8_lossy(&rest), "second");
    }

    #[test]
    fn loopback_origins_are_allowed() {
        for allowed in [
            // 前端固定从这里提供服务。
            "http://127.0.0.1:1634",
            "http://[::1]:1634",
            "http://localhost:1634",
            // Tauri 的 webview 自有来源。
            "tauri://localhost",
            "http://tauri.localhost",
        ] {
            assert!(local_origin(&origin(allowed)), "{allowed} 应当放行");
        }
    }

    #[test]
    fn private_network_origins_are_rejected() {
        // 这个 API 没有鉴权，同局域网里以私网地址托管的页面不是本机的页面。
        for rejected in [
            "http://192.168.1.20:1634",
            "http://10.0.0.5:1634",
            "http://172.16.0.9:1634",
            "http://169.254.10.2:1634",
            "http://[fd00::1]:1634",
            "http://[fe80::1]:1634",
        ] {
            assert!(!local_origin(&origin(rejected)), "{rejected} 应当拒绝");
        }
    }
}
