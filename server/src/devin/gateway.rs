//! Loopback-only Devin Connect gateway.
//!
//! This listener is intentionally separate from the Cursor listener. It is
//! disabled by default and only reads the persisted Devin binding table when
//! it is explicitly enabled.

use std::{
    collections::HashSet,
    future::IntoFuture,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderMap, Method, Response, StatusCode},
    response::IntoResponse,
    routing::{any, get},
    Router,
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::{
    network::{NetworkClients, OutboundClient},
    provider::{ModelEvent, Provider},
    store::Store,
    Error, Result,
};

use super::{
    assignment::{assignment_token_response, find_model_reference, AssignmentSessions},
    catalog,
    host_config::DevinHostPorts,
    request::{parse_chat_request, to_invocation},
    response::{finish, stream_event, ResponseState},
    wire::{self, ConnectError, MAX_BODY_SIZE},
    DevinModelBinding, DevinSettings,
};

const TOKEN_HEADER: &str = "x-devin-router-token";

/// The three loopback ports a running gateway holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DevinBoundPorts {
    pub api: u16,
    pub inference: u16,
    pub local_api: u16,
}

impl DevinBoundPorts {
    pub fn contains(&self, port: u16) -> bool {
        port == self.api || port == self.inference || port == self.local_api
    }
}

/// 写入 Devin 设置前的准入检查：设置会把客户端指向 API 与推理两个端口，而网关只在
/// 启动时绑一次端口。写下去之前必须确认它真的在这两个端口上监听，否则客户端
/// 从下一个请求起全部打到没人应答的端口，界面却只显示"已保存、等重启"。
///
/// `bound` 来自 `DevinListening::bound_ports()`（control 侧持有同一个句柄）。
pub fn ensure_host_ports_listening(
    ports: DevinHostPorts,
    bound: Option<DevinBoundPorts>,
) -> Result<()> {
    let Some(bound) = bound else {
        return Err(Error::Config(
            "the Devin gateway is not listening yet; enable it and restart haxsd byok so it binds \
             the configured ports before writing Devin settings"
                .into(),
        ));
    };
    if bound.api != ports.api_port || bound.inference != ports.inference_port {
        return Err(Error::Config(format!(
            "the Devin gateway is listening on {} / {}, not on the configured {} / {}; \
             restart haxsd byok so it binds the saved ports before writing Devin settings",
            bound.api, bound.inference, ports.api_port, ports.inference_port,
        )));
    }
    Ok(())
}

fn is_model_catalog_method(method: &str) -> bool {
    matches!(
        method,
        "GetCascadeModelConfigs"
            | "GetCascadeModelConfigsForSite"
            | "GetCommandModelConfigs"
            | "GetCommandModelConfigsForSite"
            | "GetCliModelConfigs"
            | "GetCliModelConfigsForSite"
    )
}

fn catalog_has_default_override(method: &str) -> bool {
    matches!(method, "GetCascadeModelConfigs" | "GetCliModelConfigs")
}

fn is_locally_answered_capacity_method(method: &str) -> bool {
    matches!(
        method,
        "CheckChatCapacity" | "CheckUserMessageRateLimit" | "GetModelStatuses"
    )
}

/// Tracks whether the gateway is currently accepting requests, and on which ports.
/// The management UI must ask the server instead of probing the ports from the
/// browser: those ports serve a different origin and answer no CORS request, so a
/// browser probe would report a healthy gateway as unreachable. Saving Devin
/// settings also reads the bound ports: a port this process already holds is ours,
/// not a conflict, and must not be reported as one.
#[derive(Clone, Default)]
pub struct DevinListening(Arc<parking_lot::RwLock<Option<DevinBoundPorts>>>);

impl DevinListening {
    pub fn is_listening(&self) -> bool {
        self.bound_ports().is_some()
    }

    /// The ports the gateway holds right now; `None` while it is not serving.
    pub fn bound_ports(&self) -> Option<DevinBoundPorts> {
        *self.0.read()
    }

    fn set(&self, ports: Option<DevinBoundPorts>) {
        *self.0.write() = ports;
    }
}

#[derive(Clone)]
pub struct DevinGateway {
    store: Store,
    provider: Arc<dyn Provider>,
    clients: NetworkClients,
    listening: DevinListening,
}

#[derive(Clone)]
struct GatewayState {
    store: Store,
    provider: Arc<dyn Provider>,
    assignments: Arc<AssignmentSessions>,
    /// Provides the client for upstream forwarding. Resolved per request rather
    /// than captured once, so a proxy change reaches this hop too; the client
    /// itself is pooled and cached, so this is a cheap read.
    clients: NetworkClients,
}

impl DevinGateway {
    pub fn new(store: Store, provider: Arc<dyn Provider>, clients: NetworkClients) -> Self {
        Self {
            store,
            provider,
            clients,
            listening: DevinListening::default(),
        }
    }

    /// A handle the management API reads to answer whether the ports are open.
    pub fn listening(&self) -> DevinListening {
        self.listening.clone()
    }

    pub async fn serve(self, shutdown: CancellationToken) -> Result<()> {
        let settings = self.store.devin_settings().await?;
        if !settings.enabled {
            tracing::debug!("Devin gateway is disabled; no Devin ports will be opened");
            shutdown.cancelled().await;
            return Ok(());
        }

        // The response body is forwarded exactly as it arrives, so the client of
        // this hop has to decode nothing on our behalf and no automatic
        // decompression may rewrite it — `devin_client` builds it that way. It is
        // resolved once here only so that a broken proxy configuration fails at
        // startup rather than on the first sign-in; requests resolve it again, so
        // a later proxy change is picked up.
        self.clients.devin_client().await.map_err(|error| {
            Error::Config(format!("Devin upstream client could not be built: {error}"))
        })?;

        let state = GatewayState {
            store: self.store,
            provider: self.provider,
            assignments: Arc::new(AssignmentSessions::new()),
            clients: self.clients,
        };
        let router = Self::router(&state);

        let api_listener = bind(settings.api_port).await?;
        let inference_listener = bind(settings.inference_port).await?;
        let local_listener = bind(settings.local_api_port).await?;
        tracing::info!(
            api_port = settings.api_port,
            inference_port = settings.inference_port,
            local_api_port = settings.local_api_port,
            "Devin gateway listening on loopback"
        );

        let api = axum::serve(api_listener, router.clone()).into_future();
        let inference = axum::serve(inference_listener, router.clone()).into_future();
        let local = axum::serve(local_listener, router).into_future();
        tokio::pin!(api, inference, local);
        // The ports are bound, so from here on the management API must report them
        // as open — and a settings save must recognize them as this process's own —
        // until this task returns for any reason.
        self.listening.set(Some(DevinBoundPorts {
            api: settings.api_port,
            inference: settings.inference_port,
            local_api: settings.local_api_port,
        }));
        let result = tokio::select! {
            result = &mut api => result.map_err(Error::Io),
            result = &mut inference => result.map_err(Error::Io),
            result = &mut local => result.map_err(Error::Io),
            _ = shutdown.cancelled() => Ok(()),
        };
        self.listening.set(None);
        result
    }

    /// The HTTP surface shared by all three listeners. Kept separate from
    /// `serve` so the request wiring can be exercised without binding ports.
    fn router(state: &GatewayState) -> Router {
        Router::new()
            .route("/health", get(health))
            .fallback(any(handle_request))
            .with_state(state.clone())
    }
}

async fn bind(port: u16) -> Result<TcpListener> {
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .map_err(Error::Io)
}

async fn health() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"ok":true,"service":"devin"}"#,
    )
}

async fn handle_request(
    State(state): State<GatewayState>,
    request: Request<Body>,
) -> Response<Body> {
    let (parts, body) = request.into_parts();
    if parts.method != Method::POST {
        return plain_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Devin gateway accepts POST only",
        );
    }
    let method = parts.uri.path().rsplit('/').next().unwrap_or_default();

    let settings = match state.store.devin_settings().await {
        Ok(settings) => settings,
        Err(error) => return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    if !settings.enabled {
        return connect_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "failed_precondition",
            "Devin gateway is disabled",
        );
    }
    if !authorized(&parts.headers, &settings.auth_token) {
        return connect_error_response(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "invalid Devin gateway token",
        );
    }

    // Sign-in, account state, registration and everything else the client needs
    // from its own service travels upstream untouched. Only model traffic is
    // answered here, which is what makes a real account usable behind this
    // gateway at all. `AssignModel` belongs to that model traffic: it hands the
    // client the session token this gateway issues for a mapped model UID.
    if !matches!(method, "GetChatMessage" | "AssignModel")
        && !is_model_catalog_method(method)
        && !is_locally_answered_capacity_method(method)
    {
        return forward_upstream(&state, &settings, parts, body).await;
    }
    let candidates = settings
        .bindings
        .iter()
        .filter(|binding| binding.enabled)
        .map(|binding| binding.model_uid.clone())
        .collect::<HashSet<_>>();
    // 一条启用绑定都没有时目录不是我们的东西：本地目录会是空的，而客户端只认这
    // 一份 RPC 的结果，空目录会把它原本的官方模型列表整个顶掉。这时把请求交给
    // 上游，与其它非模型流量同一条路径——"只答我们拥有的那部分"才是这里的取舍。
    if (is_model_catalog_method(method) || is_locally_answered_capacity_method(method))
        && candidates.is_empty()
    {
        return forward_upstream(&state, &settings, parts, body).await;
    }

    let content_encoding = parts
        .headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok());
    let connect_content_encoding = parts
        .headers
        .get("connect-content-encoding")
        .and_then(|value| value.to_str().ok());
    let body = match to_bytes(body, MAX_BODY_SIZE + 5).await {
        Ok(body) => body,
        Err(error) => return protocol_error(StatusCode::PAYLOAD_TOO_LARGE, error.to_string()),
    };
    // 响应封装照请求本身体判定，而"请求是不是 Connect 信封"只有解码后才知道：请求体
    // 可能是 gzip 的，拿压缩字节去嗅探必然误判，客户端会拿到裸 proto 响应。
    let unwrapped = match wire::unwrap_request(&body, content_encoding, connect_content_encoding) {
        Ok(unwrapped) => unwrapped,
        Err(error) => {
            // 真实的 Devin 客户端和本地探针在压缩层上不完全一致。失败时记录足以区分
            // "HTTP 整个 body 压缩" 与 "Connect 帧内 payload 压缩" 的摘要，但不记录
            // Authorization，也不把可能含提示词的完整 body 写进日志。
            let prefix_len = body.len().min(16);
            tracing::warn!(
                method,
                body_len = body.len(),
                body_prefix = %hex::encode(&body[..prefix_len]),
                content_encoding = ?parts.headers.get(header::CONTENT_ENCODING),
                connect_content_encoding = ?parts.headers.get("connect-content-encoding"),
                content_type = ?parts.headers.get(header::CONTENT_TYPE),
                "cannot unwrap Devin request"
            );
            return protocol_error(StatusCode::BAD_REQUEST, error.to_string());
        }
    };
    let enveloped = unwrapped.enveloped;
    let payload = unwrapped.payload;

    if is_model_catalog_method(method) {
        let gateway_url = format!("http://127.0.0.1:{}", settings.local_api_port);
        let models = match state.store.models().await {
            Ok(models) => models,
            Err(error) => {
                return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        let catalog = match catalog::catalog_response_payload(
            &settings,
            &models,
            &gateway_url,
            catalog_has_default_override(method),
        ) {
            Ok(catalog) => catalog,
            Err(error) => {
                return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        return protocol_success_response(enveloped, &catalog);
    }

    if method == "CheckChatCapacity" {
        let response =
            match wire::serialize_fields(&[wire::Field::varint(1, 1), wire::Field::varint(3, 0)]) {
                Ok(response) => response,
                Err(error) => {
                    return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                }
            };
        return protocol_success_response(enveloped, &response);
    }
    if method == "CheckUserMessageRateLimit" {
        let response = match wire::serialize_fields(&[
            wire::Field::varint(1, 1),
            wire::Field::varint(3, 1_000_000),
            wire::Field::varint(4, 1_000_000),
            wire::Field::varint(5, 0),
        ]) {
            Ok(response) => response,
            Err(error) => {
                return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        return protocol_success_response(enveloped, &response);
    }
    if method == "GetModelStatuses" {
        let response = match wire::serialize_fields(&[]) {
            Ok(response) => response,
            Err(error) => {
                return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        return protocol_success_response(enveloped, &response);
    }

    let reference =
        match find_model_reference(&payload, &parts.headers, &candidates, &state.assignments) {
            Ok(reference) => reference,
            Err(error) => return protocol_error(StatusCode::BAD_REQUEST, error.to_string()),
        };
    if reference.ambiguous {
        return protocol_error(
            StatusCode::BAD_REQUEST,
            format!(
                "Devin request contains multiple mapped model UIDs: {}",
                reference.candidates.join(", ")
            ),
        );
    }
    if method == "AssignModel" {
        let Some(uid) = (!reference.uid.is_empty()).then_some(reference.uid) else {
            return protocol_error(
                StatusCode::BAD_REQUEST,
                "Devin AssignModel has no mapped model UID",
            );
        };
        if !candidates.contains(&uid) {
            return protocol_error(
                StatusCode::BAD_REQUEST,
                format!("Devin model is not assigned: {uid}"),
            );
        }
        let assignment = state.assignments.issue(&uid);
        let harness_uid = format!("cursor-byok:{uid}");
        let response = match assignment_token_response(&assignment, &harness_uid) {
            Ok(response) => response,
            Err(error) => {
                return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        return protocol_success_response(enveloped, &response);
    }

    let request = match parse_chat_request(&payload) {
        Ok(request) => request,
        Err(error) => return protocol_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let mut request = request;
    if request.requested_model.is_empty() {
        request.requested_model = reference.uid;
    }
    let binding = match settings.binding(&request.requested_model).cloned() {
        Some(binding) => binding,
        None => {
            return protocol_error(
                StatusCode::BAD_REQUEST,
                format!("Devin model is not assigned: {}", request.requested_model),
            )
        }
    };
    let model_hash = model_lookup_hash(&binding);
    let model = match state.store.model(model_hash).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return protocol_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "the model bound to Devin model {} is no longer in the model library; \
                     re-select it on the Devin page",
                    binding.model_uid
                ),
            )
        }
        Err(error) => return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let provider_type = model.provider_type();
    let fallback_input_tokens = request.context_tokens;
    let invocation = match to_invocation(request, &binding, &model) {
        Ok(invocation) => invocation,
        Err(error) => return protocol_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let model_uid = binding.model_uid.clone();
    let cancellation = CancellationToken::new();
    let mut provider_stream = state.provider.stream(invocation, cancellation.clone());
    let mut response_state = ResponseState::new("devin-response", provider_type);
    response_state.set_fallback_input_tokens(fallback_input_tokens);
    let stream = async_stream::stream! {
        while let Some(event) = provider_stream.next().await {
            match event {
                Ok(event) => {
                    let done = matches!(&event, ModelEvent::Done(_));
                    match stream_event(&mut response_state, event) {
                        Ok(frames) => {
                            for frame in frames {
                                yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                            }
                        }
                        Err(error) => {
                            yield Ok::<Bytes, std::io::Error>(stream_error(error));
                            return;
                        }
                    }
                    if done {
                        match finish(response_state, &model_uid) {
                            Ok(frames) => {
                                for frame in frames {
                                    yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                                }
                            }
                            Err(error) => yield Ok::<Bytes, std::io::Error>(stream_error(error)),
                        }
                        return;
                    }
                }
                Err(error) => {
                    yield Ok::<Bytes, std::io::Error>(stream_error(error));
                    return;
                }
            }
        }
        yield Ok::<Bytes, std::io::Error>(stream_error("provider stream ended without Done"));
    };
    streaming_response(stream, cancellation)
}

/// The Cursor BYOK model record that serves a Devin binding: the enabled active
/// route's hash when one is selected, otherwise the legacy primary hash. The
/// Devin model UID stays the request identity.
fn model_lookup_hash(binding: &DevinModelBinding) -> &str {
    binding.effective_model_hash()
}

fn authorized(headers: &HeaderMap, configured: &str) -> bool {
    if configured.is_empty() {
        return true;
    }
    headers
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == configured)
        || headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|value| value == configured)
}

/// Forwards one request to the configured upstream and streams its answer back
/// unchanged, so the client keeps talking to its own service for everything the
/// gateway does not own. Hop-by-hop headers are dropped; the rest travels as-is
/// because the service authenticates on them.
///
/// The client comes from `OutboundClient::devin` rather than the plain cache: this
/// hop is long-lived, so a system proxy that stops listening (Clash restarting its
/// core) would otherwise keep failing until the app restarts. A connection failure
/// rebuilds the client from the current settings and retries once, exactly like the
/// provider and Cursor-passthrough paths.
async fn forward_upstream(
    state: &GatewayState,
    settings: &DevinSettings,
    parts: axum::http::request::Parts,
    body: Body,
) -> Response<Body> {
    let base = settings.upstream_api_url.trim().trim_end_matches('/');
    let path = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let target = format!("{base}{path}");
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::POST);

    let outbound = match OutboundClient::devin(state.clients.clone()).await {
        Ok(client) => client,
        Err(error) => {
            return protocol_error(
                StatusCode::BAD_GATEWAY,
                format!("Devin upstream client could not be built: {error}"),
            )
        }
    };
    let bytes = match to_bytes(body, MAX_BODY_SIZE + 5).await {
        Ok(bytes) => bytes,
        Err(error) => return protocol_error(StatusCode::PAYLOAD_TOO_LARGE, error.to_string()),
    };
    // 本地网关凭据绝不能跟着透传请求离开本机：`x-devin-router-token` 是我们自己的头，
    // 而客户端也可能把同一个令牌放进 `Authorization: Bearer`（`authorized()` 两种都收）。
    // 其余头部保持原样转发——登录/账号/遥测请求会带上游需要、而我们不知道的头。
    let local_bearer = format!("Bearer {}", settings.auth_token);
    let local_token_configured = !settings.auth_token.is_empty();
    // 重建一次就够：重试仍失败说明不是出网路由的问题，照旧上报。
    let send = |client: &reqwest::Client| {
        let mut outgoing = client.request(method.clone(), &target);
        for (name, value) in parts.headers.iter() {
            if matches!(
                name.as_str(),
                "host" | "connection" | "content-length" | "transfer-encoding" | "accept-encoding"
            ) || name.as_str() == TOKEN_HEADER
            {
                continue;
            }
            if local_token_configured
                && name == header::AUTHORIZATION
                && value.as_bytes() == local_bearer.as_bytes()
            {
                continue;
            }
            outgoing = outgoing.header(name.as_str(), value.as_bytes());
        }
        outgoing.body(bytes.clone())
    };
    let response = match send(outbound.client()).send().await {
        Ok(response) => response,
        Err(error) => {
            let Some(client) = outbound.rebuild_after(&error).await else {
                return protocol_error(
                    StatusCode::BAD_GATEWAY,
                    format!("Devin upstream request failed: {error}"),
                );
            };
            match send(&client).send().await {
                Ok(response) => response,
                Err(error) => {
                    return protocol_error(
                        StatusCode::BAD_GATEWAY,
                        format!("Devin upstream request failed: {error}"),
                    )
                }
            }
        }
    };

    let status = response.status();
    let mut builder = Response::builder().status(status);
    for (name, value) in response.headers().iter() {
        if matches!(
            name.as_str(),
            "connection" | "transfer-encoding" | "content-length" | "content-encoding"
        ) {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    // The body is forwarded undecoded, so its encoding header must survive too.
    if let Some(encoding) = response.headers().get(header::CONTENT_ENCODING) {
        builder = builder.header(header::CONTENT_ENCODING, encoding.as_bytes());
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|error| {
            protocol_error(
                StatusCode::BAD_GATEWAY,
                format!("Devin upstream response could not be rebuilt: {error}"),
            )
        })
}

fn plain_error(status: StatusCode, message: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(message.to_owned()))
        .expect("static Devin error response")
}

fn protocol_error(status: StatusCode, message: impl Into<String>) -> Response<Body> {
    let code = match status {
        StatusCode::UNAUTHORIZED => "unauthenticated",
        StatusCode::PAYLOAD_TOO_LARGE => "resource_exhausted",
        StatusCode::SERVICE_UNAVAILABLE => "failed_precondition",
        StatusCode::INTERNAL_SERVER_ERROR => "internal",
        _ => "invalid_argument",
    };
    connect_error_response(status, code, message)
}

/// 成功响应：请求是 Connect 信封就按信封回，否则回裸 proto。
///
/// `enveloped` 来自**解码后**的请求体（`wire::unwrap_request`），不是原始字节：原始
/// 字节可能是 gzip 的，嗅探会失败。
fn protocol_success_response(enveloped: bool, payload: &[u8]) -> Response<Body> {
    let (body, content_type) = if enveloped {
        let mut body =
            wire::frame(payload, false).unwrap_or_else(|error| stream_error(error).to_vec());
        body.extend_from_slice(&wire::end_frame(None));
        (body, "application/connect+proto")
    } else {
        (payload.to_vec(), "application/proto")
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body))
        .expect("Devin protocol success response")
}

fn connect_error_response(
    _status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> Response<Body> {
    let frame = wire::end_frame(Some(ConnectError {
        code: code.into(),
        message: message.into(),
    }));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/connect+proto")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(frame))
        .expect("Devin protocol error response")
}

fn streaming_response<S>(stream: S, cancellation: CancellationToken) -> Response<Body>
where
    S: Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send + 'static,
{
    let stream = CancellationOnDrop {
        stream: Box::pin(stream),
        guard: Some(cancellation.drop_guard()),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/connect+proto")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .expect("Devin streaming response")
}

/// 响应体被客户端提前丢弃时取消已经启动的上游流；读到流结尾则解除守卫。
struct CancellationOnDrop<S> {
    stream: Pin<Box<S>>,
    guard: Option<DropGuard>,
}

impl<S: Stream> Stream for CancellationOnDrop<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        let polled = this.stream.as_mut().poll_next(context);
        if matches!(polled, Poll::Ready(None)) {
            if let Some(guard) = this.guard.take() {
                guard.disarm();
            }
        }
        polled
    }
}

fn stream_error(error: impl std::fmt::Display) -> Bytes {
    Bytes::from(wire::end_frame(Some(ConnectError {
        code: "internal".into(),
        message: error.to_string(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devin::{DevinModelBinding, DevinRoute};

    #[test]
    fn token_auth_accepts_header_or_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(TOKEN_HEADER, "secret".parse().unwrap());
        assert!(authorized(&headers, "secret"));
        headers.remove(TOKEN_HEADER);
        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(authorized(&headers, "secret"));
        assert!(!authorized(&HeaderMap::new(), "secret"));
        assert!(authorized(&HeaderMap::new(), ""));
    }

    #[test]
    fn error_frame_is_bounded_by_the_same_wire_limit() {
        let frame = stream_error("provider failed");
        assert!(frame.len() < MAX_BODY_SIZE);
        assert_eq!(frame[0], 2);
    }

    #[test]
    fn selected_route_hash_drives_the_cursor_byok_model_lookup() {
        let routed = DevinModelBinding {
            model_uid: "devin-sonnet".into(),
            model_hash: "legacy-hash".into(),
            enabled: true,
            routes: vec![DevinRoute {
                route_id: "active".into(),
                model_hash: "active-hash".into(),
                label: "Active".into(),
                enabled: true,
            }],
            active_route_id: Some("active".into()),
            ..DevinModelBinding::new("", "")
        };
        assert_eq!(model_lookup_hash(&routed), "active-hash");
        assert_eq!(routed.model_uid, "devin-sonnet");

        let legacy = DevinModelBinding::new("devin-legacy", "legacy-hash");
        assert_eq!(model_lookup_hash(&legacy), "legacy-hash");
        assert_eq!(legacy.model_uid, "devin-legacy");
    }

    /// Neither the forwarded-method test nor the router wiring needs a provider
    /// that answers, so the double stays empty on purpose.
    struct IdleProvider;

    impl Provider for IdleProvider {
        fn stream(
            &self,
            _invocation: crate::model::ModelInvocation,
            _cancellation: CancellationToken,
        ) -> crate::provider::ProviderStream {
            Box::pin(futures_util::stream::empty())
        }
    }

    struct StreamingProvider {
        cancellation: Arc<parking_lot::Mutex<Option<CancellationToken>>>,
        completes: bool,
    }

    impl Provider for StreamingProvider {
        fn stream(
            &self,
            _invocation: crate::model::ModelInvocation,
            cancellation: CancellationToken,
        ) -> crate::provider::ProviderStream {
            *self.cancellation.lock() = Some(cancellation);
            if self.completes {
                Box::pin(futures_util::stream::iter([Ok(ModelEvent::Done(
                    crate::provider::FinishReason::Stop,
                ))]))
            } else {
                Box::pin(futures_util::stream::pending())
            }
        }
    }

    async fn streaming_chat_gateway(
        completes: bool,
    ) -> (
        tempfile::TempDir,
        Router,
        Arc<parking_lot::Mutex<Option<CancellationToken>>>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let model = store
            .create_model(&crate::model::ModelConfigInput {
                sort_order: 0,
                display_name: "Devin stream test model".into(),
                group_name: None,
                model_type: crate::model::ModelType::OpenAi,
                base_url: "http://127.0.0.1:1".into(),
                use_full_url: true,
                api_key: "test-key".into(),
                tooltip_data: "Devin stream test".into(),
                model_id: "test-model".into(),
                reasoning_effort: None,
                openai_endpoint: crate::model::OPENAI_CHAT_ENDPOINT.into(),
                openai_extra_params_enabled: false,
                openai_extra_params: serde_json::json!({}),
                custom_headers_enabled: false,
                custom_headers: serde_json::json!({}),
                anthropic_extra_params_enabled: false,
                anthropic_extra_params: serde_json::json!({}),
                context_window_tokens: Some(32_768),
                max_completion_tokens: Some(4_096),
                anthropic_max_tokens: None,
                anthropic_thinking_effort: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap();
        store
            .set_devin_settings(DevinSettings {
                enabled: true,
                upstream_api_url: "http://127.0.0.1:1".into(),
                bindings: vec![DevinModelBinding::new("devin-sonnet", model.model_hash)],
                ..DevinSettings::default()
            })
            .await
            .unwrap();
        let cancellation = Arc::new(parking_lot::Mutex::new(None));
        let state = GatewayState {
            store: store.clone(),
            provider: Arc::new(StreamingProvider {
                cancellation: cancellation.clone(),
                completes,
            }),
            assignments: Arc::new(AssignmentSessions::new()),
            clients: NetworkClients::new(store),
        };
        (directory, DevinGateway::router(&state), cancellation)
    }

    fn streaming_chat_request() -> Request<Body> {
        let payload = wire::serialize_fields(&[wire::Field::bytes(21, b"devin-sonnet")]).unwrap();
        Request::builder()
            .method(Method::POST)
            .uri("/exa.api_server_pb.ApiServerService/GetChatMessage")
            .body(Body::from(payload))
            .unwrap()
    }

    /// 客户端关闭 Devin 页面或取消生成时，HTTP 响应体会被丢弃；此时已经启动的上游
    /// 请求必须同步收到取消信号，不能继续消耗供应商额度。
    #[tokio::test]
    async fn dropping_a_devin_response_cancels_the_upstream_request() {
        use tower::ServiceExt;

        let (_directory, router, cancellation) = streaming_chat_gateway(false).await;
        let response = router.oneshot(streaming_chat_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cancellation = cancellation
            .lock()
            .clone()
            .expect("provider must receive a cancellation token");
        assert!(!cancellation.is_cancelled());

        drop(response);

        assert!(
            cancellation.is_cancelled(),
            "客户端丢弃响应体后必须取消上游请求"
        );
    }

    /// 正常读完响应体时要解除 DropGuard，不能把已经成功结束的上游调用标记成取消。
    #[tokio::test]
    async fn a_completed_devin_stream_disarms_the_cancellation_guard() {
        use axum::body::to_bytes as read_body;
        use tower::ServiceExt;

        let (_directory, router, cancellation) = streaming_chat_gateway(true).await;
        let response = router.oneshot(streaming_chat_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cancellation = cancellation
            .lock()
            .clone()
            .expect("provider must receive a cancellation token");

        let body = read_body(response.into_body(), MAX_BODY_SIZE)
            .await
            .unwrap();
        assert!(!body.is_empty());
        assert!(!cancellation.is_cancelled(), "正常结束时不能误发取消信号");
    }

    /// Sign-in and account calls must reach the client's own service, otherwise a
    /// real account can never log in behind this gateway.
    #[tokio::test]
    async fn methods_the_gateway_does_not_own_are_forwarded_upstream() {
        use axum::body::to_bytes as read_body;
        use tower::ServiceExt;

        let received: Arc<parking_lot::Mutex<Vec<(String, String)>>> = Arc::default();
        let seen = received.clone();
        let upstream = Router::new().fallback(any(move |request: Request<Body>| {
            let seen = seen.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = read_body(body, MAX_BODY_SIZE).await.unwrap();
                let authorization = parts
                    .headers
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                seen.lock().push((
                    parts.uri.path().to_owned(),
                    format!("{}|{authorization}", String::from_utf8_lossy(&bytes)),
                ));
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/proto")
                    .body(Body::from(b"upstream-answer".to_vec()))
                    .unwrap()
            }
        }));
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        tokio::spawn(axum::serve(upstream_listener, upstream).into_future());

        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        store
            .set_devin_settings(DevinSettings {
                enabled: true,
                upstream_api_url: format!("http://127.0.0.1:{upstream_port}"),
                ..DevinSettings::default()
            })
            .await
            .unwrap();

        let provider: Arc<dyn Provider> = Arc::new(IdleProvider);
        // The upstream here is a loopback test server, so the client must not be
        // routed through any proxy the machine happens to have configured.
        store
            .set_proxy_settings(crate::store::ProxySettingsInput {
                mode: crate::store::ProxyMode::Direct,
                address: String::new(),
                auth_enabled: false,
                username: String::new(),
                password: None,
            })
            .await
            .unwrap();
        let clients = NetworkClients::new(store.clone());
        let state = GatewayState {
            store,
            provider,
            assignments: Arc::new(AssignmentSessions::new()),
            clients,
        };

        let response = DevinGateway::router(&state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/exa.api_server_pb.ApiServerService/GetCurrentUser")
                    .header(header::AUTHORIZATION, "Bearer account-token")
                    .body(Body::from("login-body"))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let body = read_body(response.into_body(), MAX_BODY_SIZE)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], b"upstream-answer");
        let seen = received.lock().clone();
        assert_eq!(
            seen.len(),
            1,
            "the upstream must receive exactly one request"
        );
        assert_eq!(
            seen[0].0,
            "/exa.api_server_pb.ApiServerService/GetCurrentUser"
        );
        assert_eq!(seen[0].1, "login-body|Bearer account-token");
    }

    /// 本地网关令牌是"只在本机使用"的凭据：透传到厂商上游的请求既不能带
    /// `x-devin-router-token`，也不能把 `Authorization: Bearer <本地令牌>` 复制过去，
    /// 而其余头部（上游登录/账号流程要用的）必须照旧转发。
    #[tokio::test]
    async fn the_local_gateway_token_never_reaches_upstream() {
        use axum::body::to_bytes as read_body;
        use tower::ServiceExt;

        let received: Arc<parking_lot::Mutex<Vec<(String, String)>>> = Arc::default();
        let seen = received.clone();
        let upstream = Router::new().fallback(any(move |request: Request<Body>| {
            let seen = seen.clone();
            async move {
                let (parts, _body) = request.into_parts();
                seen.lock().extend(
                    parts
                        .headers
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.as_str().to_owned(),
                                value.to_str().unwrap_or_default().to_owned(),
                            )
                        })
                        .collect::<Vec<_>>(),
                );
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/proto")
                    .body(Body::from(b"upstream-answer".to_vec()))
                    .unwrap()
            }
        }));
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        tokio::spawn(axum::serve(upstream_listener, upstream).into_future());

        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        store
            .set_devin_settings(DevinSettings {
                enabled: true,
                auth_token: "local-secret".into(),
                upstream_api_url: format!("http://127.0.0.1:{upstream_port}"),
                ..DevinSettings::default()
            })
            .await
            .unwrap();
        store
            .set_proxy_settings(crate::store::ProxySettingsInput {
                mode: crate::store::ProxyMode::Direct,
                address: String::new(),
                auth_enabled: false,
                username: String::new(),
                password: None,
            })
            .await
            .unwrap();
        let provider: Arc<dyn Provider> = Arc::new(IdleProvider);
        let clients = NetworkClients::new(store.clone());
        let state = GatewayState {
            store,
            provider,
            assignments: Arc::new(AssignmentSessions::new()),
            clients,
        };

        let response = DevinGateway::router(&state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/exa.api_server_pb.ApiServerService/GetCurrentUser")
                    .header(TOKEN_HEADER, "local-secret")
                    .header(header::AUTHORIZATION, "Bearer local-secret")
                    .header("x-windsurf-client", "keep-me")
                    .body(Body::from("login-body"))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let body = read_body(response.into_body(), MAX_BODY_SIZE)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], b"upstream-answer");
        let seen = received.lock().clone();
        assert!(!seen.is_empty(), "上游必须收到这次透传");
        for (name, value) in &seen {
            assert_ne!(name, TOKEN_HEADER, "本地令牌头被转发到上游");
            assert!(
                !(name == "authorization" && value.contains("local-secret")),
                "本地令牌经 Authorization 泄漏到上游"
            );
        }
        assert!(
            seen.iter().any(|(name, _)| name == "x-windsurf-client"),
            "非本地凭据的头部必须照旧转发: {seen:?}"
        );
    }

    /// 目录请求的两种 gzip 形状都要按**解码后**的请求体判定封装。真实 Devin 不设
    /// HTTP `Content-Encoding`，而是发一个 `flags=1` 的 Connect 信封，并把 payload
    /// gzip；早期的实现把 `Connect-Content-Encoding` 当成 HTTP 编码又解一次，真实
    /// 模型请求因此直接失败。
    #[tokio::test]
    async fn http_and_connect_gzip_requests_both_get_a_framed_response() {
        use axum::body::to_bytes as read_body;
        use std::io::Write;
        use tower::ServiceExt;

        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        store
            .set_devin_settings(DevinSettings {
                enabled: true,
                upstream_api_url: "http://127.0.0.1:1".into(),
                bindings: vec![DevinModelBinding {
                    enabled: true,
                    ..DevinModelBinding::new("devin-sonnet", "model-hash")
                }],
                ..DevinSettings::default()
            })
            .await
            .unwrap();
        let clients = NetworkClients::new(store.clone());
        let state = GatewayState {
            store,
            provider: Arc::new(IdleProvider),
            assignments: Arc::new(AssignmentSessions::new()),
            clients,
        };

        // 一个 Connect 信封里的目录请求，整体再 gzip 一遍。
        let framed = wire::frame(b"catalog-request", false).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&framed).unwrap();
        let gzipped = encoder.finish().unwrap();
        // 正是旧实现的失败点：压缩后的字节看起来不像信封。
        assert!(
            !wire::is_connect_envelope(&gzipped),
            "压缩字节本来就嗅探不出来"
        );

        let router = DevinGateway::router(&state);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/exa.api_server_pb.ApiServerService/GetCliModelConfigs")
                    .header(header::CONTENT_TYPE, "application/connect+proto")
                    .header(header::CONTENT_ENCODING, "gzip")
                    .body(Body::from(gzipped))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/connect+proto"),
            "gzip 的 Connect 请求必须收到封装响应"
        );
        let body = read_body(response.into_body(), MAX_BODY_SIZE)
            .await
            .unwrap();
        assert_eq!(body[0], 0, "响应第一帧是未压缩的数据帧");
        let declared = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
        let end = &body[5 + declared..];
        assert_eq!(end[0], 2, "数据帧之后是结束帧");
        assert_eq!(
            u32::from_be_bytes([end[1], end[2], end[3], end[4]]) as usize,
            end.len() - 5
        );

        // 真实 Devin 3.11.216 的形状：body 本身是信封，标志位=1，header 只说明
        // Connect 消息编码。这里不能再把整段 body 先 gunzip。
        let connect_gzip = wire::frame(b"catalog-request", true).unwrap();
        assert_eq!(connect_gzip[0], 1);
        assert_eq!(&connect_gzip[5..7], &[0x1f, 0x8b]);
        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/exa.api_server_pb.ApiServerService/GetCliModelConfigs")
                    .header(header::CONTENT_TYPE, "application/connect+proto")
                    .header("connect-content-encoding", "gzip")
                    .body(Body::from(connect_gzip))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/connect+proto"),
        );
        let body = read_body(response.into_body(), MAX_BODY_SIZE)
            .await
            .unwrap();
        assert_eq!(body[0], 0, "Connect gzip 请求同样要收到数据帧");
        let declared = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
        assert_eq!(body[5 + declared], 2, "数据帧之后是结束帧");
    }

    /// 一个只会回固定字节的上游桩，外加它收到的请求数。
    async fn answering_upstream() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = requests.clone();
        let upstream = Router::new().fallback(any(move |_request: Request<Body>| {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/proto")
                    .body(Body::from(b"upstream-answer".to_vec()))
                    .unwrap()
            }
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(axum::serve(listener, upstream).into_future());
        (port, requests)
    }

    /// Devin 设置只能写在网关真正监听的端口上：网关没在监听，或监听的端口与设置不一致，
    /// 都必须拒绝——否则客户端从下一个请求开始全部打空，界面却只显示"已保存、等重启"。
    #[test]
    fn host_configuration_requires_the_gateway_to_listen_on_the_same_ports() {
        let ports = DevinHostPorts {
            api_port: 43_110,
            inference_port: 43_111,
        };
        ensure_host_ports_listening(
            ports,
            Some(DevinBoundPorts {
                api: 43_110,
                inference: 43_111,
                local_api: 43_112,
            }),
        )
        .unwrap();

        let not_listening = ensure_host_ports_listening(ports, None)
            .unwrap_err()
            .to_string();
        assert!(not_listening.contains("not listening"), "{not_listening}");
        for bound in [
            DevinBoundPorts {
                api: 43_120,
                inference: 43_111,
                local_api: 43_112,
            },
            DevinBoundPorts {
                api: 43_110,
                inference: 43_121,
                local_api: 43_112,
            },
        ] {
            let error = ensure_host_ports_listening(ports, Some(bound))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("listening on") && error.contains("restart haxsd byok"),
                "端口漂移时必须说清实际绑定与保存的端口: {error}"
            );
        }
    }

    /// 一条启用绑定都没有时，本地目录会是空的，而客户端只认这份 RPC 的结果：
    /// 空目录会把整份官方模型列表顶掉。这时请求必须转给上游——只答我们拥有的那部分。
    #[tokio::test]
    async fn an_empty_binding_table_forwards_the_model_catalog_upstream() {
        use axum::body::to_bytes as read_body;
        use tower::ServiceExt;

        let (upstream_port, requests) = answering_upstream().await;
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        store
            .set_devin_settings(DevinSettings {
                enabled: true,
                upstream_api_url: format!("http://127.0.0.1:{upstream_port}"),
                ..DevinSettings::default()
            })
            .await
            .unwrap();
        store
            .set_proxy_settings(crate::store::ProxySettingsInput {
                mode: crate::store::ProxyMode::Direct,
                address: String::new(),
                auth_enabled: false,
                username: String::new(),
                password: None,
            })
            .await
            .unwrap();
        let clients = NetworkClients::new(store.clone());
        let state = GatewayState {
            store,
            provider: Arc::new(IdleProvider),
            assignments: Arc::new(AssignmentSessions::new()),
            clients,
        };

        let response = DevinGateway::router(&state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/exa.api_server_pb.ApiServerService/GetCliModelConfigs")
                    .body(Body::from("catalog-request"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = read_body(response.into_body(), MAX_BODY_SIZE)
            .await
            .unwrap();
        assert_eq!(
            &body[..],
            b"upstream-answer",
            "零绑定时的模型目录必须来自上游，而不是本地那份空目录"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "上游必须真的收到这次目录请求"
        );
    }

    /// 新版客户端在主目录之外还调用 ForSite、Command 与 Cascade 变体。它们都用
    /// `client_model_configs`（字段 1）；Cascade 与 CLI 还会读字段 3 的默认模型。
    #[tokio::test]
    async fn every_current_catalog_rpc_returns_the_local_model_list() {
        use axum::body::to_bytes as read_body;
        use tower::ServiceExt;

        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        store
            .set_devin_settings(DevinSettings {
                enabled: true,
                bindings: vec![DevinModelBinding::new("devin-sonnet", "model-hash")],
                default_model_uid: Some("devin-sonnet".into()),
                ..DevinSettings::default()
            })
            .await
            .unwrap();
        let state = GatewayState {
            store: store.clone(),
            provider: Arc::new(IdleProvider),
            assignments: Arc::new(AssignmentSessions::new()),
            clients: NetworkClients::new(store),
        };

        for (method, has_default_override) in [
            ("GetCascadeModelConfigs", true),
            ("GetCascadeModelConfigsForSite", false),
            ("GetCommandModelConfigs", false),
            ("GetCommandModelConfigsForSite", false),
            ("GetCliModelConfigs", true),
            ("GetCliModelConfigsForSite", false),
        ] {
            let response = DevinGateway::router(&state)
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(format!("/exa.api_server_pb.ApiServerService/{method}"))
                        .body(Body::from("catalog-request"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{method}");
            let body = read_body(response.into_body(), MAX_BODY_SIZE)
                .await
                .unwrap();
            let fields = wire::parse_fields(&body).unwrap();
            let models = wire::all_fields(&fields, 1).collect::<Vec<_>>();
            assert_eq!(models.len(), 1, "{method} must return one local model");
            let wire::FieldValue::Bytes(model) = &models[0].value else {
                panic!("{method} model field is not a message");
            };
            let model = wire::parse_fields(model).unwrap();
            assert!(
                model.iter().any(|field| {
                    field.number == 22
                        && field.value == wire::FieldValue::Bytes(b"devin-sonnet".to_vec())
                }),
                "{method} must carry the mapped UID"
            );
            assert_eq!(
                wire::first_field(&fields, 3).is_some(),
                has_default_override,
                "{method} default-override field mismatch"
            );
        }
    }

    /// 对话前的容量与限额检查带的是本地模型 UID，官方服务不认识它。绑定存在时本地
    /// 直接放行，否则模型列表会显示，但第一句话就被上游拒绝。
    #[tokio::test]
    async fn capacity_checks_are_answered_for_local_model_routing() {
        use axum::body::to_bytes as read_body;
        use tower::ServiceExt;

        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        store
            .set_devin_settings(DevinSettings {
                enabled: true,
                bindings: vec![DevinModelBinding::new("devin-sonnet", "model-hash")],
                ..DevinSettings::default()
            })
            .await
            .unwrap();
        let state = GatewayState {
            store: store.clone(),
            provider: Arc::new(IdleProvider),
            assignments: Arc::new(AssignmentSessions::new()),
            clients: NetworkClients::new(store),
        };

        for method in [
            "CheckChatCapacity",
            "CheckUserMessageRateLimit",
            "GetModelStatuses",
        ] {
            let response = DevinGateway::router(&state)
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(format!("/exa.api_server_pb.ApiServerService/{method}"))
                        .body(Body::from("capacity-request"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{method}");
            let body = read_body(response.into_body(), MAX_BODY_SIZE)
                .await
                .unwrap();
            let fields = wire::parse_fields(&body).unwrap();
            if method == "GetModelStatuses" {
                assert!(fields.is_empty(), "local models need no warning statuses");
                continue;
            }
            assert_eq!(
                wire::first_field(&fields, 1),
                Some(&wire::Field::varint(1, 1)),
                "{method} must report capacity"
            );
        }
    }

    /// 出网路由变了（用户中途关掉代理，或系统代理那一跳没人监听）时，第一次连接
    /// 失败要按当前配置重建客户端再试一次，而不是把这一轮直接判死。
    #[tokio::test]
    async fn a_forwarded_request_is_retried_once_after_the_outbound_route_changes() {
        use axum::body::to_bytes as read_body;
        use tower::ServiceExt;

        let (upstream_port, requests) = answering_upstream().await;
        // 一个确定没人监听的代理端口：先占住再放掉。
        let dead_proxy = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        store
            .set_devin_settings(DevinSettings {
                enabled: true,
                upstream_api_url: format!("http://127.0.0.1:{upstream_port}"),
                ..DevinSettings::default()
            })
            .await
            .unwrap();
        store
            .set_proxy_settings(crate::store::ProxySettingsInput {
                mode: crate::store::ProxyMode::Custom,
                address: dead_proxy,
                auth_enabled: false,
                username: String::new(),
                password: None,
            })
            .await
            .unwrap();
        let clients = NetworkClients::new(store.clone());
        // 先让缓存里留下那份"指向死代理"的客户端：这就是系统代理那一跳刚断掉时
        // 进程手里的东西。
        clients.devin_client().await.unwrap();
        // 路由已经变了（用户改成直连），但缓存还没被任何保存路径清掉。
        store
            .set_proxy_settings(crate::store::ProxySettingsInput {
                mode: crate::store::ProxyMode::Direct,
                address: String::new(),
                auth_enabled: false,
                username: String::new(),
                password: None,
            })
            .await
            .unwrap();
        let state = GatewayState {
            store,
            provider: Arc::new(IdleProvider),
            assignments: Arc::new(AssignmentSessions::new()),
            clients,
        };

        let response = DevinGateway::router(&state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/exa.api_server_pb.ApiServerService/GetCurrentUser")
                    .body(Body::from("login-body"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = read_body(response.into_body(), MAX_BODY_SIZE)
            .await
            .unwrap();
        assert_eq!(
            &body[..],
            b"upstream-answer",
            "第一次连接失败后必须按新配置重试一次"
        );
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
