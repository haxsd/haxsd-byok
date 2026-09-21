//! Loopback-only Devin Connect gateway.
//!
//! This listener is intentionally separate from the Cursor listener. It is
//! disabled by default and only reads the persisted Devin binding table when
//! it is explicitly enabled.

use std::{collections::HashSet, future::IntoFuture, net::SocketAddr, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderMap, Method, Response, StatusCode},
    response::IntoResponse,
    routing::{any, get},
    Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::{
    provider::{ModelEvent, Provider},
    store::Store,
    Error, Result,
};

use super::{
    assignment::{assignment_token_response, find_model_reference, AssignmentSessions},
    catalog,
    request::{parse_chat_request, to_invocation},
    response::{finish, stream_event, ResponseState},
    wire::{self, ConnectError, MAX_BODY_SIZE},
    DevinModelBinding, DevinSettings,
};

const TOKEN_HEADER: &str = "x-devin-router-token";

/// Tracks whether the gateway is currently accepting requests. The management UI
/// must ask the server instead of probing the ports from the browser: those ports
/// serve a different origin and answer no CORS request, so a browser probe would
/// report a healthy gateway as unreachable.
#[derive(Clone, Default)]
pub struct DevinListening(Arc<std::sync::atomic::AtomicBool>);

impl DevinListening {
    pub fn is_listening(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set(&self, listening: bool) {
        self.0.store(listening, std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct DevinGateway {
    store: Store,
    provider: Arc<dyn Provider>,
    listening: DevinListening,
}

#[derive(Clone)]
struct GatewayState {
    store: Store,
    provider: Arc<dyn Provider>,
    assignments: Arc<AssignmentSessions>,
    /// Reused for upstream forwarding; a connection pool matters because sign-in
    /// and account calls arrive in bursts.
    upstream: reqwest::Client,
}

impl DevinGateway {
    pub fn new(store: Store, provider: Arc<dyn Provider>) -> Self {
        Self {
            store,
            provider,
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
        // decompression may rewrite it.
        let upstream = reqwest::Client::builder()
            .use_native_tls()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .map_err(|error| {
                Error::Config(format!("Devin upstream client could not be built: {error}"))
            })?;

        let state = GatewayState {
            store: self.store,
            provider: self.provider,
            assignments: Arc::new(AssignmentSessions::new()),
            upstream,
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
        // as open until this task returns for any reason.
        self.listening.set(true);
        let result = tokio::select! {
            result = &mut api => result.map_err(Error::Io),
            result = &mut inference => result.map_err(Error::Io),
            result = &mut local => result.map_err(Error::Io),
            _ = shutdown.cancelled() => Ok(()),
        };
        self.listening.set(false);
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
    // gateway at all.
    if !matches!(method, "GetChatMessage" | "GetCliModelConfigs") {
        return forward_upstream(&state, &settings, parts, body).await;
    }

    let content_encoding = parts
        .headers
        .get("connect-content-encoding")
        .or_else(|| parts.headers.get(header::CONTENT_ENCODING))
        .and_then(|value| value.to_str().ok());
    let body = match to_bytes(body, MAX_BODY_SIZE + 5).await {
        Ok(body) => body,
        Err(error) => return protocol_error(StatusCode::PAYLOAD_TOO_LARGE, error.to_string()),
    };
    let payload = match wire::unwrap_request(&body, content_encoding) {
        Ok(payload) => payload,
        Err(error) => return protocol_error(StatusCode::BAD_REQUEST, error.to_string()),
    };

    let candidates = settings
        .bindings
        .iter()
        .filter(|binding| binding.enabled)
        .map(|binding| binding.model_uid.clone())
        .collect::<HashSet<_>>();
    if method == "GetCliModelConfigs" {
        let gateway_url = format!("http://127.0.0.1:{}", settings.local_api_port);
        let catalog = match catalog::model_configs_payload(&settings, &gateway_url)
            .and_then(|catalog| catalog::rewrite_model_configs(&catalog, &settings, &gateway_url))
        {
            Ok(catalog) => catalog,
            Err(error) => {
                return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        return protocol_success_response(&body, &catalog);
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
        return protocol_success_response(&body, &response);
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
                format!("cursor-byok model hash not found: {model_hash}"),
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
    let mut provider_stream = state.provider.stream(invocation, cancellation);
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
    streaming_response(stream)
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

    let mut outgoing = state.upstream.request(
        reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
            .unwrap_or(reqwest::Method::POST),
        &target,
    );
    for (name, value) in parts.headers.iter() {
        if matches!(
            name.as_str(),
            "host" | "connection" | "content-length" | "transfer-encoding" | "accept-encoding"
        ) {
            continue;
        }
        outgoing = outgoing.header(name.as_str(), value.as_bytes());
    }
    let bytes = match to_bytes(body, MAX_BODY_SIZE + 5).await {
        Ok(bytes) => bytes,
        Err(error) => return protocol_error(StatusCode::PAYLOAD_TOO_LARGE, error.to_string()),
    };
    let response = match outgoing.body(bytes).send().await {
        Ok(response) => response,
        Err(error) => {
            return protocol_error(
                StatusCode::BAD_GATEWAY,
                format!("Devin upstream request failed: {error}"),
            )
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

fn protocol_success_response(request_body: &[u8], payload: &[u8]) -> Response<Body> {
    let (body, content_type) = if wire::is_connect_envelope(request_body) {
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

fn streaming_response<S>(stream: S) -> Response<Body>
where
    S: futures_util::Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send + 'static,
{
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/connect+proto")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .expect("Devin streaming response")
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
        let state = GatewayState {
            store,
            provider,
            assignments: Arc::new(AssignmentSessions::new()),
            upstream: reqwest::Client::builder().no_proxy().build().unwrap(),
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
}
