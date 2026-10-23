//! Routes model requests to built-in configurations or stable plugin model IDs.
use std::{sync::Arc, time::Duration};

use async_stream::try_stream;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{ProviderConfig, ProviderKind},
    model::{ModelInvocation, ModelLatency, NewLlmCall, ProviderType},
    network::OutboundClient,
    plugin::{PluginRegistry, ADAPTER_ID_PREFIX},
    store::Store,
    Error, Result,
};

use super::{
    normalize::NormalizedProvider, recorder::CancelOnDrop, AnthropicProvider, CallRecorder,
    OpenAiChatProvider, OpenAiResponsesProvider, Provider, ProviderStream,
};

pub struct ProviderRouter {
    store: Store,
    plugins: PluginRegistry,
    clients: crate::network::NetworkClients,
    request_timeout: Duration,
    stream_idle_timeout: Duration,
}

impl ProviderRouter {
    pub fn new(
        store: Store,
        plugins: PluginRegistry,
        clients: crate::network::NetworkClients,
        request_timeout: Duration,
        stream_idle_timeout: Duration,
    ) -> Self {
        Self {
            store,
            plugins,
            clients,
            request_timeout,
            stream_idle_timeout,
        }
    }
}

impl Provider for ProviderRouter {
    fn stream(
        &self,
        invocation: ModelInvocation,
        cancellation: CancellationToken,
    ) -> ProviderStream {
        let store = self.store.clone();
        let plugins = self.plugins.clone();
        let clients = self.clients.clone();
        let request_timeout = self.request_timeout;
        let stream_idle_timeout = self.stream_idle_timeout;
        Box::pin(try_stream! {
            let selected = invocation.request.model.model_id.clone();
            // 两条分支只负责装配 Recorder 与 Provider 流;
            // 事件消费(空闲超时看门狗、记录、错误规范化)对两者完全一致。
            let (recorder, _cancel_on_drop, mut stream): (CallRecorder, CancelOnDrop, ProviderStream) =
                if selected.starts_with(ADAPTER_ID_PREFIX) {
                    // 插件模型与内置模型走完全相同的流程:资源选择与将来的
                    // 负载均衡都在插件 Provider 内部。
                    let plan = plugins.plan_model(&selected).await?;
                    let mut routed = invocation.clone();
                    routed.request.model.display_name = Some(plan.model.display_name.clone());
                    if let Some(tokens) = plan.model.max_output_tokens {
                        routed.request.model.max_output_tokens.get_or_insert(tokens);
                    }
                    let recorder = start_recorder(
                        &store,
                        RecordedCall {
                            invocation: &invocation,
                            reasoning_effort: routed.request.model.reasoning.effort.as_deref(),
                            model_hash: &selected,
                            display_name: &plan.model.display_name,
                            provider_type: ProviderType::Plugin,
                            request_url: &plan.request_url,
                            model_id: &plan.model.model_id,
                        },
                    )
                    .await?;
                    let guard = recorder.cancel_on_drop();
                    let provider: Arc<dyn Provider> = Arc::new(NormalizedProvider::new(Arc::new(PluginModelProvider {
                        registry: plugins.clone(),
                        recorder: recorder.clone(),
                    })));
                    (recorder, guard, provider.stream(routed, cancellation.clone()))
                } else {
                    let mut routed = invocation.clone();
                    let model = store.model(&selected).await?.ok_or_else(|| Error::Provider(format!("unknown model: {selected}")))?;
                    let provider_type = model.provider_type();
                    let request_url = model.request_url()?;
                    model.configure(&mut routed.request.model);
                    routed.request.model.extra_params =
                        super::request_template::render_json_strings(
                            model.extra_params(),
                            &invocation.conversation_id,
                        );
                    routed.request.model.model_id = model.model_id.clone();
                    let recorder = start_recorder(
                        &store,
                        RecordedCall {
                            invocation: &invocation,
                            reasoning_effort: routed.request.model.reasoning.effort.as_deref(),
                            model_hash: &model.model_hash,
                            display_name: &model.display_name,
                            provider_type,
                            request_url: &request_url,
                            model_id: &model.model_id,
                        },
                    )
                    .await?;
                    let guard = recorder.cancel_on_drop();
                    let config = ProviderConfig {
                        kind: provider_kind(provider_type),
                        request_url,
                        api_key: model.api_key.clone(),
                        custom_headers: if model.custom_headers_enabled {
                            custom_headers(
                                &model.custom_headers,
                                &invocation.conversation_id,
                            )?
                        } else {
                            reqwest::header::HeaderMap::new()
                        },
                        max_output_tokens: model.max_output_tokens(),
                        request_timeout,
                    };
                    let outbound = OutboundClient::provider(clients.clone(), request_timeout).await?;
                    let provider = build_observed(&config, recorder.clone(), outbound)?;
                    (recorder, guard, provider.stream(routed, cancellation.clone()))
                };

            let stream_started = std::time::Instant::now();
            tracing::debug!(
                model = %selected,
                request_timeout_ms = request_timeout.as_millis() as u64,
                stream_idle_timeout_ms = stream_idle_timeout.as_millis() as u64,
                "provider stream created"
            );
            let mut last_event_time = std::time::Instant::now();
            let mut event_count: u64 = 0;
            loop {
                let event = match next_provider_event(&mut stream, stream_idle_timeout).await {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(_) => {
                        let elapsed_ms = stream_started.elapsed().as_millis() as u64;
                        let error = stream_idle_timeout_error(stream_idle_timeout);
                        tracing::warn!(
                            error = %error,
                            elapsed_ms,
                            event_count,
                            idle_timeout_ms = stream_idle_timeout.as_millis() as u64,
                            "provider stream idle timeout"
                        );
                        Err(error)
                    }
                };
                let now = std::time::Instant::now();
                let gap_ms = now.duration_since(last_event_time).as_millis() as u64;
                let elapsed_ms = now.duration_since(stream_started).as_millis() as u64;
                event_count += 1;
                match event {
                    Ok(event) => {
                        if gap_ms > 5000 {
                            tracing::debug!(
                                gap_ms,
                                elapsed_ms,
                                event = event_name(&event),
                                event_count,
                                "slow gap detected between provider events"
                            );
                        }
                        warn_on_record_failure("event", recorder.event(&event).await);
                        last_event_time = now;
                        yield event;
                    }
                    Err(error) => {
                        let error = normalize_provider_stream_error(error, request_timeout);
                        tracing::debug!(
                            error = %error,
                            elapsed_ms,
                            gap_ms,
                            event_count,
                            "provider stream error"
                        );
                        // 记录失败只留 warn：它不能顶掉真正的失败原因（用户看到的必须是
                        // 上游/超时那个错，而不是"观测写不进去"）。
                        warn_on_record_failure("failure", recorder.failed(&error).await);
                        Err(error)?;
                    }
                }
            }
            finish_stream(&recorder, &cancellation).await?;
        })
    }
}

fn event_name(event: &super::ModelEvent) -> &'static str {
    match event {
        super::ModelEvent::Start { .. } => "Start",
        super::ModelEvent::TextStart => "TextStart",
        super::ModelEvent::TextDelta(_) => "TextDelta",
        super::ModelEvent::TextEnd => "TextEnd",
        super::ModelEvent::ThinkingStart => "ThinkingStart",
        super::ModelEvent::ThinkingDelta(_) => "ThinkingDelta",
        super::ModelEvent::ThinkingSignature(_) => "ThinkingSignature",
        super::ModelEvent::ThinkingEnd => "ThinkingEnd",
        super::ModelEvent::ToolCallStart { .. } => "ToolCallStart",
        super::ModelEvent::ToolCallArgumentsDelta { .. } => "ToolCallArgsDelta",
        super::ModelEvent::ToolCallEnd { .. } => "ToolCallEnd",
        super::ModelEvent::ProviderReplayState(_) => "ReplayState",
        super::ModelEvent::Usage(_) => "Usage",
        super::ModelEvent::StopSequence(_) => "StopSequence",
        super::ModelEvent::Done(_) => "Done",
    }
}

/// 开一条调用记录。
///
/// `reasoning_effort` 由调用方传**生效值**（`configure` 之后 `routed` 上的强度），不是
/// `invocation` 里的入参：调用方没表态时 `configure` 会补模型配置的强度（Anthropic 默认
/// xhigh），记入参会让界面显示"没填"，而上游实际收到的是 xhigh。
/// 一条调用记录的身份与生效参数。
///
/// 七个字段来自不同的来源（调用、模型配置、路由结果），打包成参数只是为了不让
/// `start_recorder` 排七个位置参数——它们是同一次记录的全部输入，逐个传递容易错位。
struct RecordedCall<'a> {
    invocation: &'a ModelInvocation,
    reasoning_effort: Option<&'a str>,
    model_hash: &'a str,
    display_name: &'a str,
    provider_type: ProviderType,
    request_url: &'a str,
    model_id: &'a str,
}

async fn start_recorder(store: &Store, call: RecordedCall<'_>) -> Result<CallRecorder> {
    let RecordedCall {
        invocation,
        reasoning_effort,
        model_hash,
        display_name,
        provider_type,
        request_url,
        model_id,
    } = call;
    CallRecorder::start(
        store.clone(),
        NewLlmCall {
            // 调用用途随 invocation 一路带下来：压缩摘要与普通对话共用记录表，
            // 但只有普通对话能当上下文用量锚点（见 `CallPurpose`）。
            purpose: invocation.purpose,
            client_source: invocation.client_source,
            call_id: invocation.call_id.clone(),
            run_id: invocation.run_id.clone(),
            conversation_id: invocation.conversation_id.clone(),
            provider_call_index: invocation.provider_call_index.min(i64::MAX as u64) as i64,
            model_hash: model_hash.into(),
            provider_type,
            provider_url: request_url.into(),
            request_type: provider_type,
            request_url: request_url.into(),
            model_id: model_id.into(),
            display_name: display_name.into(),
            reasoning_effort: reasoning_effort.map(str::to_owned),
            fast: invocation.request.model.latency == ModelLatency::Fast,
            // 上下文用量锚点的计数：只数持久化消息，不含只对 provider 可见的瞬时尾巴
            // （见 `ModelInvocation::persisted_message_count`，审计 09/F7）。
            message_count: invocation.persisted_message_count(),
            tool_count: invocation.request.prompt.tools.len(),
            detailed: false,
        },
    )
    .await
}

/// 观测写入失败只留一条 warn。
///
/// 调用记录是旁路：库被关掉、磁盘写满时，`recorder.*` 会返回错误，而这一轮回答本身是好的
/// ——把它传播出去等于让用户在聊天框里看到"记录失败"，还会顶掉真正的上游错误。这个函数
/// 返回 `()` 不是随手写的：类型上就没有可传播的错误，调用点不可能再写出 `?`。
fn warn_on_record_failure(label: &str, recorded: Result<()>) {
    if let Err(error) = recorded {
        tracing::warn!(
            error = %crate::diagnostics::error_chain(&error),
            record = label,
            "failed to record provider observability; keeping the response"
        );
    }
}

async fn finish_stream(recorder: &CallRecorder, cancellation: &CancellationToken) -> Result<()> {
    if recorder.is_finished() {
        return Ok(());
    }
    if cancellation.is_cancelled() {
        warn_on_record_failure("cancelled", recorder.cancelled().await);
    } else {
        let error = Error::Provider("provider stream ended without Done".into());
        warn_on_record_failure("failure", recorder.failed(&error).await);
        return Err(error);
    }
    Ok(())
}

/// 插件模型的 Provider 实现;对路由与规范化层完全等同于内置 Provider。
struct PluginModelProvider {
    registry: PluginRegistry,
    recorder: CallRecorder,
}

impl Provider for PluginModelProvider {
    fn stream(
        &self,
        invocation: ModelInvocation,
        cancellation: CancellationToken,
    ) -> ProviderStream {
        self.registry
            .stream_model(invocation, cancellation, self.recorder.clone())
    }
}

fn provider_kind(provider_type: ProviderType) -> ProviderKind {
    match provider_type {
        ProviderType::OpenAiChat => ProviderKind::OpenAiChat,
        ProviderType::OpenAiResponses => ProviderKind::OpenAiResponses,
        ProviderType::Anthropic => ProviderKind::Anthropic,
        // 内置模型的 provider_type 只来自 ModelType,不可能是插件。
        ProviderType::Plugin => unreachable!("plugin models never use built-in provider configs"),
    }
}

async fn next_provider_event(
    stream: &mut ProviderStream,
    idle_timeout: Duration,
) -> std::result::Result<Option<Result<super::ModelEvent>>, tokio::time::error::Elapsed> {
    tokio::time::timeout(idle_timeout, stream.next()).await
}

fn stream_idle_timeout_error(idle_timeout: Duration) -> Error {
    Error::Provider(format!(
        "provider stream idle timeout: no events received for {} seconds ({} minutes)",
        idle_timeout.as_secs(),
        idle_timeout.as_secs() / 60
    ))
}

fn request_timeout_error(request_timeout: Duration) -> Error {
    Error::Provider(format!(
        "provider request timed out after {} seconds ({} minutes)",
        request_timeout.as_secs(),
        request_timeout.as_secs() / 60
    ))
}

fn normalize_provider_stream_error(error: Error, request_timeout: Duration) -> Error {
    match error {
        Error::Http(source) if source.is_timeout() => request_timeout_error(request_timeout),
        Error::Http(source) if source.is_body() => Error::Provider(format!(
            "provider stream transport failed while reading the response body: {}",
            root_error_message(&source)
        )),
        error => error,
    }
}

fn root_error_message(error: &(dyn std::error::Error + 'static)) -> String {
    let mut current = error;
    while let Some(source) = current.source() {
        current = source;
    }
    current.to_string()
}

fn custom_headers(
    value: &serde_json::Value,
    conversation_id: &str,
) -> Result<reqwest::header::HeaderMap> {
    let rendered = super::request_template::render_json_strings(value, conversation_id);
    let object = rendered
        .as_object()
        .ok_or_else(|| Error::Config("custom headers must be an object".into()))?;
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in object {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| Error::Config(format!("invalid custom header name: {error}")))?;
        let value = value
            .as_str()
            .ok_or_else(|| Error::Config("custom header values must be strings".into()))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|error| Error::Config(format!("invalid custom header value: {error}")))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

pub fn build(config: &ProviderConfig) -> Result<Arc<dyn Provider>> {
    build_inner(config, None, None)
}

fn build_observed(
    config: &ProviderConfig,
    recorder: CallRecorder,
    outbound: OutboundClient,
) -> Result<Arc<dyn Provider>> {
    build_inner(config, Some(recorder), Some(outbound))
}

fn build_inner(
    config: &ProviderConfig,
    recorder: Option<CallRecorder>,
    outbound: Option<OutboundClient>,
) -> Result<Arc<dyn Provider>> {
    // 没有出网环境（直接调用 `build` 的测试与插件路径）时用固定客户端，
    // 它不参与"代理设置变了就重建"的兜底。
    let outbound = match outbound {
        Some(outbound) => outbound,
        None => OutboundClient::fixed(
            reqwest::Client::builder()
                .timeout(config.request_timeout)
                .build()?,
        ),
    };
    let provider: Arc<dyn Provider> = match config.kind {
        ProviderKind::OpenAiChat => {
            Arc::new(OpenAiChatProvider::new(outbound, config.clone()).with_recorder(recorder))
        }
        ProviderKind::OpenAiResponses => {
            Arc::new(OpenAiResponsesProvider::new(outbound, config.clone()).with_recorder(recorder))
        }
        ProviderKind::Anthropic => {
            Arc::new(AnthropicProvider::new(outbound, config.clone()).with_recorder(recorder))
        }
    };
    Ok(Arc::new(NormalizedProvider::new(provider)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CallClient, CallPurpose, ContentPart, ModelConfigInput, ModelRequest, ModelSpec, ModelType,
        ProjectedContent, ProjectedMessage, PromptSpec, Role, CONTINUE_TAIL_MESSAGE_ID,
    };
    use crate::store::ContextUsageAnchor;

    /// 压缩摘要的用途必须一路到达记录层。
    ///
    /// 记录层以前把 purpose 写死成 `Chat`，于是摘要调用被当成普通对话：`llm_calls`
    /// 里看不出它，更糟的是它会成为上下文用量锚点（输入是被裁剪过的重放历史，
    /// 系统性低估下一步的估算）。这里走**真实路由**（ProviderRouter + 本地上游 +
    /// recorder）而不是直接调 `start_recorder`。
    #[tokio::test]
    async fn a_compaction_summary_call_is_recorded_with_its_own_purpose() {
        // `PluginRegistry::managed` 只接受托管数据目录；即使这条路径用不到插件，
        // 也要把它指到本进程专属的临时目录，不能碰用户真实数据。
        static ISOLATE: std::sync::Once = std::sync::Once::new();
        ISOLATE.call_once(|| {
            let directory = Box::leak(Box::new(tempfile::tempdir().unwrap()));
            std::env::set_var("HAXSD_BYOK_DATA_DIR", directory.path());
        });

        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("purpose.db").display()
        ))
        .await
        .unwrap();
        // 两个上游给不同的输入用量：摘要行更新，但锚点必须还是 chat 行。
        let model = |base_url: String| ModelConfigInput {
            sort_order: 0,
            display_name: "Purpose Model".into(),
            group_name: None,
            model_type: ModelType::Anthropic,
            base_url,
            use_full_url: true,
            api_key: "key".into(),
            tooltip_data: "Purpose model".into(),
            model_id: "vendor-claude".into(),
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
        };
        let upstream = |input_tokens: u64| {
            crate::provider::testing::sse_upstream(
                "/v1/messages",
                vec![
                    serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":input_tokens}}}),
                    serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
                    serde_json::json!({"type":"content_block_stop","index":0}),
                    serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                    serde_json::json!({"type":"message_stop"}),
                ],
            )
        };
        let chat_model = store
            .create_model(&model(upstream(42).await))
            .await
            .unwrap();
        let summary_model = store.create_model(&model(upstream(7).await)).await.unwrap();
        let clients = crate::network::NetworkClients::new(store.clone());
        let plugins = PluginRegistry::managed(
            store.clone(),
            crate::plugin::PluginRuntime::managed().unwrap(),
            "purpose-test".into(),
        )
        .unwrap();
        let router = ProviderRouter::new(
            store.clone(),
            plugins,
            clients,
            Duration::from_secs(30),
            Duration::from_secs(30),
        );
        let invocation = |call_id: &str, model_hash: &str, purpose: CallPurpose, history: usize| {
            ModelInvocation {
                purpose,
                client_source: CallClient::Internal,
                call_id: call_id.into(),
                run_id: "purpose-run".into(),
                conversation_id: "purpose-conversation".into(),
                provider_call_index: 0,
                request: ModelRequest {
                    prompt: PromptSpec {
                        instructions: String::new(),
                        tools: Vec::new(),
                    },
                    model: ModelSpec::new(model_hash.to_owned()),
                    history: (0..history)
                        .map(|index| ProjectedMessage {
                            message_id: format!("m-{}", index),
                            role: Role::User,
                            content: ProjectedContent::Parts(vec![ContentPart::Text {
                                text: "hi".into(),
                            }]),
                        })
                        .collect(),
                },
            }
        };

        for (call_id, model_hash, purpose, history) in [
            (
                "chat-call",
                chat_model.model_hash.as_str(),
                CallPurpose::Chat,
                3,
            ),
            (
                "summary-call",
                summary_model.model_hash.as_str(),
                CallPurpose::CompactionSummary,
                1,
            ),
        ] {
            let events = crate::provider::testing::drain(router.stream(
                invocation(call_id, model_hash, purpose, history),
                CancellationToken::new(),
            ))
            .await
            .unwrap();
            assert_eq!(
                crate::provider::testing::finish_of(&events),
                Some(crate::provider::FinishReason::Stop)
            );
        }

        let calls = store.llm_calls(10).await.unwrap();
        let purpose_of = |call_id: &str| {
            calls
                .iter()
                .find(|call| call.call_id == call_id)
                .unwrap_or_else(|| panic!("call_id missing: {}", call_id))
                .purpose
                .clone()
        };
        assert_eq!(purpose_of("chat-call"), "chat");
        assert_eq!(
            purpose_of("summary-call"),
            "compaction-summary",
            "摘要调用的用途必须原样落库"
        );
        assert_eq!(
            store
                .latest_context_usage("purpose-conversation")
                .await
                .unwrap(),
            Some(ContextUsageAnchor {
                context_input_tokens: 42,
                message_count: 3,
            }),
            "更新的摘要行不是锚点；锚点仍是最近一次 chat"
        );
    }

    /// 上下文用量锚点的计数（09/F7）：只数**持久化**消息，不含只对 provider 可见的
    /// 瞬时尾巴（`runtime:continue`）——否则下一个 run 按这个偏大的下标切片会漏掉
    /// 一条新消息。
    #[tokio::test]
    async fn the_recorded_anchor_counts_persisted_messages_only() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let history = |persisted: usize, tail: Option<&str>| {
            let mut messages = (0..persisted)
                .map(|index| ProjectedMessage {
                    message_id: format!("m{index}"),
                    role: Role::User,
                    content: ProjectedContent::Parts(vec![ContentPart::Text { text: "hi".into() }]),
                })
                .collect::<Vec<_>>();
            if let Some(tail) = tail {
                messages.push(ProjectedMessage {
                    message_id: tail.into(),
                    role: Role::User,
                    content: ProjectedContent::Parts(vec![ContentPart::Text {
                        text: "continue".into(),
                    }]),
                });
            }
            messages
        };
        let invocation = |call_id: &str, persisted: usize, tail: Option<&str>| ModelInvocation {
            purpose: crate::model::CallPurpose::Chat,
            client_source: crate::model::CallClient::Cursor,
            call_id: call_id.into(),
            run_id: "anchor-run".into(),
            conversation_id: "anchor-conversation".into(),
            provider_call_index: 0,
            request: ModelRequest {
                prompt: PromptSpec {
                    instructions: String::new(),
                    tools: Vec::new(),
                },
                model: ModelSpec::new("anchor-model".to_owned()),
                history: history(persisted, tail),
            },
        };

        for (call_id, persisted, tail) in [
            ("anchor-with-tail", 3_usize, Some(CONTINUE_TAIL_MESSAGE_ID)),
            ("anchor-without-tail", 4, None),
        ] {
            start_recorder(
                &store,
                RecordedCall {
                    invocation: &invocation(call_id, persisted, tail),
                    reasoning_effort: None,
                    model_hash: "hash",
                    display_name: "Anchor Model",
                    provider_type: ProviderType::OpenAiChat,
                    request_url: "https://example.com/v1/chat/completions",
                    model_id: "anchor-model",
                },
            )
            .await
            .unwrap();
        }

        let calls = store.llm_calls(10).await.unwrap();
        assert_eq!(calls.len(), 2);
        // 带尾巴那次的历史长度是 4，锚点必须记 3；不带尾巴那次记 4。
        for (call_id, expected) in [("anchor-with-tail", 3_i64), ("anchor-without-tail", 4)] {
            let call = calls
                .iter()
                .find(|call| call.call_id == call_id)
                .unwrap_or_else(|| panic!("{call_id} is recorded"));
            assert_eq!(
                call.message_count, expected,
                "{call_id}: 锚点只数持久化消息"
            );
        }
    }

    /// 调用记录里的思考强度必须是**生效值**：调用方没表态时 `configure` 会补上模型配置的
    /// 强度，记入参（`None`）会让界面显示"没填"，而上游其实收到的是 xhigh。
    #[tokio::test]
    async fn the_recorded_reasoning_effort_is_the_effective_one() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let invocation = ModelInvocation {
            purpose: crate::model::CallPurpose::Chat,
            client_source: crate::model::CallClient::Cursor,
            call_id: "effort-call".into(),
            run_id: "effort-run".into(),
            conversation_id: "effort-conversation".into(),
            provider_call_index: 0,
            request: ModelRequest {
                prompt: PromptSpec {
                    instructions: String::new(),
                    tools: Vec::new(),
                },
                model: ModelSpec::new("effort-model".to_owned()),
                history: Vec::new(),
            },
        };
        assert_eq!(
            invocation.request.model.reasoning.effort, None,
            "入参本身没表态"
        );
        let mut configured = invocation.request.model.clone();
        configured.reasoning.effort = Some("xhigh".into());

        start_recorder(
            &store,
            RecordedCall {
                invocation: &invocation,
                reasoning_effort: configured.reasoning.effort.as_deref(),
                model_hash: "hash",
                display_name: "Effort Model",
                provider_type: ProviderType::Anthropic,
                request_url: "https://example.com/v1/messages",
                model_id: "effort-model",
            },
        )
        .await
        .unwrap();

        let calls = store.llm_calls(10).await.unwrap();
        let call = calls
            .iter()
            .find(|call| call.call_id == "effort-call")
            .expect("调用记录已落库");
        assert_eq!(call.reasoning_effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn renders_cursor_conversation_id_in_custom_header_values() {
        let template = serde_json::json!({
            "x-opencode-session-id": "{{SessionId}}",
            "x-label": "cursor/{{SessionId}}"
        });

        let headers = custom_headers(&template, "cursor-conversation-id").unwrap();

        assert_eq!(
            headers.get("x-opencode-session-id").unwrap(),
            "cursor-conversation-id"
        );
        assert_eq!(
            headers.get("x-label").unwrap(),
            "cursor/cursor-conversation-id"
        );
        assert_eq!(template["x-opencode-session-id"], "{{SessionId}}");
    }

    #[tokio::test]
    async fn pending_provider_event_hits_the_idle_timeout() {
        let mut stream: ProviderStream = Box::pin(futures_util::stream::pending());

        let result = next_provider_event(&mut stream, Duration::from_millis(1)).await;

        assert!(result.is_err());
    }

    #[test]
    fn timeout_errors_state_the_boundary_and_duration() {
        let Error::Provider(idle) = stream_idle_timeout_error(Duration::from_secs(30 * 60)) else {
            panic!("idle timeout must be a provider error");
        };
        assert_eq!(
            idle,
            "provider stream idle timeout: no events received for 1800 seconds (30 minutes)"
        );

        let Error::Provider(request) = request_timeout_error(Duration::from_secs(60 * 60)) else {
            panic!("request timeout must be a provider error");
        };
        assert_eq!(
            request,
            "provider request timed out after 3600 seconds (60 minutes)"
        );
    }
}
