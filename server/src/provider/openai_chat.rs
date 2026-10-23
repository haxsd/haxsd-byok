//! Implements the OpenAI Chat Completions provider adapter.
use std::collections::BTreeMap;

use async_stream::try_stream;
use base64::{engine::general_purpose::STANDARD, Engine};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};

use crate::{
    config::ProviderConfig,
    model::{
        ContentPart, ModelInvocation, ModelLatency, ProjectedContent, ProjectedMessage, Role,
        ToolCallContent, Usage,
    },
    network::OutboundClient,
    Error, Result,
};

use super::{
    apply_openai_prompt_cache_key, map_sse_error, merge_extra_params, provider_event_error,
    recorder::recorded_headers, CallRecorder, FinishReason, ModelEvent, Provider, ProviderStream,
};
use crate::upstream::{self, UpstreamCall};

#[derive(Default)]
struct ChatToolState {
    call_id: String,
    name: String,
    arguments: String,
    emitted_arguments: usize,
    started: bool,
}

pub struct OpenAiChatProvider {
    outbound: OutboundClient,
    config: ProviderConfig,
    recorder: Option<CallRecorder>,
}

impl OpenAiChatProvider {
    pub fn new(outbound: OutboundClient, config: ProviderConfig) -> Self {
        Self {
            outbound,
            config,
            recorder: None,
        }
    }

    pub fn with_recorder(mut self, recorder: Option<CallRecorder>) -> Self {
        self.recorder = recorder;
        self
    }
}

impl Provider for OpenAiChatProvider {
    fn stream(
        &self,
        invocation: ModelInvocation,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> ProviderStream {
        let outbound = self.outbound.clone();
        let config = self.config.clone();
        let recorder = self.recorder.clone();
        Box::pin(try_stream! {
            let ModelInvocation { call_id, request, .. } = invocation;
            tracing::debug!(
                model = %request.model.model_id,
                call_id = %call_id,
                history_len = request.history.len(),
                tools_count = request.prompt.tools.len(),
                "OpenAI Chat provider stream started"
            );
            let messages = openai_chat_messages(&request.prompt.instructions, &request.history)?;
            let mut body = json!({
                "model": request.model.model_id,
                "messages": messages,
                "stream": true,
                "stream_options": {"include_usage": true}
            });
            if !request.prompt.tools.is_empty() {
                body["tools"] = json!(request.prompt.tools.iter().map(|tool| json!({"type":"function","function":{
                    "name": tool.name, "description": tool.description, "parameters": tool.parameters
                }})).collect::<Vec<_>>());
            }
            if !request.model.stop_sequences.is_empty() {
                // OpenAI Chat 最多接受 4 条停止序列；多出来的由入站网关在事件层截断。
                body["stop"] = json!(request
                    .model
                    .stop_sequences
                    .iter()
                    .take(4)
                    .collect::<Vec<_>>());
            }
            apply_model(&mut body, &request.model, config.max_output_tokens)?;
            merge_extra_params(&mut body, &request.model.extra_params)?;
            apply_openai_prompt_cache_key(&mut body, &request.model.model_id)?;
            let request_headers = recorded_headers(&config, &[("content-type", "application/json")]);
            if let Some(recorder) = &recorder {
                recorder.request(request_headers.clone(), &body).await?;
            }
            let Some(response) = upstream::send(
                &UpstreamCall {
                    label: "OpenAI Chat",
                    model: &request.model.model_id,
                    url: &config.request_url,
                },
                &outbound,
                |client| client.post(&config.request_url)
                    .bearer_auth(&config.api_key).headers(config.custom_headers.clone()).json(&body),
                &cancellation,
                recorder.as_ref(),
            ).await? else { return };
            yield ModelEvent::Start { model_call_id: call_id };
            let chunk_recorder = recorder.clone();
            let chunks = response.bytes_stream()
                .map(|chunk| chunk.map_err(Error::from))
                .then(move |chunk| {
                    let recorder = chunk_recorder.clone();
                    async move {
                        let chunk = chunk?;
                        if let Some(recorder) = recorder { recorder.response_chunk(&chunk).await?; }
                        Ok::<_, Error>(chunk)
                    }
                });
            let source = chunks.eventsource();
            futures_util::pin_mut!(source);
            let mut text_open = false;
            let mut thinking_open = false;
            let mut reasoning = String::new();
            let mut tools = BTreeMap::<usize, ChatToolState>::new();
            let mut final_usage = None;
            let mut finish = None;
            let mut saw_done_marker = false;
            let mut loop_iteration: u64 = 0;
            loop {
                loop_iteration += 1;
                let event = tokio::select! {
                    _ = cancellation.cancelled() => {
                        tracing::debug!(
                            iteration = loop_iteration,
                            saw_done_marker,
                            tool_count = tools.len(),
                            "OpenAI Chat stream cancelled"
                        );
                        return;
                    }
                    event = source.next() => event,
                };
                let Some(event) = event else {
                    tracing::debug!(
                        iteration = loop_iteration,
                        saw_done_marker,
                        "OpenAI Chat SSE stream ended"
                    );
                    break;
                };
                let event = event.map_err(|error| {
                    tracing::debug!(iteration = loop_iteration, error = %error, "OpenAI Chat SSE event failed");
                    map_sse_error("OpenAI Chat", error)
                })?;
                if event.data == "[DONE]" { saw_done_marker = true; break; }
                let value: Value = serde_json::from_str(&event.data)?;
                if let Some(error) = provider_event_error("OpenAI Chat", &value) {
                    Err(error)?;
                }
                if let Some(usage) = value.get("usage").filter(|value| !value.is_null()) {
                    final_usage = Some(openai_usage(usage));
                }
                let Some(choice) = value.get("choices").and_then(Value::as_array).and_then(|values| values.first()) else { continue; };
                let delta = choice.get("delta").unwrap_or(&Value::Null);
                if let Some(reasoning_delta) = delta.get("reasoning_content").or_else(|| delta.get("reasoning")).and_then(Value::as_str).filter(|text| !text.is_empty()) {
                    if !thinking_open { thinking_open = true; yield ModelEvent::ThinkingStart; }
                    reasoning.push_str(reasoning_delta);
                    yield ModelEvent::ThinkingDelta(reasoning_delta.into());
                }
                if let Some(content) = delta.get("content").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                    if thinking_open { thinking_open = false; yield ModelEvent::ThinkingEnd; }
                    if !text_open { text_open = true; yield ModelEvent::TextStart; }
                    yield ModelEvent::TextDelta(content.into());
                }
                if let Some(tool_deltas) = delta.get("tool_calls").and_then(Value::as_array) {
                    for (position, tool) in tool_deltas.iter().enumerate() {
                        let index = tool.get("index").and_then(Value::as_u64).map_or(position, |index| index as usize);
                        let id = tool.get("id").and_then(Value::as_str);
                        let function = tool.get("function").unwrap_or(&Value::Null);
                        let name = function.get("name").and_then(Value::as_str);
                        let arguments = function.get("arguments").and_then(Value::as_str);
                        for event in update_chat_tool(index, id, name, arguments, &mut tools) { yield event; }
                    }
                }
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    finish = Some(map_finish(reason, !tools.is_empty()));
                }
            }
            if thinking_open { yield ModelEvent::ThinkingEnd; }
            if text_open { yield ModelEvent::TextEnd; }
            for (index, tool) in &mut tools {
                if !tool.started {
                    if tool.name.is_empty() {
                        Err(Error::Provider("OpenAI Chat tool call is missing name".into()))?;
                    }
                    if tool.call_id.is_empty() {
                        tool.call_id = format!("call-{index}");
                    }
                    tool.started = true;
                    yield ModelEvent::ToolCallStart { index: *index, call_id: tool.call_id.clone(), name: tool.name.clone() };
                    if !tool.arguments.is_empty() {
                        tool.emitted_arguments = tool.arguments.len();
                        yield ModelEvent::ToolCallArgumentsDelta { index: *index, delta: tool.arguments.clone() };
                    }
                }
                yield ModelEvent::ToolCallEnd { index: *index };
            }
            if let Some(usage) = final_usage { yield ModelEvent::Usage(usage); }
            if !reasoning.is_empty() {
                yield ModelEvent::ProviderReplayState(crate::model::ProviderReplayState {
                    provider_kind: "openai_chat".into(),
                    value: json!({"reasoning_content": reasoning}),
                });
            }
            let finish = finish.or_else(|| saw_done_marker.then_some(if tools.is_empty() { FinishReason::Stop } else { FinishReason::ToolUse }))
                .ok_or_else(|| Error::Provider("OpenAI Chat stream ended without finish_reason".into()))?;
            yield ModelEvent::Done(finish);
        })
    }
}

fn apply_model(
    body: &mut Value,
    model: &crate::model::ModelSpec,
    route_max_output_tokens: Option<u64>,
) -> Result<()> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| Error::Provider("OpenAI Chat request body is not an object".into()))?;
    if let Some(max) = model.max_output_tokens.or(route_max_output_tokens) {
        object.insert("max_completion_tokens".into(), json!(max));
    }
    if let Some(effort) = &model.reasoning.effort {
        object.insert("reasoning_effort".into(), json!(effort));
    }
    if model.latency == ModelLatency::Fast {
        object.insert("service_tier".into(), json!("fast"));
    }
    Ok(())
}

fn openai_chat_messages(instructions: &str, messages: &[ProjectedMessage]) -> Result<Vec<Value>> {
    let mut output = Vec::with_capacity(messages.len() + usize::from(!instructions.is_empty()));
    if !instructions.is_empty() {
        output.push(json!({"role": "system", "content": instructions}));
    }
    for message in messages {
        let mut value = Map::new();
        value.insert(
            "role".into(),
            Value::String(role_name(&message.role).into()),
        );
        match &message.content {
            ProjectedContent::Parts(parts) => {
                let content = chat_content(&message.role, parts)?;
                // Some Chat Completions upstreams reject empty-string content.
                match &content {
                    Value::String(text) if text.is_empty() => continue,
                    Value::Array(parts) if parts.is_empty() => continue,
                    _ => {}
                }
                value.insert("content".into(), content);
            }
            ProjectedContent::Assistant {
                text,
                replay_state,
                calls,
                ..
            } => {
                let replay_reasoning = replay_state
                    .as_ref()
                    .filter(|state| state.provider_kind == "openai_chat")
                    .and_then(|state| state.value.get("reasoning_content"))
                    .and_then(Value::as_str)
                    .filter(|reasoning| !reasoning.is_empty());

                // Chat Completions rejects an empty assistant content string, and some
                // providers (notably DeepSeek) reject an assistant message that carries a
                // null content with no tool_calls. A pure reasoning-only assistant turn
                // (no text and no tool_calls) must therefore be omitted entirely; the
                // reasoning is still attached below whenever the turn also has text and/or
                // tool_calls.
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                value.insert(
                    "content".into(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        Value::String(text.clone())
                    },
                );
                if let Some(reasoning) = replay_reasoning {
                    value.insert("reasoning_content".into(), Value::String(reasoning.into()));
                }
                if !calls.is_empty() {
                    value.insert(
                        "tool_calls".into(),
                        Value::Array(
                            calls
                                .iter()
                                .map(openai_tool_call)
                                .collect::<Result<Vec<_>>>()?,
                        ),
                    );
                }
            }
            ProjectedContent::ToolResult(result) => {
                value.insert(
                    "content".into(),
                    if result.provider_parts.is_empty() {
                        Value::String(result.content.clone())
                    } else {
                        chat_content(&Role::User, &result.provider_parts)?
                    },
                );
                value.insert("tool_call_id".into(), Value::String(result.call_id.clone()));
            }
        }
        output.push(Value::Object(value));
    }
    Ok(output)
}

fn chat_content(_role: &Role, parts: &[ContentPart]) -> Result<Value> {
    let mut text = String::new();
    let mut only_text = true;
    for part in parts {
        match part {
            ContentPart::Text { text: part } => text.push_str(part),
            ContentPart::Image { .. } => {
                only_text = false;
                break;
            }
        }
    }
    if only_text {
        return Ok(Value::String(text));
    }
    Ok(Value::Array(
        parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => Ok(json!({"type":"text", "text":text})),
                ContentPart::Image { mime_type, data } => Ok(json!({
                    "type":"image_url",
                    "image_url":{"url":format!(
                        "data:{mime_type};base64,{}",
                        STANDARD.encode(data)
                    )},
                })),
            })
            .collect::<Result<Vec<_>>>()?,
    ))
}

fn openai_tool_call(call: &ToolCallContent) -> Result<Value> {
    Ok(json!({
        "id": call.call_id,
        "type": "function",
        "function": {
            "name": call.name,
            "arguments": serde_json::to_string(&call.arguments)?,
        }
    }))
}

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn map_finish(value: &str, has_tools: bool) -> FinishReason {
    match value {
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::Stop,
        // Observed tool calls outrank ordinary or unknown stop labels because
        // OpenAI-compatible servers commonly emit tools with `stop`.
        _ if has_tools => FinishReason::ToolUse,
        _ => FinishReason::Stop,
    }
}

fn update_chat_tool(
    index: usize,
    call_id: Option<&str>,
    name: Option<&str>,
    arguments: Option<&str>,
    tools: &mut BTreeMap<usize, ChatToolState>,
) -> Vec<ModelEvent> {
    let tool = tools.entry(index).or_default();
    if let Some(call_id) = call_id {
        merge_chat_fragment(&mut tool.call_id, call_id);
    }
    if let Some(name) = name {
        merge_chat_fragment(&mut tool.name, name);
    }
    if let Some(arguments) = arguments {
        merge_chat_fragment(&mut tool.arguments, arguments);
    }

    let mut events = Vec::new();
    if !tool.started && !tool.call_id.is_empty() && !tool.name.is_empty() {
        tool.started = true;
        events.push(ModelEvent::ToolCallStart {
            index,
            call_id: tool.call_id.clone(),
            name: tool.name.clone(),
        });
    }
    if tool.started && tool.emitted_arguments < tool.arguments.len() {
        let delta = tool.arguments[tool.emitted_arguments..].to_string();
        tool.emitted_arguments = tool.arguments.len();
        events.push(ModelEvent::ToolCallArgumentsDelta { index, delta });
    }
    events
}

/// 归并同一个字段的分片。
///
/// 上游有两种用法：多数只发增量，少数每个分片都重复下发完整的当前值（这个函数就是为后者
/// 存在的）。判据只用"整串的前后包含"这种确定关系：
///   * 与当前值相同 —— 重复下发整串，忽略；
///   * 当前值是分片的严格前缀 —— 分片是新的完整值，替换；
///   * 分片是当前值的严格前缀 —— 分片是旧的完整值（迟到的重复），忽略；
///   * 其余（含"分片恰好等于当前值的末尾"）—— 按增量追加。
///
/// 最后一条是重点：以前 `target.ends_with(fragment)` 会把合法的重复增量当重复下发吞掉
/// （参数 JSON 里 `{`、`"`、`a` 这类片段挨着重复是常态），而更容易被重复下发的
/// `arguments` 又完全没走这套归并，于是整串参数被拼两遍、拼出 `{...}{...}` 这种非法 JSON。
fn merge_chat_fragment(target: &mut String, fragment: &str) {
    if target.starts_with(fragment) {
        return;
    }
    if fragment.starts_with(target.as_str()) {
        *target = fragment.into();
        return;
    }
    target.push_str(fragment);
}

pub(crate) fn openai_usage(value: &Value) -> Usage {
    let input_tokens = value.get("prompt_tokens").and_then(Value::as_u64);
    Usage {
        input_tokens,
        context_input_tokens: input_tokens,
        output_tokens: value.get("completion_tokens").and_then(Value::as_u64),
        total_tokens: value.get("total_tokens").and_then(Value::as_u64),
        cache_read_tokens: value
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        cache_write_tokens: None,
        reasoning_tokens: value
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProviderReplayState;
    use crate::provider::testing;

    /// 结束条件一：`[DONE]` 就是收尾信号，即使没有 finish_reason 也要正常结束。
    #[tokio::test]
    async fn a_done_marker_ends_the_stream_even_without_a_finish_reason() {
        let url = testing::sse_upstream(
            "/v1/chat/completions",
            vec![
                json!({"choices":[{"delta":{"content":"你好"}}]}),
                json!({"choices":[{"delta":{},"finish_reason":null}]}),
                json!("[DONE]"),
            ],
        )
        .await;
        let provider = OpenAiChatProvider::new(
            crate::network::OutboundClient::fixed(reqwest::Client::new()),
            testing::provider_config(crate::config::ProviderKind::OpenAiChat, url),
        );

        let events = testing::drain(provider.stream(
            testing::invocation("gpt-5.1"),
            tokio_util::sync::CancellationToken::new(),
        ))
        .await
        .unwrap_or_else(|error| panic!("带 [DONE] 的流必须正常收尾: {error}"));

        assert_eq!(testing::text_of(&events), "你好");
        assert_eq!(testing::finish_of(&events), Some(FinishReason::Stop));
    }

    /// 结束条件二：既没有 `[DONE]` 也没有 finish_reason 就断开——必须报错。把这种情况当
    /// 成功会让"被截断的回答"看起来像完整回答。
    #[tokio::test]
    async fn a_stream_that_ends_without_done_or_a_finish_reason_is_an_error() {
        let url = testing::sse_upstream(
            "/v1/chat/completions",
            vec![json!({"choices":[{"delta":{"content":"断"}}]})],
        )
        .await;
        let provider = OpenAiChatProvider::new(
            crate::network::OutboundClient::fixed(reqwest::Client::new()),
            testing::provider_config(crate::config::ProviderKind::OpenAiChat, url),
        );

        let error = testing::drain(provider.stream(
            testing::invocation("gpt-5.1"),
            tokio_util::sync::CancellationToken::new(),
        ))
        .await
        .expect_err("没有收尾信号的流必须报错");

        assert!(
            error.to_string().contains("without finish_reason"),
            "{error}"
        );
    }

    fn assistant_message(
        text: &str,
        reasoning: Option<&str>,
        calls: Vec<ToolCallContent>,
    ) -> ProjectedMessage {
        ProjectedMessage {
            message_id: "assistant-1".into(),
            role: Role::Assistant,
            content: ProjectedContent::Assistant {
                text: text.into(),
                thinking: String::new(),
                replay_state: reasoning.map(|reasoning| ProviderReplayState {
                    provider_kind: "openai_chat".into(),
                    value: json!({"reasoning_content": reasoning}),
                }),
                calls,
            },
        }
    }

    #[test]
    fn observed_tool_calls_outrank_ordinary_stop_reasons() {
        assert_eq!(map_finish("stop", true), FinishReason::ToolUse);
        assert_eq!(map_finish("", true), FinishReason::ToolUse);
    }

    #[test]
    fn content_filter_never_executes_observed_tool_calls() {
        assert_eq!(map_finish("content_filter", true), FinishReason::Stop);
        assert_eq!(map_finish("content_filter", false), FinishReason::Stop);
    }

    #[test]
    fn finish_reason_mapping_without_tool_calls_is_unchanged() {
        assert_eq!(map_finish("stop", false), FinishReason::Stop);
        assert_eq!(map_finish("content_filter", false), FinishReason::Stop);
        assert_eq!(map_finish("", false), FinishReason::Stop);
        assert_eq!(map_finish("tool_calls", false), FinishReason::ToolUse);
        assert_eq!(map_finish("function_call", false), FinishReason::ToolUse);
    }

    #[test]
    fn a_truncated_response_stays_truncated_even_with_tool_calls() {
        assert_eq!(map_finish("length", true), FinishReason::Length);
        assert_eq!(map_finish("length", false), FinishReason::Length);
    }

    #[test]
    fn assistant_with_only_reasoning_content_is_omitted() {
        let message = assistant_message("", Some("only thinking"), Vec::new());
        let messages = openai_chat_messages("", &[message]).expect("messages should build");
        assert!(
            messages.is_empty(),
            "reasoning-only assistant turn must be omitted, got: {messages:?}"
        );
    }

    #[test]
    fn assistant_with_text_and_reasoning_content_is_kept() {
        let message = assistant_message("hello", Some("reasoning"), Vec::new());
        let messages = openai_chat_messages("", &[message]).expect("messages should build");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], json!("assistant"));
        assert_eq!(messages[0]["content"], json!("hello"));
        assert_eq!(messages[0]["reasoning_content"], json!("reasoning"));
    }

    #[test]
    fn assistant_with_tool_calls_and_reasoning_content_is_kept() {
        let message = assistant_message(
            "",
            Some("reasoning"),
            vec![ToolCallContent {
                index: 0,
                call_id: "c1".into(),
                name: "f".into(),
                arguments: json!({}),
            }],
        );
        let messages = openai_chat_messages("", &[message]).expect("messages should build");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], json!("assistant"));
        assert_eq!(messages[0]["content"], Value::Null);
        assert_eq!(messages[0]["reasoning_content"], json!("reasoning"));
        assert_eq!(messages[0]["tool_calls"][0]["id"], json!("c1"));
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], json!("f"));
    }

    #[test]
    fn empty_parts_content_is_omitted() {
        let message = ProjectedMessage {
            message_id: "user-1".into(),
            role: Role::User,
            content: ProjectedContent::Parts(vec![ContentPart::Text {
                text: String::new(),
            }]),
        };
        let messages = openai_chat_messages("", &[message]).expect("messages should build");
        assert!(
            messages.is_empty(),
            "empty Parts content must be omitted, got: {messages:?}"
        );
    }

    fn chat_tools(
        fragments: &[(Option<&str>, Option<&str>, Option<&str>)],
    ) -> BTreeMap<usize, ChatToolState> {
        let mut tools = BTreeMap::new();
        for (call_id, name, arguments) in fragments {
            update_chat_tool(0, *call_id, *name, *arguments, &mut tools);
        }
        tools
    }

    /// 重复下发整串参数的上游不能把参数拼两遍：以前 `arguments` 直接 `push_str`，
    /// 结果是 `{...}{...}` 这种非法 JSON，整轮工具白跑。
    #[test]
    fn a_repeated_whole_arguments_value_is_ignored() {
        let tools = chat_tools(&[
            (Some("call-1"), Some("read"), Some(r#"{"path":"a.txt"}"#)),
            (Some("call-1"), Some("read"), Some(r#"{"path":"a.txt"}"#)),
        ]);

        assert_eq!(tools[&0].arguments, r#"{"path":"a.txt"}"#);
    }

    /// 分片式下发里，分片刚好延伸出完整值时替换而不是追加。
    #[test]
    fn a_full_arguments_snapshot_replaces_the_accumulated_fragments() {
        let tools = chat_tools(&[
            (Some("call-1"), Some("read"), Some(r#"{"path":"#)),
            (Some("call-1"), Some("read"), Some(r#"{"path":"a.txt"}"#)),
        ]);

        assert_eq!(tools[&0].arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(tools[&0].name, "read");
        assert_eq!(tools[&0].call_id, "call-1");
    }

    /// 正常增量按顺序拼起来，拼到最后是合法 JSON，且发给模型的分片正好是增量。
    #[test]
    fn incremental_fragments_are_appended_in_order() {
        let mut tools = BTreeMap::new();
        update_chat_tool(
            0,
            Some("call-1"),
            Some("read"),
            Some(r#"{"path""#),
            &mut tools,
        );
        let deltas = update_chat_tool(0, None, None, Some(r#":"a.txt"}"#), &mut tools);

        assert_eq!(tools[&0].arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(
            deltas,
            vec![ModelEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: r#":"a.txt"}"#.into(),
            }]
        );
        serde_json::from_str::<Value>(&tools[&0].arguments).expect("拼出来的参数必须是合法 JSON");
    }

    /// 合法地重复末尾分片必须照旧追加：以前的 `ends_with` 会把这种增量当"重复下发"
    /// 吞掉，拼出的 JSON 就少字符。
    #[test]
    fn a_fragment_that_merely_repeats_the_tail_is_still_appended() {
        let mut target = "abc".to_string();
        merge_chat_fragment(&mut target, "c");
        assert_eq!(target, "abcc");

        // 正常增量流里的相邻重复：`{"path":"a` 之后再来一个 `a`。
        let tools = chat_tools(&[
            (Some("call-1"), Some("read"), Some(r#"{"path":"a"#)),
            (None, None, Some("a")),
        ]);
        assert_eq!(tools[&0].arguments, r#"{"path":"aa"#);
    }

    /// 与当前值完全相同的分片只能理解成"重复下发整串"：`arguments` 拼两遍会得到非法
    /// JSON，而真正逐字符重复的分片拼出来同样不是合法 JSON，所以这里以"不重复"为准
    /// （见上面那条整串重复的用例）。
    #[test]
    fn an_identical_fragment_is_treated_as_a_resend() {
        let mut target = r#"{"path":"a.txt"}"#.to_string();
        merge_chat_fragment(&mut target, r#"{"path":"a.txt"}"#);
        assert_eq!(target, r#"{"path":"a.txt"}"#);
    }

    /// 旧值迟到（分片是当前值的前缀）时忽略，不能把已积累的值截短。
    #[test]
    fn a_late_older_snapshot_does_not_truncate_the_field() {
        let mut target = "call-abc".to_string();
        merge_chat_fragment(&mut target, "call-");
        assert_eq!(target, "call-abc");
    }
}
