//! 通往模型上游的统一发送路径：一次尝试，外加对"还没有拿到任何响应内容"的瞬时
//! 失败的受限重试。
//!
//! 三条上游路径都在这里出网：Cursor 的通用 provider 流、Devin 的模型调用（两者都
//! 经 provider 适配器）与 Codex 的 Responses 直通。重试只覆盖上游明确还没有开始
//! 作答的失败——发送阶段的连接类错误（连接被拒、TLS、DNS），或网关自己的
//! 502/503/504——因为一旦响应字节开始流向客户端，再发一次就意味着重复生成。
//!
//! 不重试：4xx（含 429，交给既有逻辑）、已经开始读响应体之后的中断、发送中途断开、
//! 请求级超时（上游一直不回话，重试只会把一次超时预算翻几倍）与取消。

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::{network::OutboundClient, provider::CallRecorder, Error, Result};

/// 首次尝试之外允许的重试次数。
const MAX_RETRIES: u32 = 2;

/// 每次重试前的等待：第 1 次 ~300 ms，第 2 次 ~1 s。指数退避够不上这么多轮，
/// 固定值让最坏情况的总延迟可预期。
const RETRY_BACKOFF: [Duration; MAX_RETRIES as usize] =
    [Duration::from_millis(300), Duration::from_secs(1)];

/// 一次上游请求的身份：重试日志与最终错误文案都用它说明"谁、打到哪里"。
pub(crate) struct UpstreamCall<'a> {
    /// 路径名：进重试日志，也作为最终错误文案的前缀（如 `OpenAI Chat`）。
    pub label: &'a str,
    /// 模型 id（官方转发这类没有模型维度的路径留空），只进日志。
    pub model: &'a str,
    /// 上游地址，只进日志。
    pub url: &'a str,
}

/// 一次上游请求的结局。非成功状态与发送失败都交给调用方成文：Codex 直通要按
/// 自己的文案与状态码回给客户端，provider 适配器走 [`send`] 的统一文案。
pub(crate) enum Attempt {
    /// 上游接受了请求（2xx）；响应体由调用方读取。
    Response(reqwest::Response),
    /// 上游给出了非成功状态；响应体还没读，`retries` 是已经用掉的次数。
    Rejected {
        response: reqwest::Response,
        retries: u32,
    },
    /// 发送阶段失败（连接/TLS/发送）；`retries` 是已经用掉的次数。
    Failed { error: reqwest::Error, retries: u32 },
    /// 被取消：没有发出（或不再发出）请求。
    Cancelled,
}

/// 发送一次上游请求，对瞬时失败自动重试。
///
/// `build` 每次尝试都会被重新调用：请求体必须可重放（当前三条路径发的都是
/// 一次构造好的 JSON）。
pub(crate) async fn attempt<F>(
    call: &UpstreamCall<'_>,
    outbound: &OutboundClient,
    build: F,
    cancellation: &CancellationToken,
    recorder: Option<&CallRecorder>,
) -> Result<Attempt>
where
    F: Fn(&reqwest::Client) -> reqwest::RequestBuilder,
{
    let mut client = outbound.client().clone();
    // 出网路由（代理设置）在运行期间会变，缓存里的客户端可能已经指向没人监听的
    // 代理端口：第一连接类失败时按当前配置重建一次。它和瞬时失败重试共用同一份
    // 次数预算，所以一次请求最多发出 MAX_RETRIES + 1 次。
    let mut recovery_allowed = true;
    let mut retries = 0_u32;
    loop {
        let sent = tokio::select! {
            _ = cancellation.cancelled() => return Ok(Attempt::Cancelled),
            sent = build(&client).send() => sent,
        };
        let response = match sent {
            Ok(response) => response,
            Err(error) => {
                // 重建客户端仍然对超时做：它刷新的是缓存里那条路由（下一次请求就不必
                // 再撞同一堵墙），但"要不要重发"由 `retryable_send_failure` 单独决定。
                let rebuilt = if recovery_allowed {
                    recovery_allowed = false;
                    outbound.rebuild_after(&error).await
                } else {
                    None
                };
                let retryable = retryable_send_failure(&error);
                let Some(backoff) = retry_backoff(retryable, retries) else {
                    return Ok(Attempt::Failed { error, retries });
                };
                if let Some(rebuilt) = rebuilt {
                    client = rebuilt;
                }
                if !wait_before_retry(call, retries, &error.to_string(), backoff, cancellation)
                    .await
                {
                    return Ok(Attempt::Cancelled);
                }
                retries += 1;
                continue;
            }
        };
        if response.status().is_success() {
            record_headers(recorder, &response).await;
            return Ok(Attempt::Response(response));
        }
        if retryable_status(response.status()) {
            if let Some(backoff) = retry_backoff(true, retries) {
                let reason = format!("HTTP {}", response.status());
                // 不读正文：这次尝试的内容不会被任何人看到，连接直接放掉。
                drop(response);
                if !wait_before_retry(call, retries, &reason, backoff, cancellation).await {
                    return Ok(Attempt::Cancelled);
                }
                retries += 1;
                continue;
            }
        }
        record_headers(recorder, &response).await;
        return Ok(Attempt::Rejected { response, retries });
    }
}

/// provider 适配器共用的发送：成功给出 2xx 响应，`None` 表示收到取消。
///
/// 非成功状态与重试后的发送失败都已经转成带文案的错误：
/// `{label} 502 Bad Gateway (after 2 retries): <上游正文>`。
pub(crate) async fn send<F>(
    call: &UpstreamCall<'_>,
    outbound: &OutboundClient,
    build: F,
    cancellation: &CancellationToken,
    recorder: Option<&CallRecorder>,
) -> Result<Option<reqwest::Response>>
where
    F: Fn(&reqwest::Client) -> reqwest::RequestBuilder,
{
    match attempt(call, outbound, build, cancellation, recorder).await? {
        Attempt::Response(response) => Ok(Some(response)),
        Attempt::Cancelled => Ok(None),
        Attempt::Rejected { response, retries } => {
            let status = response.status();
            let body = tokio::select! {
                _ = cancellation.cancelled() => return Ok(None),
                body = read_rejected_body(response) => body,
            };
            Err(Error::Provider(rejected_message(
                call.label, status, retries, &body,
            )))
        }
        Attempt::Failed { error, retries } => Err(send_failure(call.label, error, retries)),
    }
}

/// 错误正文的读取上限。这段文字只是给人看的,却会同时进日志、`llm_calls` 的错误字段和
/// 界面文案;上游"回了 4xx 之后慢慢滴字节"时,原来的 `response.bytes()` 会一直读到客户端
/// 总超时(默认 1 小时),内存也跟着无界增长。
const REJECTED_BODY_LIMIT: usize = 64 * 1024;

/// 单次读取的空闲上界,独立于客户端总超时:慢滴字节拖不住它。
const REJECTED_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// 读非成功响应体:读满上限或空闲超时就停下,被截断时在正文尾部标出来。
async fn read_rejected_body(mut response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let chunk = match tokio::time::timeout(REJECTED_BODY_IDLE_TIMEOUT, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            // 读完、读失败或空闲超时:都用已经读到的部分,不把整条错误链弄丢。
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return body,
        };
        if body.len() + chunk.len() > REJECTED_BODY_LIMIT {
            body.extend_from_slice(&chunk[..REJECTED_BODY_LIMIT - body.len()]);
            body.extend_from_slice(" …<truncated>".as_bytes());
            return body;
        }
        body.extend_from_slice(&chunk);
    }
}

/// 非成功状态的最终文案：`{label} 502 Bad Gateway (after 2 retries): <正文>`。
///
/// 正文为空时连冒号一起省略，界面拿到的是完整的一句话而不是悬空的标点。
pub(crate) fn rejected_message(
    label: &str,
    status: reqwest::StatusCode,
    retries: u32,
    body: &[u8],
) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    let note = retry_note(retries);
    if text.is_empty() {
        format!("{label} {status}{note}")
    } else {
        format!("{label} {status}{note}: {text}")
    }
}

/// 发送阶段失败后的最终错误。
///
/// 没有重试过就原样返回传输层错误：错误链里带着 URL 与底层原因，既有逻辑（请求
/// 超时的友好文案、连接错误的诊断）都按 `Error::Http` 识别它。重试过就把次数写进
/// 文案，界面据此知道这不是第一次尝试的偶发结果。
pub(crate) fn send_failure(label: &str, error: reqwest::Error, retries: u32) -> Error {
    if retries == 0 {
        return Error::Http(error);
    }
    Error::Provider(format!(
        "{label} upstream request failed{}: {error}",
        retry_note(retries)
    ))
}

/// `(after 2 retries)`；没有重试过就是空串。
pub(crate) fn retry_note(retries: u32) -> String {
    if retries == 0 {
        String::new()
    } else {
        format!(" (after {retries} retries)")
    }
}

/// 记下最终响应的状态码；中间那些被重试掉的尝试不写记录。
///
/// 写库失败只留一条 warn：记录是观测，用户要的是这一轮回答——把已经拿到的 2xx 响应
/// 因为"状态码没记上"变成错误，等于让观测反过来决定产品行为。
async fn record_headers(recorder: Option<&CallRecorder>, response: &reqwest::Response) {
    let Some(recorder) = recorder else {
        return;
    };
    if let Err(error) = recorder.response_headers(response.status().as_u16()).await {
        tracing::warn!(
            status = response.status().as_u16(),
            %error,
            "failed to record the upstream response status; keeping the response"
        );
    }
}

/// 上游网关自己给出的临时不可用状态：它明确表示这次没有开始作答。
fn retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 502..=504)
}

/// 发送阶段失败是否值得重试。
///
/// 只认连接类失败（`Kind::Connect`：连接被拒、TLS、DNS 解析失败）——它们确定请求还没
/// 交给上游，重发一次不会让上游重复生成。两种失败明确排除：
/// * 请求级超时（`is_timeout()`）：请求已经发出去了，只是上游没回话；重试既会把一次
///   超时预算翻几倍，也可能让上游再生成一遍（模块头第 9-10 行的承诺）。
/// * 发送中途断开（`is_request()` 但不 `is_connect()`）：请求可能已经送到一半。
fn retryable_send_failure(error: &reqwest::Error) -> bool {
    error.is_connect()
}

/// 这次重试前要等多久；没有预算或失败不值得重试时就该收尾了。
fn retry_backoff(retryable: bool, retries: u32) -> Option<Duration> {
    (retryable && retries < MAX_RETRIES).then(|| RETRY_BACKOFF[retries as usize])
}

/// 退避并留下一条 warn。返回 `false` 表示退避期间被取消，第二次请求不许发出。
async fn wait_before_retry(
    call: &UpstreamCall<'_>,
    retries: u32,
    reason: &str,
    backoff: Duration,
    cancellation: &CancellationToken,
) -> bool {
    tracing::warn!(
        model = call.model,
        url = call.url,
        path = call.label,
        retry = retries + 1,
        max_retries = MAX_RETRIES,
        delay_ms = backoff.as_millis() as u64,
        reason,
        "upstream request failed before any response; retrying"
    );
    tokio::select! {
        _ = cancellation.cancelled() => false,
        _ = tokio::time::sleep(backoff) => true,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use futures_util::StreamExt;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        config::{ProviderConfig, ProviderKind},
        model::{
            CallClient, ModelConfigInput, ModelInvocation, ModelRequest, ModelSpec, ModelType,
            NewLlmCall, PromptSpec, ProviderType, OPENAI_CHAT_ENDPOINT,
        },
        network::NetworkClients,
        provider::{FinishReason, ModelEvent, OpenAiChatProvider, Provider, ProviderStream},
        store::{ProxyMode, ProxySettingsInput, Store},
    };

    /// 上游网关自己的 502：正文就是网关那一层的一段说明。
    const GATEWAY_DOWN: &[u8] =
        b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 4\r\nconnection: close\r\n\r\ndown";
    /// 正常的 JSON 应答。
    const OK_JSON: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
    /// 正常的 SSE 应答：一段文本加一个终止块。
    const OK_SSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n";
    /// 声明了 100 字节正文却只写 5 字节就关连接：响应体读到一半断开。
    const TRUNCATED_BODY: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\nconnection: close\r\n\r\nshort";
    /// 客户端自己的错：不重试。
    const BAD_REQUEST: &[u8] =
        b"HTTP/1.1 400 Bad Request\r\ncontent-length: 4\r\nconnection: close\r\n\r\nnope";

    /// 脚本化的本地上游：第 n 个连接按脚本里的第 n 条应答（越界就重复最后一条），
    /// 写完即关连接，于是连接数就等于尝试次数。
    struct Stub {
        origin: String,
        url: String,
        connections: Arc<AtomicUsize>,
        served: tokio::sync::mpsc::UnboundedReceiver<usize>,
    }

    impl Stub {
        async fn start(script: Vec<&'static [u8]>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let connections = Arc::new(AtomicUsize::new(0));
            let counted = connections.clone();
            let (served, responses) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let index = counted.fetch_add(1, Ordering::SeqCst);
                    let response = script.get(index).or(script.last()).copied().unwrap_or(b"");
                    let mut request = [0_u8; 4096];
                    let _ = socket.read(&mut request).await;
                    let _ = socket.write_all(response).await;
                    let _ = socket.flush().await;
                    let _ = served.send(index);
                    let _ = socket.shutdown().await;
                }
            });
            Self {
                origin: format!("http://{address}"),
                url: format!("http://{address}/v1/chat/completions"),
                connections,
                served: responses,
            }
        }

        fn attempts(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }

        /// 等到第 `index` 条应答已经写出去（脚本下标，第一次连接是 0）。
        async fn wait_for_response(&mut self, index: usize) {
            while let Some(served) = self.served.recv().await {
                if served == index {
                    return;
                }
            }
        }
    }

    /// 回了 4xx 之后一直滴字节的上游：写 chunked 头，然后一块块吐，永远不写结束块。
    ///
    /// "读到客户端总超时"这种故障只能这样复现：响应体既不结束、也不断开。`written()`
    /// 是已经吐出去的正文字节数，用例用它证明"客户端是读满上限就收手"，而不是靠 sleep
    /// 猜时间。
    struct DrippingStub {
        url: String,
        body_bytes: Arc<AtomicUsize>,
    }

    impl DrippingStub {
        /// 每次吐的正文大小：够小才能在截断点前后各有几次读。
        const CHUNK: usize = 8 * 1024;

        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let body_bytes = Arc::new(AtomicUsize::new(0));
            let written = body_bytes.clone();
            tokio::spawn(async move {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await;
                let head = b"HTTP/1.1 400 Bad Request\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                if socket.write_all(head).await.is_err() {
                    return;
                }
                let chunk = vec![b'x'; Self::CHUNK];
                let size = format!("{:x}\r\n", chunk.len());
                loop {
                    let frame = [size.as_bytes(), chunk.as_slice(), b"\r\n"].concat();
                    if socket.write_all(&frame).await.is_err() {
                        return;
                    }
                    written.fetch_add(chunk.len(), Ordering::SeqCst);
                    // 让出执行权：不 yield 的话这个循环会一直占着 worker，客户端那次
                    // "读满上限"的返回也要排到它后面。
                    tokio::task::yield_now().await;
                }
            });
            Self {
                url: format!("http://{address}/v1/chat/completions"),
                body_bytes,
            }
        }

        /// 桩已经吐出去的正文总量。
        fn written(&self) -> usize {
            self.body_bytes.load(Ordering::SeqCst)
        }
    }

    /// 测试用的固定客户端：不跟随代理设置，直接连本地桩。
    fn fixed_outbound() -> OutboundClient {
        OutboundClient::fixed(
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
        )
    }

    /// 绑定一个端口再放掉：地址确定，但上面肯定没人监听。
    async fn dead_address() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        address.to_string()
    }

    async fn test_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        (directory, store)
    }

    /// 一条真实的调用记录。`llm_calls.model_hash` 有外键，所以先建一个模型。
    async fn test_recorder(call_id: &str) -> (tempfile::TempDir, Store, CallRecorder) {
        let (directory, store) = test_store().await;
        let model = store
            .create_model(&ModelConfigInput {
                sort_order: 0,
                display_name: "Test Model".into(),
                group_name: None,
                model_type: ModelType::OpenAi,
                base_url: "https://example.com/v1/chat/completions".into(),
                use_full_url: true,
                api_key: "test-key".into(),
                tooltip_data: "Test Model".into(),
                model_id: "test-model".into(),
                reasoning_effort: None,
                openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
                openai_extra_params_enabled: false,
                openai_extra_params: json!({}),
                custom_headers_enabled: false,
                custom_headers: json!({}),
                anthropic_extra_params_enabled: false,
                anthropic_extra_params: json!({}),
                context_window_tokens: None,
                max_completion_tokens: None,
                anthropic_max_tokens: None,
                anthropic_thinking_effort: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap();
        let recorder = CallRecorder::start(
            store.clone(),
            NewLlmCall {
                purpose: crate::model::CallPurpose::Chat,
                client_source: CallClient::Internal,
                call_id: call_id.into(),
                run_id: call_id.into(),
                conversation_id: call_id.into(),
                provider_call_index: 0,
                model_hash: model.model_hash,
                provider_type: ProviderType::OpenAiChat,
                provider_url: "https://example.com".into(),
                request_type: ProviderType::OpenAiChat,
                request_url: "https://example.com/v1/chat/completions".into(),
                model_id: "test-model".into(),
                display_name: "Test Model".into(),
                reasoning_effort: None,
                fast: false,
                message_count: 1,
                tool_count: 0,
                detailed: false,
            },
        )
        .await
        .unwrap();
        (directory, store, recorder)
    }

    async fn set_proxy(store: &Store, mode: ProxyMode, address: &str) {
        store
            .set_proxy_settings(ProxySettingsInput {
                mode,
                address: address.into(),
                auth_enabled: false,
                username: String::new(),
                password: None,
            })
            .await
            .unwrap();
    }

    /// 收集测试期间的日志：重试必须留下 warn，而且只留一条。
    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock()).into_owned()
        }
    }

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogs;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// 测试线程的日志落进 `CapturedLogs`；守卫必须活到断言之后。
    fn capture_logs(logs: &CapturedLogs) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .finish(),
        )
    }

    /// 网关 502 发生在拿到任何响应内容之前：重试一次，第二次的响应就是调用方
    /// 看到的那一个；记录里只有一次调用、一个最终状态码，日志里恰好一条重试 warn。
    #[tokio::test]
    async fn a_gateway_failure_before_any_content_is_retried() {
        let logs = CapturedLogs::default();
        let _captured = capture_logs(&logs);
        let stub = Stub::start(vec![GATEWAY_DOWN, OK_JSON]).await;
        let (_directory, store, recorder) = test_recorder("retry-502").await;

        let outcome = attempt(
            &UpstreamCall {
                label: "OpenAI Chat",
                model: "test-model",
                url: &stub.url,
            },
            &fixed_outbound(),
            |client| client.post(&stub.url).json(&json!({"model": "test-model"})),
            &CancellationToken::new(),
            Some(&recorder),
        )
        .await
        .unwrap();

        let Attempt::Response(response) = outcome else {
            panic!("the retried attempt must produce a response");
        };
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"{}");
        assert_eq!(stub.attempts(), 2, "第一次 502 之后必须再发一次");

        let calls = store.llm_calls(10).await.unwrap();
        assert_eq!(calls.len(), 1, "一次调用只留一条记录");
        assert_eq!(calls[0].http_status, Some(200), "记录的是最终状态码");

        let text = logs.text();
        assert_eq!(
            text.matches("upstream request failed before any response; retrying")
                .count(),
            1
        );
        assert!(text.contains("test-model"));
        assert!(text.contains(&stub.url));
    }

    /// 连接层失败一次之后成功：调用方拿到 200，桩只被连过一次，日志里一条 warn。
    #[tokio::test]
    async fn a_connection_failure_before_any_content_is_retried() {
        let logs = CapturedLogs::default();
        let _captured = capture_logs(&logs);
        let stub = Stub::start(vec![OK_JSON]).await;
        let stub_url = stub.url.clone();
        let dead = format!("http://{}/v1/chat/completions", dead_address().await);
        let tries = Arc::new(AtomicUsize::new(0));
        let counted = tries.clone();

        let outcome = attempt(
            &UpstreamCall {
                label: "OpenAI Chat",
                model: "test-model",
                url: &stub.url,
            },
            &fixed_outbound(),
            move |client| {
                let url = if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    dead.clone()
                } else {
                    stub_url.clone()
                };
                client.post(url).json(&json!({"model": "test-model"}))
            },
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, Attempt::Response(response) if response.status().is_success()));
        assert_eq!(tries.load(Ordering::SeqCst), 2, "第一次必须是真的发过");
        assert_eq!(stub.attempts(), 1, "桩只被连过一次");
        assert_eq!(
            logs.text()
                .matches("upstream request failed before any response; retrying")
                .count(),
            1
        );
    }

    /// 重试有上限：第三次仍失败就按最后一次的结果上报，数字写进文案。
    #[tokio::test]
    async fn the_retry_bound_and_the_retry_count_reach_the_error_message() {
        let stub = Stub::start(vec![GATEWAY_DOWN]).await;

        let error = send(
            &UpstreamCall {
                label: "OpenAI Chat",
                model: "test-model",
                url: &stub.url,
            },
            &fixed_outbound(),
            |client| client.post(&stub.url).json(&json!({"model": "test-model"})),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

        let Error::Provider(message) = error else {
            panic!("a rejected status must be a provider error");
        };
        assert_eq!(
            message,
            "OpenAI Chat 502 Bad Gateway (after 2 retries): down"
        );
        assert_eq!(stub.attempts(), 1 + MAX_RETRIES as usize);
    }

    /// 发送阶段失败也会把次数写进文案：界面据此知道这不是第一次尝试的偶发结果。
    #[tokio::test]
    async fn a_persistent_connection_failure_reports_the_retry_count() {
        let dead = format!("http://{}/v1/chat/completions", dead_address().await);

        let error = send(
            &UpstreamCall {
                label: "OpenAI Chat",
                model: "test-model",
                url: &dead,
            },
            &fixed_outbound(),
            |client| client.post(&dead).json(&json!({"model": "test-model"})),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

        let Error::Provider(message) = error else {
            panic!("a retried send failure must carry the count: {error:?}");
        };
        assert!(
            message.starts_with("OpenAI Chat upstream request failed (after 2 retries):"),
            "unexpected message: {message}"
        );
    }

    /// 已经开始返回响应体之后断开：不再重试，错误原样交给调用方（这里表现为
    /// 读正文失败），上游不会被问第二次。
    #[tokio::test]
    async fn a_response_body_that_breaks_is_never_retried() {
        let logs = CapturedLogs::default();
        let _captured = capture_logs(&logs);
        let stub = Stub::start(vec![TRUNCATED_BODY]).await;

        let response = send(
            &UpstreamCall {
                label: "OpenAI Chat",
                model: "test-model",
                url: &stub.url,
            },
            &fixed_outbound(),
            |client| client.post(&stub.url).json(&json!({"model": "test-model"})),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap()
        .expect("the headers arrived, so the caller owns the broken body");

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert!(response.bytes().await.is_err(), "断开的正文必须报错");
        assert_eq!(stub.attempts(), 1, "吐过字节之后不许重试");
        assert!(logs.text().is_empty(), "不重试就不该有 warn");
    }

    /// 4xx 不重试：它说的是这次请求本身的问题，再发一次也是同一个结果。
    #[tokio::test]
    async fn a_client_error_is_reported_without_a_retry() {
        let stub = Stub::start(vec![BAD_REQUEST]).await;

        let error = send(
            &UpstreamCall {
                label: "OpenAI Chat",
                model: "test-model",
                url: &stub.url,
            },
            &fixed_outbound(),
            |client| client.post(&stub.url).json(&json!({"model": "test-model"})),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

        let Error::Provider(message) = error else {
            panic!("a rejected status must be a provider error");
        };
        assert_eq!(message, "OpenAI Chat 400 Bad Request: nope");
        assert_eq!(stub.attempts(), 1);
    }

    /// 4xx 之后上游一直滴字节（chunked、永不结束）：正文必须停在上限处并**立刻收手**，
    /// 而不是像原来的 `response.bytes()` 那样一直读到客户端总超时、内存跟着涨。
    ///
    /// 用例外面套的是真实墙钟上界：这条用例的失败模式是"永远读下去"，上界必须来自用例
    /// 自己，坏掉的时候才是干净失败。
    #[tokio::test]
    async fn a_rejected_body_that_keeps_dripping_stops_at_the_limit() {
        let stub = DrippingStub::start().await;
        let wall_clock_bound = Duration::from_secs(10);

        let error = tokio::time::timeout(
            wall_clock_bound,
            send(
                &UpstreamCall {
                    label: "OpenAI Chat",
                    model: "test-model",
                    url: &stub.url,
                },
                &fixed_outbound(),
                |client| client.post(&stub.url).json(&json!({"model": "test-model"})),
                &CancellationToken::new(),
                None,
            ),
        )
        .await
        .expect("正文上限生效时必须立刻收手，而不是把整份响应读完")
        .unwrap_err();

        let Error::Provider(message) = error else {
            panic!("被拒绝的状态必须是 provider 错误");
        };
        let prefix = "OpenAI Chat 400 Bad Request: ";
        assert!(message.starts_with(prefix), "{message}");
        assert!(
            message.ends_with("…<truncated>"),
            "截断必须在正文尾部标出来: {message}"
        );
        assert_eq!(
            message.len(),
            prefix.len() + REJECTED_BODY_LIMIT + " …<truncated>".len(),
            "正文必须正好停在 {REJECTED_BODY_LIMIT} 字节，多一字节都说明没按上限收手"
        );
        assert!(
            stub.written() > REJECTED_BODY_LIMIT,
            "桩还在继续吐（已吐 {} 字节），所以截断只可能来自上限本身，不是上游读完了",
            stub.written()
        );
    }

    /// 退避期间被取消：立刻结束，第二次请求不许发出。
    #[tokio::test]
    async fn cancellation_during_the_backoff_stops_the_second_attempt() {
        let mut stub = Stub::start(vec![GATEWAY_DOWN]).await;
        let url = stub.url.clone();
        let outbound = fixed_outbound();
        let cancellation = CancellationToken::new();
        let upstream_call = UpstreamCall {
            label: "OpenAI Chat",
            model: "test-model",
            url: &url,
        };

        let send = attempt(
            &upstream_call,
            &outbound,
            |client| client.post(&url).json(&json!({"model": "test-model"})),
            &cancellation,
            None,
        );
        let stop = async {
            // 第一次的 502 已经写出去，此刻正处于 300 ms 退避里。
            stub.wait_for_response(0).await;
            cancellation.cancel();
        };
        let (outcome, ()) = tokio::join!(send, stop);

        assert!(matches!(outcome.unwrap(), Attempt::Cancelled));
        tokio::time::sleep(RETRY_BACKOFF[0] + Duration::from_millis(200)).await;
        assert_eq!(stub.attempts(), 1, "取消之后不许再发第二次请求");
    }

    /// 出网路由变了（用户中途关掉 Clash）：第一次连接失败按当前配置重建客户端，
    /// 这次重试发往新地址。
    #[tokio::test]
    async fn a_changed_outbound_route_is_retried_with_the_rebuilt_client() {
        let (_directory, store) = test_store().await;
        set_proxy(
            &store,
            ProxyMode::Custom,
            &format!("http://{}", dead_address().await),
        )
        .await;
        let clients = NetworkClients::new(store.clone());
        let outbound = OutboundClient::provider(clients.clone(), Duration::from_secs(5))
            .await
            .unwrap();

        // 缓存建好之后把代理换成一个能用的地址：路由变了，缓存里的客户端还指着
        // 旧地址。
        let stub = Stub::start(vec![OK_JSON]).await;
        set_proxy(&store, ProxyMode::Custom, &stub.origin).await;

        let outcome = attempt(
            &UpstreamCall {
                label: "OpenAI Chat",
                model: "test-model",
                url: "http://provider.invalid/v1/chat/completions",
            },
            &outbound,
            |client| client.get("http://provider.invalid/v1/chat/completions"),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

        assert!(
            matches!(outcome, Attempt::Response(response) if response.status().is_success()),
            "the rebuilt client must carry the retried request"
        );
        assert_eq!(stub.attempts(), 1);
        assert!(
            !clients.outbound_route_changed().await,
            "the cache must now match the current settings"
        );
    }

    /// 适配器路径（Cursor 的通用 provider 流与 Devin 的模型调用都走它）：网关 502
    /// 之后重试拿到的 SSE，就是调用方看到的事件流。
    #[tokio::test]
    async fn the_openai_chat_adapter_retries_a_gateway_failure() {
        let stub = Stub::start(vec![GATEWAY_DOWN, OK_SSE]).await;
        let provider = OpenAiChatProvider::new(
            fixed_outbound(),
            ProviderConfig {
                kind: ProviderKind::OpenAiChat,
                request_url: stub.url.clone(),
                api_key: "test-key".into(),
                custom_headers: reqwest::header::HeaderMap::new(),
                max_output_tokens: None,
                request_timeout: Duration::from_secs(30),
            },
        );

        let events = collect_events(provider.stream(
            ModelInvocation {
                purpose: crate::model::CallPurpose::Chat,
                client_source: CallClient::Internal,
                call_id: "retry-adapter".into(),
                run_id: "run".into(),
                conversation_id: "conversation".into(),
                provider_call_index: 0,
                request: ModelRequest {
                    prompt: PromptSpec {
                        instructions: "hello".into(),
                        tools: Vec::new(),
                    },
                    model: ModelSpec::new("stub-model"),
                    history: Vec::new(),
                },
            },
            CancellationToken::new(),
        ))
        .await;

        assert!(events
            .iter()
            .any(|event| matches!(event, ModelEvent::TextDelta(text) if text == "hi")));
        assert_eq!(events.last(), Some(&ModelEvent::Done(FinishReason::Stop)));
        assert_eq!(stub.attempts(), 2);
    }

    /// 一个"接了不回话"的上游：TCP 连上、读掉请求，然后一直不回。
    async fn silent_upstream() -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let counted = connections.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut request = [0_u8; 4096];
                    let _ = socket.read(&mut request).await;
                    // 占住连接直到测试结束（调用方超时之后会自己放弃它）。
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    drop(socket);
                });
            }
        });
        (format!("http://{address}"), connections)
    }

    /// 记录状态码写库失败不能把已经拿到的上游响应丢掉：用户要的是这一轮回答，
    /// 观测里缺一条状态码只该留下一条 warn。
    #[tokio::test]
    async fn a_failed_header_record_keeps_the_upstream_response() {
        let logs = CapturedLogs::default();
        let _captured = capture_logs(&logs);
        let stub = Stub::start(vec![OK_JSON]).await;
        let (_directory, store, recorder) = test_recorder("record-failure").await;
        // 关掉连接池：之后的每一条记录写入都会以 PoolClosed 失败。
        store.pool().close().await;

        let outcome = attempt(
            &UpstreamCall {
                label: "OpenAI Chat",
                model: "test-model",
                url: &stub.url,
            },
            &fixed_outbound(),
            |client| client.post(&stub.url).json(&json!({"model": "test-model"})),
            &CancellationToken::new(),
            Some(&recorder),
        )
        .await
        .expect("记录失败不该让整次调用报错");

        let Attempt::Response(response) = outcome else {
            panic!("已经拿到的 2xx 响应必须交给调用方");
        };
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(stub.attempts(), 1);
        assert!(
            logs.text()
                .contains("failed to record the upstream response status"),
            "记录失败必须留下 warn: {}",
            logs.text()
        );
    }

    /// 请求级超时不重试：上游接了却不回话时请求已经发出去了，重发一次既会把一次超时
    /// 预算翻几倍，也可能让上游再生成一遍（模块头第 9-10 行的承诺）。
    #[tokio::test]
    async fn a_request_timeout_is_never_retried() {
        let (origin, connections) = silent_upstream().await;
        let url = format!("{origin}/v1/chat/completions");
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap();

        let outcome = attempt(
            &UpstreamCall {
                label: "OpenAI Chat",
                model: "test-model",
                url: &url,
            },
            &OutboundClient::fixed(client),
            |client| client.post(&url).json(&json!({"model": "test-model"})),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

        let Attempt::Failed { error, retries } = outcome else {
            panic!("一个不回话的上游只能以发送失败收场");
        };
        assert!(error.is_timeout(), "这里要构造的是请求级超时: {error:?}");
        assert_eq!(retries, 0, "超时不许进重试预算");
        assert_eq!(connections.load(Ordering::SeqCst), 1, "只许发一次");
    }

    async fn collect_events(mut stream: ProviderStream) -> Vec<ModelEvent> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        events
    }
}
