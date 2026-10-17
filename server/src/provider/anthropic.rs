//! Implements the Anthropic provider adapter.
use async_stream::try_stream;
use base64::{engine::general_purpose::STANDARD, Engine};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::{
    config::ProviderConfig,
    model::{ContentPart, ModelInvocation, ProjectedContent, ProjectedMessage, Role, Usage},
    network::OutboundClient,
    Error, Result,
};

use super::{
    map_sse_error, merge_extra_params, provider_event_error, recorder::recorded_headers,
    CallRecorder, FinishReason, ModelEvent, Provider, ProviderStream,
};
use crate::upstream::{self, UpstreamCall};

/// 模型配置与调用方都没给 `max_tokens` 时的兜底。
///
/// 取在役 Anthropic 模型都接受的最小公共上限（claude-3-haiku / claude-2.1 这类老模型的
/// 上限就是 4096），而不是按最大的模型取值：`GET /v1/models` 不返回输出上限，模型发现
/// 补不上真实值，而 400 不可重试、也不匹配上下文溢出，用户只会看到"max_tokens 超限"，
/// 联想不到是"留空"造成的。Codex 路径（`codex/upstream.rs`）的同类兜底也是 4096。
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 4_096;

pub struct AnthropicProvider {
    outbound: OutboundClient,
    config: ProviderConfig,
    recorder: Option<CallRecorder>,
}

impl AnthropicProvider {
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

impl Provider for AnthropicProvider {
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
            let mut messages = anthropic_messages(&request.history)?;
            mark_cache_breakpoint(&mut messages);
            let system = if request.prompt.instructions.is_empty() {
                Value::String(String::new())
            } else {
                json!([{
                    "type": "text",
                    "text": request.prompt.instructions,
                    "cache_control": {"type": "ephemeral"}
                }])
            };
            let max_tokens = resolve_max_output_tokens(&request.model, config.max_output_tokens);
            let mut body = json!({
                "model": request.model.model_id, "system": system, "messages": messages,
                "max_tokens": max_tokens, "stream": true
            });
            if !request.prompt.tools.is_empty() {
                let tool_count = request.prompt.tools.len();
                body["tools"] = json!(request.prompt.tools.iter().enumerate().map(|(index, tool)| {
                    let mut value = json!({
                        "name": tool.name, "description": tool.description, "input_schema": tool.parameters
                    });
                    if index + 1 == tool_count {
                        value["cache_control"] = json!({"type": "ephemeral"});
                    }
                    value
                }).collect::<Vec<_>>());
            }
            apply_model(&mut body, &request.model, max_tokens)?;
            merge_extra_params(&mut body, &request.model.extra_params)?;
            let request_headers = recorded_headers(
                &config,
                &[("content-type", "application/json"), ("anthropic-version", "2023-06-01")],
            );
            if let Some(recorder) = &recorder {
                recorder.request(request_headers.clone(), &body).await?;
            }
            let Some(response) = upstream::send(
                &UpstreamCall {
                    label: "Anthropic",
                    model: &request.model.model_id,
                    url: &config.request_url,
                },
                &outbound,
                |client| client.post(&config.request_url)
                    .header("x-api-key", &config.api_key).header("anthropic-version", "2023-06-01")
                    .headers(config.custom_headers.clone())
                    .json(&body),
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
            let mut block_types = std::collections::BTreeMap::<usize, String>::new();
            let mut thinking_text = std::collections::HashMap::<usize, String>::new();
            let mut thinking_signatures = std::collections::HashMap::<usize, String>::new();
            let mut thinking_blocks = Vec::new();
            let mut finish = None;
            let mut saw_tool = false;
            let mut terminal = false;
            let mut final_usage = None::<Usage>;
            while let Some(event) = tokio::select! {
                _ = cancellation.cancelled() => { return; }
                event = source.next() => event,
            } {
                let event = event.map_err(|error| map_sse_error("Anthropic", error))?;
                let value: Value = serde_json::from_str(&event.data)?;
                if let Some(error) = provider_event_error("Anthropic", &value) {
                    Err(error)?;
                }
                let data_kind = value.get("type").and_then(Value::as_str);
                let kind = match event.event.as_str() {
                    "" | "message" => data_kind.unwrap_or(event.event.as_str()),
                    kind => kind,
                };
                match kind {
                    "message_start" => if let Some(usage) = value.pointer("/message/usage") {
                        merge_usage(final_usage.get_or_insert_default(), anthropic_usage(usage));
                    },
                    "content_block_start" => {
                        let index = required_u64(&value, "index")? as usize;
                        let block = value.get("content_block").unwrap_or(&Value::Null);
                        let kind = required_string(block, "type")?;
                        block_types.insert(index, kind.into());
                        match kind {
                            "text" => yield ModelEvent::TextStart,
                            "thinking" => {
                                thinking_text.insert(index, String::new());
                                thinking_signatures.insert(index, String::new());
                                yield ModelEvent::ThinkingStart;
                            }
                            "redacted_thinking" => thinking_blocks.push(block.clone()),
                            "tool_use" => {
                                saw_tool = true;
                                yield ModelEvent::ToolCallStart {
                                    index,
                                    call_id: required_string(block, "id")?.into(),
                                    name: required_string(block, "name")?.into(),
                                };
                            }
                            _ => {}
                        }
                    }
                    "content_block_delta" => {
                        let index = required_u64(&value, "index")? as usize;
                        let delta = value.get("delta").unwrap_or(&Value::Null);
                        let delta_kind = required_string(delta, "type")?;
                        if let std::collections::btree_map::Entry::Vacant(entry) = block_types.entry(index) {
                            match delta_kind {
                                "text_delta" => {
                                    entry.insert("text".into());
                                    yield ModelEvent::TextStart;
                                }
                                "thinking_delta" | "signature_delta" => {
                                    entry.insert("thinking".into());
                                    thinking_text.insert(index, String::new());
                                    thinking_signatures.insert(index, String::new());
                                    yield ModelEvent::ThinkingStart;
                                }
                                _ => {}
                            }
                        }
                        match delta_kind {
                            "text_delta" => if let Some(text) = delta.get("text").and_then(Value::as_str) { yield ModelEvent::TextDelta(text.into()); },
                            "thinking_delta" => if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                thinking_text.entry(index).or_default().push_str(text);
                                yield ModelEvent::ThinkingDelta(text.into());
                            },
                            "signature_delta" => if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                                thinking_signatures.entry(index).or_default().push_str(signature);
                            },
                            "input_json_delta" => if let Some(text) = delta.get("partial_json").and_then(Value::as_str) { yield ModelEvent::ToolCallArgumentsDelta { index, delta: text.into() }; },
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        let index = required_u64(&value, "index")? as usize;
                        if let Some(kind) = block_types.remove(&index) {
                            for event in close_anthropic_block(index, &kind, &mut thinking_text, &mut thinking_signatures, &mut thinking_blocks) {
                                yield event;
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(usage) = value.get("usage") {
                            merge_usage(final_usage.get_or_insert_default(), anthropic_usage(usage));
                        }
                        finish = match value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                            Some("tool_use") => Some(FinishReason::ToolUse),
                            Some("max_tokens" | "model_context_window_exceeded") => Some(FinishReason::Length),
                            Some("end_turn" | "stop_sequence" | "pause_turn" | "refusal") => Some(FinishReason::Stop),
                            None => finish,
                            Some(_) => Some(if saw_tool { FinishReason::ToolUse } else { FinishReason::Stop }),
                        };
                    }
                    "message_stop" => {
                        for (index, kind) in std::mem::take(&mut block_types) {
                            for event in close_anthropic_block(index, &kind, &mut thinking_text, &mut thinking_signatures, &mut thinking_blocks) {
                                yield event;
                            }
                        }
                        terminal = true;
                        if !thinking_blocks.is_empty() {
                            yield ModelEvent::ProviderReplayState(
                                crate::model::ProviderReplayState {
                                    provider_kind: "anthropic".into(),
                                    value: json!({"blocks": std::mem::take(&mut thinking_blocks)}),
                                },
                            );
                        }
                        if let Some(usage) = final_usage {
                            yield ModelEvent::Usage(usage);
                        }
                        let finish = match finish {
                            Some(FinishReason::Length) => FinishReason::Length,
                            _ if saw_tool => FinishReason::ToolUse,
                            Some(finish) => finish,
                            None => FinishReason::Stop,
                        };
                        yield ModelEvent::Done(finish);
                    }
                    _ => {}
                }
            }
            if !terminal && finish.is_some() {
                for (index, kind) in std::mem::take(&mut block_types) {
                    for event in close_anthropic_block(index, &kind, &mut thinking_text, &mut thinking_signatures, &mut thinking_blocks) {
                        yield event;
                    }
                }
                if !thinking_blocks.is_empty() {
                    yield ModelEvent::ProviderReplayState(crate::model::ProviderReplayState {
                        provider_kind: "anthropic".into(),
                        value: json!({"blocks": std::mem::take(&mut thinking_blocks)}),
                    });
                }
                if let Some(usage) = final_usage { yield ModelEvent::Usage(usage); }
                terminal = true;
                let finish = match finish {
                    Some(FinishReason::Length) => FinishReason::Length,
                    _ if saw_tool => FinishReason::ToolUse,
                    Some(finish) => finish,
                    None => FinishReason::Stop,
                };
                yield ModelEvent::Done(finish);
            }
            if !terminal {
                Err(Error::Provider("Anthropic stream ended without message_stop".into()))?;
            }
        })
    }
}

fn close_anthropic_block(
    index: usize,
    kind: &str,
    thinking_text: &mut std::collections::HashMap<usize, String>,
    thinking_signatures: &mut std::collections::HashMap<usize, String>,
    thinking_blocks: &mut Vec<Value>,
) -> Vec<ModelEvent> {
    match kind {
        "text" => vec![ModelEvent::TextEnd],
        "thinking" => {
            let thinking = thinking_text.remove(&index).unwrap_or_default();
            let signature = thinking_signatures.remove(&index).unwrap_or_default();
            if !signature.is_empty() {
                thinking_blocks.push(json!({
                    "type": "thinking",
                    "thinking": thinking,
                    "signature": signature,
                }));
            }
            vec![ModelEvent::ThinkingEnd]
        }
        "tool_use" => vec![ModelEvent::ToolCallEnd { index }],
        _ => Vec::new(),
    }
}

/// 本轮请求的 `max_tokens`：调用方指定的优先（压缩/摘要这类调用会按需收紧），
/// 其次是模型配置，都没有才用兜底。
fn resolve_max_output_tokens(model: &crate::model::ModelSpec, route_max: Option<u64>) -> u64 {
    model
        .max_output_tokens
        .or(route_max)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

fn apply_model(body: &mut Value, model: &crate::model::ModelSpec, max_tokens: u64) -> Result<()> {
    // 末轮是"没有签名 thinking 的工具续轮"时不能开 thinking:上游会直接 400,而 4xx
    // 不可重试,这条历史又一直留在会话里,后续每个请求都会重复同一个 400。
    // 同一条规则在 codex/bridge/request.rs 里已经守着 Codex 那条路。
    let supports_thinking = match body.get("messages").and_then(Value::as_array) {
        Some(messages) => trailing_turn_supports_thinking(messages),
        None => true,
    };
    let object = body
        .as_object_mut()
        .ok_or_else(|| Error::Provider("Anthropic request body is not an object".into()))?;
    if model.reasoning.is_enabled() && supports_thinking {
        object.insert(
            "thinking".into(),
            thinking_config(&model.reasoning, max_tokens),
        );
    }
    // 决定不开 thinking 时,思考强度的 output_config 也得一起去掉:留着它等于
    // "说了不算",上游可以据此判定请求自相矛盾。用 `enabled + budget_tokens` 形态时同理:
    // `effort` 只属于 adaptive thinking,两种形态一起发就是两个互相冲突的强度。
    if supports_thinking && model.reasoning.thinking_budget_tokens.is_none() {
        if let Some(effort) = &model.reasoning.effort {
            object.insert("output_config".into(), json!({"effort":effort}));
        }
    }
    Ok(())
}

/// Anthropic 要求 `budget_tokens` 不小于 1024；`codex/upstream.rs` 对同一个字段做同样的夹取。
const MIN_THINKING_BUDGET: u64 = 1_024;

/// 思考配置。界面上的"思考预算 Token"有值时发 `{"type":"enabled","budget_tokens":N}`——
/// 这是"按预算思考"的形态，也是 Cursor/Devin 路径上唯一会读这个字段的地方；留空才发
/// adaptive（上游按强度自适应）。预算必须小于 `max_tokens`，否则上游直接 400。
fn thinking_config(reasoning: &crate::model::ReasoningSpec, max_tokens: u64) -> Value {
    match reasoning.thinking_budget_tokens {
        Some(budget) => json!({
            "type": "enabled",
            "budget_tokens": budget.max(MIN_THINKING_BUDGET).min(max_tokens.saturating_sub(1)),
        }),
        None => json!({"type":"adaptive", "display":"summarized"}),
    }
}

/// 末条 assistant 带 `tool_use` 却没有 `thinking` / `redacted_thinking` 块时,
/// Anthropic 不接受这一轮继续开 thinking:工具续轮必须回放上一条助手消息的签名 thinking。
/// 这种历史是正常的——上一轮可能由 OpenAI 模型产出,或者签名不是本仓库的 envelope。
fn trailing_turn_supports_thinking(messages: &[Value]) -> bool {
    let Some(blocks) = messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return true;
    };
    if !blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
    {
        return true;
    }
    blocks.iter().any(|block| {
        matches!(
            block.get("type").and_then(Value::as_str),
            Some("thinking" | "redacted_thinking")
        )
    })
}

fn merge_usage(total: &mut Usage, update: Usage) {
    merge_usage_field(&mut total.input_tokens, update.input_tokens);
    merge_usage_field(&mut total.context_input_tokens, update.context_input_tokens);
    merge_usage_field(&mut total.output_tokens, update.output_tokens);
    merge_usage_field(&mut total.total_tokens, update.total_tokens);
    merge_usage_field(&mut total.cache_read_tokens, update.cache_read_tokens);
    merge_usage_field(&mut total.cache_write_tokens, update.cache_write_tokens);
    merge_usage_field(&mut total.reasoning_tokens, update.reasoning_tokens);
    if let Some(request_tokens) = total
        .context_input_tokens
        .zip(total.output_tokens)
        .and_then(|(input, output)| input.checked_add(output))
    {
        total.total_tokens = Some(request_tokens);
    }
}

fn merge_usage_field(total: &mut Option<u64>, update: Option<u64>) {
    if let Some(update) = update {
        *total = Some(total.map_or(update, |current| current.max(update)));
    }
}

fn anthropic_messages(messages: &[ProjectedMessage]) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for message in messages {
        match &message.content {
            ProjectedContent::Parts(parts) => {
                let content = anthropic_parts(&message.role, parts)?;
                if !content.is_empty() {
                    push_anthropic(&mut output, role_name(&message.role), content);
                }
            }
            ProjectedContent::ToolResult(result) => {
                let content = if result.provider_parts.is_empty() {
                    Value::String(result.content.clone())
                } else {
                    Value::Array(anthropic_parts(&Role::User, &result.provider_parts)?)
                };
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": result.call_id,
                    "content": content,
                });
                // 失败的工具调用要标出来:不标的话模型把它当普通结果继续推理,
                // 于是"命令没跑成"被理解成"命令成功了但没输出"。
                if result.is_error {
                    block["is_error"] = json!(true);
                }
                push_anthropic(&mut output, "user", vec![block]);
            }
            ProjectedContent::Assistant {
                text,
                replay_state,
                calls,
                ..
            } => {
                let mut content = Vec::new();
                if let Some(blocks) = replay_state
                    .as_ref()
                    .filter(|state| state.provider_kind == "anthropic")
                    .and_then(|state| state.value.get("blocks"))
                    .and_then(Value::as_array)
                {
                    content.extend(blocks.iter().cloned());
                }
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
                content.extend(calls.iter().map(|call| {
                    json!({
                        "type": "tool_use",
                        "id": call.call_id,
                        "name": call.name,
                        "input": call.arguments,
                    })
                }));
                if !content.is_empty() {
                    push_anthropic(&mut output, "assistant", content);
                }
            }
        }
    }
    Ok(output)
}

fn mark_cache_breakpoint(messages: &mut [Value]) {
    let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return;
    };
    let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in content.iter_mut().rev() {
        let kind = block.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("text" | "image" | "tool_result")) {
            continue;
        }
        if let Some(block) = block.as_object_mut() {
            block.insert("cache_control".into(), json!({"type": "ephemeral"}));
            return;
        }
    }
}

fn anthropic_parts(role: &Role, parts: &[ContentPart]) -> Result<Vec<Value>> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } if text.is_empty() => None,
            ContentPart::Text { text } => Some(Ok(json!({"type":"text", "text":text}))),
            ContentPart::Image { mime_type, data } if *role == Role::User => Some(Ok(json!({
                "type":"image",
                "source":{
                    "type":"base64",
                    "media_type":mime_type,
                    "data":STANDARD.encode(data),
                },
            }))),
            ContentPart::Image { .. } => Some(Err(Error::Protocol(
                "Anthropic only accepts images in user messages".into(),
            ))),
        })
        .collect()
}

fn push_anthropic(output: &mut Vec<Value>, role: &str, mut content: Vec<Value>) {
    if let Some(last) = output
        .last_mut()
        .filter(|last| last.get("role").and_then(Value::as_str) == Some(role))
    {
        if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
            existing.append(&mut content);
            return;
        }
    }
    output.push(json!({"role":role, "content":content}));
}

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "user",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user",
    }
}

fn required_string<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Provider(format!("Anthropic event is missing {name}")))
}

fn required_u64(value: &Value, name: &str) -> Result<u64> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::Provider(format!("Anthropic event is missing {name}")))
}

fn anthropic_usage(value: &Value) -> Usage {
    let input_tokens = value.get("input_tokens").and_then(Value::as_u64);
    let cache_read_tokens = value.get("cache_read_input_tokens").and_then(Value::as_u64);
    let cache_write_tokens = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    let context_input_tokens = input_tokens.map(|input| {
        input
            .saturating_add(cache_read_tokens.unwrap_or_default())
            .saturating_add(cache_write_tokens.unwrap_or_default())
    });
    Usage {
        input_tokens,
        context_input_tokens,
        output_tokens: value.get("output_tokens").and_then(Value::as_u64),
        total_tokens: value.get("total_tokens").and_then(Value::as_u64),
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelSpec, ReasoningSpec, ToolResultContent};
    use crate::provider::testing;

    /// 测试用的输出上限，够大，不会参与思考预算的夹取。
    const MAX_TOKENS: u64 = 64_000;

    /// 普通 Anthropic 请求照旧发 thinking：强度来自模型配置的"思考强度"，
    /// `configure` 兜底后就是这个形状。P2-1 让"关闭"可表达之后，这条不能被顺手关掉。
    #[test]
    fn a_request_that_did_not_ask_for_reasoning_still_sends_thinking() {
        let mut model = ModelSpec::new("claude-sonnet-4");
        model.reasoning.enabled = Some(true);
        model.reasoning.effort = Some("xhigh".into());

        let mut body = serde_json::json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "你好"}]},
        ]});
        apply_model(&mut body, &model, MAX_TOKENS).unwrap();

        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    /// 界面上的"思考预算 Token"有值时发 `enabled + budget_tokens`：Cursor/Devin 路径
    /// 以前完全不读这个字段，用户设了等于没设。
    #[test]
    fn a_thinking_budget_switches_to_the_enabled_form_and_is_capped_by_max_tokens() {
        let mut model = ModelSpec::new("claude-sonnet-4");
        model.reasoning.enabled = Some(true);
        model.reasoning.effort = Some("xhigh".into());
        model.reasoning.thinking_budget_tokens = Some(8_192);

        let mut body = serde_json::json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "你好"}]},
        ]});
        apply_model(&mut body, &model, 4_096).unwrap();

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(
            body["thinking"]["budget_tokens"], 4_095,
            "预算必须小于 max_tokens，否则上游 400"
        );
        assert!(
            body.get("output_config").is_none(),
            "effort 只属于 adaptive 形态，两种形态不能一起发"
        );

        let mut body = serde_json::json!({"messages": []});
        apply_model(&mut body, &model, MAX_TOKENS).unwrap();
        assert_eq!(body["thinking"]["budget_tokens"], 8_192);
    }

    /// Anthropic 要求预算不小于 1024。
    #[test]
    fn a_budget_below_the_api_minimum_is_raised_to_it() {
        let mut model = ModelSpec::new("claude-sonnet-4");
        model.reasoning.enabled = Some(true);
        model.reasoning.thinking_budget_tokens = Some(100);

        let mut body = serde_json::json!({"messages": []});
        apply_model(&mut body, &model, MAX_TOKENS).unwrap();

        assert_eq!(body["thinking"]["budget_tokens"], 1_024);
    }

    /// 显式关思考的请求（摘要/压缩）既不发 thinking，也不发预算或强度。
    #[test]
    fn an_explicitly_disabled_request_sends_no_thinking_at_all() {
        let mut model = ModelSpec::new("claude-sonnet-4");
        model.reasoning.enabled = Some(false);
        model.reasoning.effort = Some("xhigh".into());
        model.reasoning.thinking_budget_tokens = Some(8_192);

        let mut body = serde_json::json!({"messages": []});
        apply_model(&mut body, &model, MAX_TOKENS).unwrap();

        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }

    /// 留空"最大输出 Token"时用的兜底必须被在役模型接受：以前是 65000，输出上限更低的
    /// 模型每个请求都被 400 拒（不可重试，提示只说 max_tokens 超限）。
    #[test]
    fn the_fallback_max_tokens_fits_every_model_and_never_overrides_a_configured_limit() {
        assert_eq!(
            DEFAULT_MAX_OUTPUT_TOKENS, 4_096,
            "与 codex/upstream.rs 的同类兜底保持同一个安全值"
        );

        let mut model = ModelSpec::new("claude-sonnet-4");
        assert_eq!(resolve_max_output_tokens(&model, None), 4_096);
        assert_eq!(
            resolve_max_output_tokens(&model, Some(32_000)),
            32_000,
            "模型配置里写了真实上限就用它，不被兜底压低"
        );
        model.max_output_tokens = Some(8_192);
        assert_eq!(
            resolve_max_output_tokens(&model, Some(1_024)),
            8_192,
            "调用方指定的预算优先于模型配置"
        );
    }

    /// 已知上限的模型不能被兜底压到 4096：桌面端留空"最大输出 Token"时，模型配置会给出该
    /// 模型自己的上限（见 `model::configuration` 的模型表用例），这里钉住 provider 侧真的
    /// 用了它——否则 64k 级模型的回答会被静默截断，而且思考预算会被 4096 夹成 4095。
    #[test]
    fn a_known_model_keeps_its_real_limit_instead_of_the_fallback() {
        let mut model = ModelSpec::new("claude-sonnet-4");
        model.reasoning.enabled = Some(true);
        model.reasoning.thinking_budget_tokens = Some(32_000);

        let route_max = crate::model::known_anthropic_max_output_tokens(&model.model_id);
        assert_eq!(route_max, Some(64_000), "claude-sonnet-4 的真实上限");
        let max_tokens = resolve_max_output_tokens(&model, route_max);
        assert_eq!(max_tokens, 64_000, "模型配置里已知的上限不被 4096 兜底压低");

        let mut body = serde_json::json!({"messages": []});
        apply_model(&mut body, &model, max_tokens).unwrap();
        assert_eq!(
            body["thinking"]["budget_tokens"], 32_000,
            "思考预算按真实上限夹取，而不是被 4096 夹成 4095"
        );
    }

    /// 表里没有的模型（自建网关、别名）仍要能出请求：`max_tokens` 落在所有在役模型都接受
    /// 的兜底值上，思考预算严格小于它。上游对 `budget_tokens >= max_tokens` 直接 400，
    /// 而 4xx 不可重试，这条历史会一直留在会话里。
    #[test]
    fn an_unknown_model_still_produces_a_request_the_api_accepts() {
        let mut model = ModelSpec::new("my-gateway/claude-alias");
        model.reasoning.enabled = Some(true);
        model.reasoning.thinking_budget_tokens = Some(32_000);

        assert_eq!(
            crate::model::known_anthropic_max_output_tokens(&model.model_id),
            None,
            "未知模型不猜上限"
        );
        let max_tokens = resolve_max_output_tokens(&model, None);
        assert_eq!(max_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
        assert!(max_tokens > 0);

        let mut body = serde_json::json!({"messages": []});
        apply_model(&mut body, &model, max_tokens).unwrap();
        assert_eq!(body["thinking"]["budget_tokens"], 4_095);
        assert!(
            body["thinking"]["budget_tokens"].as_u64().unwrap() < max_tokens,
            "预算必须严格小于 max_tokens"
        );
    }

    /// 结束条件一：上游发了 message_stop，正常收尾。
    #[tokio::test]
    async fn a_message_stop_ends_the_stream_with_done() {
        let url = testing::sse_upstream(
            "/v1/messages",
            vec![
                json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                json!({"type":"message_stop"}),
            ],
        )
        .await;
        let provider = AnthropicProvider::new(
            crate::network::OutboundClient::fixed(reqwest::Client::new()),
            testing::provider_config(crate::config::ProviderKind::Anthropic, url),
        );

        let events = testing::drain(provider.stream(
            testing::invocation("claude-sonnet-4"),
            tokio_util::sync::CancellationToken::new(),
        ))
        .await
        .unwrap_or_else(|error| panic!("带 message_stop 的流必须正常收尾: {error}"));

        assert_eq!(testing::text_of(&events), "你好");
        assert_eq!(testing::finish_of(&events), Some(FinishReason::Stop));
    }

    /// 结束条件二：上游发了 stop_reason 却没发 message_stop 就断开——内容与收尾都要交付，
    /// 不能把用户已经看到的回答丢掉。
    #[tokio::test]
    async fn a_truncated_stream_with_a_stop_reason_still_finishes() {
        let url = testing::sse_upstream(
            "/v1/messages",
            vec![
                json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"半句"}}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            ],
        )
        .await;
        let provider = AnthropicProvider::new(
            crate::network::OutboundClient::fixed(reqwest::Client::new()),
            testing::provider_config(crate::config::ProviderKind::Anthropic, url),
        );

        let events = testing::drain(provider.stream(
            testing::invocation("claude-sonnet-4"),
            tokio_util::sync::CancellationToken::new(),
        ))
        .await
        .unwrap_or_else(|error| panic!("有 stop_reason 时缺终止符也要收尾: {error}"));

        assert_eq!(testing::text_of(&events), "半句");
        assert_eq!(testing::finish_of(&events), Some(FinishReason::Stop));
    }

    /// 结束条件三：既没有 stop_reason 也没有 message_stop 就断开——必须报错，不能当作
    /// "回答完了"静默成功。
    #[tokio::test]
    async fn a_stream_that_ends_without_any_terminator_is_an_error() {
        let url = testing::sse_upstream(
            "/v1/messages",
            vec![
                json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"断"}}),
            ],
        )
        .await;
        let provider = AnthropicProvider::new(
            crate::network::OutboundClient::fixed(reqwest::Client::new()),
            testing::provider_config(crate::config::ProviderKind::Anthropic, url),
        );

        let error = testing::drain(provider.stream(
            testing::invocation("claude-sonnet-4"),
            tokio_util::sync::CancellationToken::new(),
        ))
        .await
        .expect_err("没有终止符的流必须报错");

        assert!(
            error.to_string().contains("without message_stop"),
            "{error}"
        );
    }

    /// 失败的工具结果必须带 is_error,否则模型把失败当成功继续推理。
    #[test]
    fn failed_tool_results_are_marked_as_errors() {
        let message = |is_error: bool| ProjectedMessage {
            message_id: "m1".into(),
            role: Role::Tool,
            content: ProjectedContent::ToolResult(ToolResultContent {
                call_id: "call-1".into(),
                name: "shell".into(),
                content: "exit code 1: no such file".into(),
                is_error,
                image: None,
                provider_parts: Vec::new(),
            }),
        };

        let failed = anthropic_messages(&[message(true)]).unwrap();
        assert_eq!(failed[0]["content"][0]["type"], "tool_result");
        assert_eq!(failed[0]["content"][0]["is_error"], true);

        let succeeded = anthropic_messages(&[message(false)]).unwrap();
        assert!(
            succeeded[0]["content"][0].get("is_error").is_none(),
            "成功的结果不需要这个字段"
        );
    }

    /// 工具续轮没有签名 thinking 时不能开 thinking,否则上游 400 且不可重试。
    #[test]
    fn thinking_is_dropped_when_the_trailing_tool_turn_has_no_signed_thinking() {
        let mut model = ModelSpec::new("claude-sonnet-4");
        model.reasoning = ReasoningSpec {
            enabled: Some(true),
            effort: Some("xhigh".into()),
            thinking_budget_tokens: None,
        };

        // 末条 assistant 只有 tool_use(上一轮可能由别的协议产出)。
        let mut body = serde_json::json!({"messages": [{
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "call-1", "name": "shell", "input": {}}],
        }]});
        apply_model(&mut body, &model, MAX_TOKENS).unwrap();
        assert!(body.get("thinking").is_none(), "不能开 thinking");
        assert!(body.get("output_config").is_none(), "思考强度也要一起去掉");

        // 带签名 thinking 的续轮照旧开。
        let mut body = serde_json::json!({"messages": [{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "…", "signature": "sig"},
                {"type": "tool_use", "id": "call-1", "name": "shell", "input": {}},
            ],
        }]});
        apply_model(&mut body, &model, MAX_TOKENS).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "xhigh");

        // 上一条是 redacted_thinking 也算可续轮。
        let mut body = serde_json::json!({"messages": [{
            "role": "assistant",
            "content": [
                {"type": "redacted_thinking", "data": "…"},
                {"type": "tool_use", "id": "call-1", "name": "shell", "input": {}},
            ],
        }]});
        apply_model(&mut body, &model, MAX_TOKENS).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");

        // 末轮不是工具轮(普通回复 / 首轮没有助手消息)照旧开。
        let mut body = serde_json::json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "你好"}]},
        ]});
        apply_model(&mut body, &model, MAX_TOKENS).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
    }

    #[test]
    fn cached_tokens_are_included_once_in_anthropic_context_input() {
        let usage = anthropic_usage(&serde_json::json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_read_input_tokens": 20,
            "cache_creation_input_tokens": 30
        }));

        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.context_input_tokens, Some(60));
        assert_eq!(usage.output_tokens, Some(5));
    }

    #[test]
    fn streamed_anthropic_usage_includes_cached_input_in_total() {
        let mut usage = Usage::default();
        merge_usage(
            &mut usage,
            anthropic_usage(&serde_json::json!({
                "input_tokens": 414,
                "output_tokens": 0,
                "cache_read_input_tokens": 100_352,
                "cache_creation_input_tokens": 0
            })),
        );
        merge_usage(
            &mut usage,
            anthropic_usage(&serde_json::json!({"output_tokens": 191})),
        );

        assert_eq!(usage.input_tokens, Some(414));
        assert_eq!(usage.cache_read_tokens, Some(100_352));
        assert_eq!(usage.cache_write_tokens, Some(0));
        assert_eq!(usage.context_input_tokens, Some(100_766));
        assert_eq!(usage.output_tokens, Some(191));
        assert_eq!(usage.total_tokens, Some(100_957));
    }
}
