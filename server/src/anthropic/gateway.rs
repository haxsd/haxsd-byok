//! Claude Code 的独立回环监听：启停、端口、令牌与并发边界。
//!
//! 只监听 `127.0.0.1`。鉴权用网关自己的 Bearer 令牌（也接受 `x-api-key`，Claude Code
//! 两种都可能发），令牌只生成一次并保存在 `service_settings` 里；比较走 SHA-256 摘要，
//! 不泄露长度、不做短路比较。
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    network::NetworkClients,
    provider::{ModelEvent, Provider},
    store::Store,
    Error, Result,
};

use super::{
    messages::{self, MessageAssembler, ResponseTranslator},
    models, ApiError, ClaudeSettings, ClaudeStatus,
};

const MAX_REQUEST_BODY: usize = 24 * 1024 * 1024;
const MAX_REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct ClaudeGateway {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    provider: Arc<dyn Provider>,
    clients: NetworkClients,
    runtime: Mutex<Option<Running>>,
    operations: Mutex<()>,
    last_error: std::sync::Mutex<Option<String>>,
    active: AtomicUsize,
    /// 令牌在库里只生成一次；校验只需要读它，取到之后常驻内存（鉴权失败时重读）。
    token: std::sync::RwLock<Option<String>>,
}

struct Running {
    port: u16,
    shutdown: CancellationToken,
    alive: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

/// 跨流的活动计数；响应 Body 被客户端丢弃时也释放。
struct ActiveRequest {
    gateway: ClaudeGateway,
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.gateway.inner.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ClaudeGateway {
    pub fn new(store: Store, provider: Arc<dyn Provider>, clients: NetworkClients) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                provider,
                clients,
                runtime: Mutex::new(None),
                operations: Mutex::new(()),
                last_error: std::sync::Mutex::new(None),
                active: AtomicUsize::new(0),
                token: std::sync::RwLock::new(None),
            }),
        }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route(
                "/health",
                get(|| async { Json(serde_json::json!({"ok": true, "service": "claude"})) }),
            )
            .route("/api/hello", get(hello))
            .route("/v1/messages", post(messages_handler))
            .route("/v1/messages/count_tokens", post(count_tokens_handler))
            .route("/v1/models", get(models_handler))
            .with_state(self.clone())
    }

    fn record_error(&self, error: Option<String>) {
        *self
            .inner
            .last_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = error;
    }

    pub async fn start(&self) -> Result<()> {
        let _operation = self.inner.operations.lock().await;
        let settings = self.inner.store.claude_settings().await?;
        self.sync_listener(&settings).await
    }

    /// 令牌缓存：每个请求都要校验令牌，取库要过全局写锁，所以取到之后常驻内存。
    async fn token(&self) -> Result<String> {
        if let Some(token) = self
            .inner
            .token
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
        {
            return Ok(token);
        }
        self.reload_token().await
    }

    /// 重读（必要时重新生成）令牌并刷新缓存。
    ///
    /// 缓存会让"库里的令牌已经换掉"这件事在内存里看不出来；鉴权失败时再读一次，
    /// 客户端拿着新令牌重试就能通过。主动轮换见 [`ClaudeGateway::rotate_token`]。
    async fn reload_token(&self) -> Result<String> {
        let token = self.inner.store.claude_token().await?;
        *self
            .inner
            .token
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(token.clone());
        Ok(token)
    }

    /// 轮换网关令牌：写库 + 刷新缓存，旧令牌立即失效。
    pub async fn rotate_token(&self) -> Result<String> {
        let _operation = self.inner.operations.lock().await;
        let token = self.inner.store.rotate_claude_token().await?;
        *self
            .inner
            .token
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(token.clone());
        Ok(token)
    }

    async fn sync_listener(&self, settings: &ClaudeSettings) -> Result<()> {
        let mut runtime = self.inner.runtime.lock().await;
        if settings.enabled
            && runtime.as_ref().is_some_and(|running| {
                running.port == settings.port && running.alive.load(Ordering::SeqCst)
            })
        {
            return Ok(());
        }
        // 先绑定新端口：失败时旧监听继续服务，设置也没有被改写。
        let listener = if settings.enabled {
            match TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], settings.port))).await {
                Ok(listener) => Some(listener),
                Err(error) => {
                    self.record_error(Some(error.to_string()));
                    return Err(error.into());
                }
            }
        } else {
            None
        };
        if let Some(previous) = runtime.take() {
            previous.shutdown.cancel();
            let mut task = previous.task;
            match tokio::time::timeout(Duration::from_secs(10), &mut task).await {
                Ok(_) => {}
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    tracing::warn!("Claude listener shutdown timed out");
                }
            }
        }
        if let Some(listener) = listener {
            let shutdown = CancellationToken::new();
            let graceful = shutdown.clone();
            let alive = Arc::new(AtomicBool::new(true));
            let serving = alive.clone();
            let router = self.router();
            let task = tokio::spawn(async move {
                if let Err(error) = axum::serve(listener, router)
                    .with_graceful_shutdown(graceful.cancelled_owned())
                    .await
                {
                    tracing::error!(%error, "Claude listener stopped");
                }
                serving.store(false, Ordering::SeqCst);
            });
            *runtime = Some(Running {
                port: settings.port,
                shutdown,
                alive,
                task,
            });
        }
        self.record_error(None);
        Ok(())
    }

    /// 当前正在监听的端口（自己有监听器时才有值）。保存同一个端口时它算我们自己的
    /// 监听器，不是"被别的程序占用"。
    pub async fn listening_port(&self) -> Option<u16> {
        let runtime = self.inner.runtime.lock().await;
        runtime
            .as_ref()
            .filter(|running| running.alive.load(Ordering::SeqCst))
            .map(|running| running.port)
    }

    pub async fn save_settings(&self, settings: ClaudeSettings) -> Result<ClaudeSettings> {
        let _operation = self.inner.operations.lock().await;
        settings.validate()?;
        if self.inner.active.load(Ordering::SeqCst) != 0 {
            return Err(Error::Config(
                "Claude has active requests; wait before changing its configuration".into(),
            ));
        }
        let previous = self.inner.store.claude_settings().await?;
        let saved = self.inner.store.set_claude_settings(settings).await?;
        if let Err(error) = self.sync_listener(&saved).await {
            self.inner.store.set_claude_settings(previous).await?;
            return Err(error);
        }
        Ok(saved)
    }

    pub async fn status(&self) -> Result<ClaudeStatus> {
        let settings = self.inner.store.claude_settings().await?;
        let models = models::catalog(&self.inner.store, &settings)
            .await
            .map_err(|error| Error::Store(error.message))?;
        let runtime = self.inner.runtime.lock().await;
        let listening = runtime.as_ref().is_some_and(|running| {
            running.port == settings.port && running.alive.load(Ordering::SeqCst)
        });
        drop(runtime);
        let calls: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM llm_calls WHERE client_source = 'claude'")
                .fetch_one(self.inner.store.pool())
                .await?;
        Ok(ClaudeStatus {
            enabled: settings.enabled,
            listening,
            port: settings.port,
            base_url: format!("http://127.0.0.1:{}", settings.port),
            token: self.token().await?,
            models: models.len(),
            default_model_hash: settings.default_model_hash.clone(),
            calls: calls.max(0) as usize,
            last_error: self
                .inner
                .last_error
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone(),
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _operation = self.inner.operations.lock().await;
        let mut runtime = self.inner.runtime.lock().await;
        if let Some(running) = runtime.take() {
            // 网关不写用户文件，退出时没有必须等待的清理：直接断开监听，别让一条
            // 正在生成的流把进程退出拖满排水预算。
            running.shutdown.cancel();
            running.task.abort();
            let _ = running.task.await;
        }
        Ok(())
    }
}

async fn hello() -> impl IntoResponse {
    "hello"
}

/// 校验网关令牌。失败时重读一次库里的令牌再判：删掉那一行是文档里写明的轮换方式，
/// 而缓存会让旧令牌继续通过校验（见 `ClaudeGateway::token`）。
async fn authorize(gateway: &ClaudeGateway, headers: &HeaderMap) -> Result<(), ApiError> {
    let expected = gateway
        .token()
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    if authorized(headers, &expected) {
        return Ok(());
    }
    let expected = gateway
        .reload_token()
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    if authorized(headers, &expected) {
        return Ok(());
    }
    Err(ApiError::authentication("invalid Claude gateway token"))
}

async fn messages_handler(
    State(gateway): State<ClaudeGateway>,
    request: Request<Body>,
) -> Response {
    if let Err(error) = authorize(&gateway, request.headers()).await {
        return error.into_response();
    }
    // 活动计数从鉴权之后就开始：读正文、解析请求、准备图片也是这次请求的生命周期，
    // 配置修改必须等它结束。计数随 guard 一起进入流，客户端断开时一并释放。
    gateway.inner.active.fetch_add(1, Ordering::SeqCst);
    let active = ActiveRequest {
        gateway: gateway.clone(),
    };
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let settings = match gateway.inner.store.claude_settings().await {
        Ok(settings) => settings,
        Err(error) => return ApiError::from_error(&error).into_response(),
    };
    if !settings.enabled {
        return ApiError::unavailable("Claude gateway is disabled").into_response();
    }
    let prepared = match messages::prepare(
        &gateway.inner.store,
        &settings,
        &body,
        &gateway.inner.clients,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => return error.into_response(),
    };
    let cancellation = gateway
        .inner
        .runtime
        .lock()
        .await
        .as_ref()
        .map(|running| running.shutdown.clone())
        .unwrap_or_default();
    let request_id = format!("req-{}", uuid::Uuid::new_v4());
    if prepared.stream {
        streamed(&gateway, prepared, cancellation, request_id, active).await
    } else {
        collected(&gateway, prepared, cancellation, request_id, active).await
    }
}

/// 流式：第一个上游事件决定 HTTP 状态，之后只能靠 SSE 的 `error` 事件收尾。
async fn streamed(
    gateway: &ClaudeGateway,
    prepared: messages::PreparedRequest,
    cancellation: CancellationToken,
    request_id: String,
    active: ActiveRequest,
) -> Response {
    let stop_sequences = prepared.invocation.request.model.stop_sequences.clone();
    let model = prepared.requested_model.clone();
    // `message_start` 的输入用量在权威 usage 到达之前就要有值；用与
    // `/count_tokens` 相同的共享估算器，`message_delta` 随后用真实用量覆盖它。
    let estimated_input_tokens = crate::model::estimate_context_tokens(
        &prepared.invocation.request.prompt,
        &prepared.invocation.request.history,
    );
    let mut stream = gateway
        .inner
        .provider
        .stream(prepared.invocation, cancellation);

    // 连接、鉴权、限流这类失败还能用非 2xx 回给客户端（Claude Code 会据此重试）；
    // 一旦发出 200 就只能用 SSE 的 error 事件。
    let first = match stream.next().await {
        Some(Ok(event)) => event,
        Some(Err(error)) => return provider_failure(gateway, &error),
        None => {
            let error = Error::Provider("provider stream ended without any event".into());
            return provider_failure(gateway, &error);
        }
    };

    let message_id = format!("msg-{}", uuid::Uuid::new_v4());
    let error_gateway = gateway.clone();
    let output = async_stream::stream! {
        let _active = active;
        let mut translator =
            ResponseTranslator::new(message_id, model, stop_sequences, estimated_input_tokens);
        yield Ok::<_, std::convert::Infallible>(messages::sse_bytes(&translator.start()));
        let mut pending = Some(first);
        loop {
            let next = match pending.take() {
                Some(event) => Some(Ok(event)),
                None => match tokio::time::timeout(messages::PING_INTERVAL, stream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        yield Ok(messages::sse_bytes(&serde_json::json!({"type": "ping"})));
                        continue;
                    }
                },
            };
            match next {
                Some(Ok(event)) => {
                    let done = matches!(event, ModelEvent::Done(_));
                    for value in translator.push(event) {
                        yield Ok(messages::sse_bytes(&value));
                    }
                    if done {
                        for value in translator.finish() {
                            yield Ok(messages::sse_bytes(&value));
                        }
                        return;
                    }
                }
                Some(Err(error)) => {
                    error_gateway.record_error(Some(error.to_string()));
                    yield Ok(messages::sse_bytes(&translator.fail(&error)));
                    return;
                }
                None => {
                    let error = Error::Provider("provider stream ended without Done".into());
                    error_gateway.record_error(Some(error.to_string()));
                    yield Ok(messages::sse_bytes(&translator.fail(&error)));
                    return;
                }
            }
        }
    };
    let mut response = (
        [
            (
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/event-stream"),
            ),
            (
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-cache"),
            ),
        ],
        Body::from_stream(output),
    )
        .into_response();
    set_request_id(&mut response, &request_id);
    response
}

/// 非流式：先把整条上游事件流收完，再一次性写出完整消息。
///
/// 调用方（Claude Code 的 compaction 重发、Desktop 的连通性测试）要的是一份
/// Anthropic `message` 文档，但上游共享接口只有流；这里复用同一个 translator，
/// 保证停止序列、思考签名与 stop_reason 的语义和流式完全一致。
///
/// 错误永远发生在写出第一个字节之前，所以非流式路径总是能用 HTTP 状态码回答——
/// 包括"已经收到部分文本之后上游才失败"的情况。
async fn collected(
    gateway: &ClaudeGateway,
    prepared: messages::PreparedRequest,
    cancellation: CancellationToken,
    request_id: String,
    active: ActiveRequest,
) -> Response {
    let _active = active;
    let stop_sequences = prepared.invocation.request.model.stop_sequences.clone();
    let estimated_input_tokens = crate::model::estimate_context_tokens(
        &prepared.invocation.request.prompt,
        &prepared.invocation.request.history,
    );
    let mut translator = ResponseTranslator::new(
        format!("msg-{}", uuid::Uuid::new_v4()),
        prepared.requested_model,
        stop_sequences,
        estimated_input_tokens,
    );
    let mut assembler = MessageAssembler::new();
    if let Err(error) = assembler.push(&translator.start()) {
        return error.into_response();
    }
    let mut stream = gateway
        .inner
        .provider
        .stream(prepared.invocation, cancellation);
    loop {
        let event = match stream.next().await {
            Some(Ok(event)) => event,
            Some(Err(error)) => return provider_failure(gateway, &error),
            None => {
                let error = Error::Provider("provider stream ended without Done".into());
                return provider_failure(gateway, &error);
            }
        };
        let done = matches!(event, ModelEvent::Done(_));
        for value in translator.push(event) {
            if let Err(error) = assembler.push(&value) {
                return error.into_response();
            }
        }
        if !done {
            continue;
        }
        for value in translator.finish() {
            if let Err(error) = assembler.push(&value) {
                return error.into_response();
            }
        }
        let message = match assembler.finish() {
            Ok(message) => message,
            Err(error) => return error.into_response(),
        };
        let mut response = Json(message).into_response();
        set_request_id(&mut response, &request_id);
        return response;
    }
}

fn provider_failure(gateway: &ClaudeGateway, error: &Error) -> Response {
    gateway.record_error(Some(error.to_string()));
    ApiError::from_error(error).into_response()
}

fn set_request_id(response: &mut Response, request_id: &str) {
    if let Ok(value) = header::HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert(header::HeaderName::from_static("request-id"), value);
    }
}

async fn count_tokens_handler(
    State(gateway): State<ClaudeGateway>,
    request: Request<Body>,
) -> Response {
    if let Err(error) = authorize(&gateway, request.headers()).await {
        return error.into_response();
    }
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let settings = match gateway.inner.store.claude_settings().await {
        Ok(settings) => settings,
        Err(error) => return ApiError::from_error(&error).into_response(),
    };
    if !settings.enabled {
        return ApiError::unavailable("Claude gateway is disabled").into_response();
    }
    match messages::count_tokens(
        &gateway.inner.store,
        &settings,
        &body,
        &gateway.inner.clients,
    )
    .await
    {
        // `estimated` 是网关自己的标记：共享估算器不是精确分词器，客户端接受这个
        // 数字，但界面不该把它当成计费口径。
        Ok(tokens) => Json(serde_json::json!({
            "input_tokens": tokens,
            "estimated": true,
        }))
        .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn models_handler(State(gateway): State<ClaudeGateway>, request: Request<Body>) -> Response {
    if let Err(error) = authorize(&gateway, request.headers()).await {
        return error.into_response();
    }
    let settings = match gateway.inner.store.claude_settings().await {
        Ok(settings) => settings,
        Err(error) => return ApiError::from_error(&error).into_response(),
    };
    match models::catalog(&gateway.inner.store, &settings).await {
        Ok(models) => Json(models::catalog_json(&models)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn read_body(request: Request<Body>) -> Result<Value, ApiError> {
    let bytes = match tokio::time::timeout(
        MAX_REQUEST_BODY_TIMEOUT,
        to_bytes(request.into_body(), MAX_REQUEST_BODY),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return Err(ApiError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                kind: "request_too_large",
                message: format!("request body exceeds {MAX_REQUEST_BODY} bytes"),
                should_retry: false,
                retry_after: None,
            })
        }
        Err(_) => {
            return Err(ApiError {
                status: StatusCode::REQUEST_TIMEOUT,
                kind: "api_error",
                message: "request body was not received in time".into(),
                should_retry: true,
                retry_after: None,
            })
        }
    };
    serde_json::from_slice(&bytes)
        .map_err(|error| ApiError::invalid(format!("request body is not valid JSON: {error}")))
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim());
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    [bearer, api_key]
        .into_iter()
        .flatten()
        .any(|candidate| token_matches(candidate, expected))
}

/// 摘要比较：两个输入先各自哈希成 32 字节，再逐字节累积差异。长度不参与比较，
/// 也不会在第一个不同字节处提前返回。
fn token_matches(candidate: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let candidate = Sha256::digest(candidate.as_bytes());
    let expected = Sha256::digest(expected.as_bytes());
    let mut difference = 0_u8;
    for (left, right) in candidate.iter().zip(expected.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        anthropic::ClaudeSettings,
        model::{ModelConfigInput, ModelType},
        provider::{FinishReason, ModelEvent, Provider, ProviderStream},
        store::Store,
    };
    use futures_util::stream;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    struct ScriptedProvider {
        responses: StdMutex<VecDeque<Vec<crate::Result<ModelEvent>>>>,
    }

    impl ScriptedProvider {
        fn with_results(results: Vec<crate::Result<ModelEvent>>) -> Self {
            let mut responses = VecDeque::new();
            responses.push_back(results);
            Self {
                responses: StdMutex::new(responses),
            }
        }
    }

    impl Provider for ScriptedProvider {
        fn stream(
            &self,
            _invocation: crate::model::ModelInvocation,
            _cancellation: CancellationToken,
        ) -> ProviderStream {
            let events = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            Box::pin(stream::iter(events))
        }
    }

    async fn gateway() -> (tempfile::TempDir, Store, ClaudeGateway, String) {
        gateway_with(vec![
            ModelEvent::Start {
                model_call_id: "call".into(),
            },
            ModelEvent::TextStart,
            ModelEvent::TextDelta("hello".into()),
            ModelEvent::TextEnd,
            ModelEvent::Usage(crate::model::Usage {
                input_tokens: Some(3),
                context_input_tokens: Some(3),
                output_tokens: Some(1),
                total_tokens: Some(4),
                ..Default::default()
            }),
            ModelEvent::Done(FinishReason::Stop),
        ])
        .await
    }

    async fn gateway_with(
        events: Vec<ModelEvent>,
    ) -> (tempfile::TempDir, Store, ClaudeGateway, String) {
        let results = events.into_iter().map(Ok).collect();
        gateway_with_results(results).await
    }

    async fn gateway_with_results(
        results: Vec<crate::Result<ModelEvent>>,
    ) -> (tempfile::TempDir, Store, ClaudeGateway, String) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("claude-gateway.db").display()
        ))
        .await
        .unwrap();
        let model = store
            .create_model(&ModelConfigInput {
                sort_order: 0,
                display_name: "Claude".into(),
                group_name: None,
                model_type: ModelType::Anthropic,
                base_url: "https://api.anthropic.com".into(),
                use_full_url: false,
                api_key: "key".into(),
                tooltip_data: "Claude".into(),
                model_id: "claude-sonnet-4".into(),
                reasoning_effort: None,
                openai_endpoint: String::new(),
                openai_extra_params_enabled: false,
                openai_extra_params: serde_json::json!({}),
                custom_headers_enabled: false,
                custom_headers: serde_json::json!({}),
                anthropic_extra_params_enabled: false,
                anthropic_extra_params: serde_json::json!({}),
                context_window_tokens: Some(200_000),
                max_completion_tokens: None,
                anthropic_max_tokens: Some(8_192),
                anthropic_thinking_effort: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        store
            .set_claude_settings(ClaudeSettings {
                enabled: true,
                port,
                default_model_hash: Some(model.model_hash.clone()),
                ..ClaudeSettings::default()
            })
            .await
            .unwrap();
        let token = store.claude_token().await.unwrap();
        let provider = Arc::new(ScriptedProvider::with_results(results)) as Arc<dyn Provider>;
        let gateway =
            ClaudeGateway::new(store.clone(), provider, NetworkClients::new(store.clone()));
        (directory, store, gateway, token)
    }

    fn request(token: &str) -> Request<Body> {
        Request::post("/v1/messages")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 1_024,
                    "stream": true,
                    "messages": [{"role": "user", "content": "hi"}],
                })
                .to_string(),
            ))
            .unwrap()
    }

    fn non_stream_request(token: &str) -> Request<Body> {
        Request::post("/v1/messages")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 1_024,
                    "stream": false,
                    "system": "be brief",
                    "messages": [{"role": "user", "content": "hi"}],
                })
                .to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn a_wrong_token_is_rejected_before_any_provider_call() {
        let (_directory, _store, gateway, token) = gateway().await;
        use tower::ServiceExt;
        let wrong = token.clone() + "-wrong";
        let response = gateway.router().oneshot(request(&wrong)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["type"], "authentication_error");
    }

    #[tokio::test]
    async fn a_good_token_reaches_the_provider_and_returns_sse() {
        let (_directory, _store, gateway, token) = gateway().await;
        use tower::ServiceExt;
        let response = gateway.router().oneshot(request(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        for event in [
            "message_start",
            "content_block_start",
            "text_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ] {
            assert!(text.contains(event), "missing {event}: {text}");
        }
    }

    #[tokio::test]
    async fn the_token_is_generated_once_and_never_serialized_with_settings() {
        let (_directory, store, gateway, token) = gateway().await;
        assert!(token.len() >= 32);
        assert_eq!(store.claude_token().await.unwrap(), token);
        let serialized = serde_json::to_string(&store.claude_settings().await.unwrap()).unwrap();
        assert!(!serialized.contains(&token));
        assert!(gateway.token().await.unwrap() == token);
    }

    #[tokio::test]
    async fn disabled_gateways_refuse_messages_but_still_answer_health() {
        let (_directory, store, gateway, token) = gateway().await;
        let mut settings = store.claude_settings().await.unwrap();
        settings.enabled = false;
        store.set_claude_settings(settings).await.unwrap();

        use tower::ServiceExt;
        let response = gateway.router().oneshot(request(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let response = gateway
            .router()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn model_discovery_hello_and_count_tokens_are_served() {
        let (_directory, store, gateway, token) = gateway().await;
        use tower::ServiceExt;

        let response = gateway
            .router()
            .oneshot(
                Request::get("/v1/models")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let discovery: Value = serde_json::from_slice(&body).unwrap();
        let hash = store
            .claude_settings()
            .await
            .unwrap()
            .default_model_hash
            .unwrap();
        // Claude Code 与 Desktop 的发现过滤器只保留 id 含 `claude` 的条目。
        let expected = String::from("claude-") + &hash;
        assert_eq!(discovery["data"][0]["id"], expected);
        assert_eq!(discovery["data"][0]["type"], "model");
        assert_eq!(discovery["first_id"], expected);
        assert_eq!(discovery["last_id"], expected);

        let response = gateway
            .router()
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-sonnet-4-5",
                            "messages": [{"role": "user", "content": "hello"}],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let counted: Value = serde_json::from_slice(&body).unwrap();
        assert!(counted["input_tokens"].as_u64().unwrap() > 0);
        assert_eq!(counted["estimated"], true);
    }

    /// Axum 的 `get()` 同时匹配 HEAD：路由层执行 GET handler，先按响应体的确切长度
    /// 写 `content-length`，再把 body 换成空。这里确认路由命中、状态码与这个长度。
    #[tokio::test]
    async fn head_api_hello_is_served_by_the_get_route() {
        let (_directory, _store, gateway, _token) = gateway().await;
        use tower::ServiceExt;
        let response = gateway
            .router()
            .oneshot(Request::head("/api/hello").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "5");
        let body = to_bytes(response.into_body(), 64).await.unwrap();
        assert!(body.is_empty(), "HEAD 响应不带 body");
    }

    #[tokio::test]
    async fn a_non_streaming_request_returns_one_complete_message() {
        let (_directory, _store, gateway, token) = gateway().await;
        use tower::ServiceExt;
        let response = gateway
            .router()
            .oneshot(non_stream_request(&token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert!(response.headers().contains_key("request-id"));

        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let message: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["model"], "claude-sonnet-4-5");
        assert!(message["id"].as_str().unwrap().starts_with("msg-"));
        assert_eq!(message["content"][0]["type"], "text");
        assert_eq!(message["content"][0]["text"], "hello");
        assert_eq!(message["stop_reason"], "end_turn");
        assert!(message["stop_sequence"].is_null());
        assert_eq!(message["usage"]["input_tokens"], 3);
        assert_eq!(message["usage"]["output_tokens"], 1);
    }

    /// 非流式在写出第一个字节之前就拿到了错误，所以即使已经收到部分文本也能用 HTTP
    /// 状态码收尾——Claude Code 据此重试，而不是把半截消息当成成功。
    #[tokio::test]
    async fn a_non_streaming_failure_after_partial_content_is_an_http_error() {
        let (_directory, _store, gateway, token) = gateway_with_results(vec![
            Ok(ModelEvent::Start {
                model_call_id: "call".into(),
            }),
            Ok(ModelEvent::TextStart),
            Ok(ModelEvent::TextDelta("partial".into())),
            Err(Error::Provider("Anthropic 529 Overloaded: busy".into())),
        ])
        .await;
        use tower::ServiceExt;
        let response = gateway
            .router()
            .oneshot(non_stream_request(&token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::from_u16(529).unwrap());
        assert_eq!(response.headers()["x-should-retry"], "true");
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["type"], "error");
        assert_eq!(error["error"]["type"], "overloaded_error");
    }

    #[tokio::test]
    async fn discovery_requires_the_gateway_token() {
        let (_directory, _store, gateway, _token) = gateway().await;
        use tower::ServiceExt;
        let response = gateway
            .router()
            .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// 轮换令牌：旧令牌立即失效，缓存同步到新值。
    #[tokio::test]
    async fn rotating_the_token_invalidates_the_cached_one_immediately() {
        let (_directory, store, gateway, token) = gateway().await;
        assert_eq!(gateway.token().await.unwrap(), token, "先把旧令牌装进缓存");

        let rotated = gateway.rotate_token().await.unwrap();
        assert_ne!(rotated, token);
        assert_eq!(store.claude_token().await.unwrap(), rotated);

        use tower::ServiceExt;
        let response = gateway.router().oneshot(request(&token)).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "旧令牌必须立即失效"
        );
        let response = gateway.router().oneshot(request(&rotated)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "新令牌立即可用");
        assert_eq!(gateway.token().await.unwrap(), rotated);
    }

    /// 库里的令牌被外部换掉（例如另一个进程轮换）时，带新令牌的请求会重读并刷新
    /// 缓存，不会因为内存里还是旧值而一直 401。
    #[tokio::test]
    async fn a_rejected_request_reloads_a_token_replaced_behind_the_cache() {
        let (_directory, store, gateway, token) = gateway().await;
        assert_eq!(gateway.token().await.unwrap(), token, "先把旧令牌装进缓存");
        let replaced = store.rotate_claude_token().await.unwrap();
        assert_ne!(replaced, token);

        use tower::ServiceExt;
        let response = gateway.router().oneshot(request(&replaced)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "重读之后新令牌必须能用");
        assert_eq!(gateway.token().await.unwrap(), replaced);
    }

    /// 活动计数必须在 prepare（读正文、下载图片）期间就生效：否则配置修改会在
    /// 一轮请求的准备阶段穿过检查，而那次请求马上要用旧配置发出去。
    #[tokio::test]
    async fn the_active_count_covers_request_preparation() {
        use axum::{http::header, routing::get, Router as AxumRouter};
        use std::sync::Arc;
        let (_directory, store, gateway, token) = gateway().await;

        // 一个"卡住"的图片源：请求到达时通知测试，等测试放行才返回。
        let hit = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let app = AxumRouter::new().route(
            "/slow.png",
            get({
                let hit = hit.clone();
                let release = release.clone();
                move || {
                    let hit = hit.clone();
                    let release = release.clone();
                    async move {
                        hit.notify_one();
                        release.notified().await;
                        ([(header::CONTENT_TYPE, "image/png")], "png-bytes")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url = String::from("http://") + &address.to_string() + "/slow.png";
        let pending = tokio::spawn({
            let gateway = gateway.clone();
            let token = token.clone();
            async move {
                use tower::ServiceExt;
                gateway
                    .router()
                    .oneshot(
                        Request::post("/v1/messages")
                            .header(header::AUTHORIZATION, format!("Bearer {token}"))
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                serde_json::json!({
                                    "model": "claude-sonnet-4-5",
                                    "max_tokens": 1_024,
                                    "messages": [{"role": "user", "content": [
                                        {"type": "image", "source": {"type": "url", "url": url}},
                                    ]}],
                                })
                                .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
            }
        });

        hit.notified().await;
        assert_eq!(
            gateway.inner.active.load(Ordering::SeqCst),
            1,
            "准备阶段的请求必须计入活动数"
        );
        let settings = store.claude_settings().await.unwrap();
        assert!(
            gateway.save_settings(settings).await.is_err(),
            "有请求在准备时不许改配置"
        );

        release.notify_one();
        let response = pending.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(
            gateway.inner.active.load(Ordering::SeqCst),
            0,
            "请求结束后计数必须归零"
        );
    }
}
