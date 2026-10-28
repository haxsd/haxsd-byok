//! Responses 请求 → 上游 Chat/Anthropic 请求：完整历史、工具声明、tool_choice、并行、图像与推理回放。
//!
//! 无法表达的语义（远端存储、跨请求引用、hosted 工具、远端压缩）一律明确报错，不做静默降级。
//! 少数为了兼容严格上游的必要动作（未配对工具回合、孤儿工具结果）在下面逐处说明；不属于本
//! 模型/协议的推理密文则降级成"没有可回放状态"（见 `replay::decode`），不拖着整轮一起失败。
use super::{replay, tools::ToolMap, BridgeRequest, UpstreamFormat};
use crate::{Error, Result};
use serde_json::{json, Map, Value};

/// Anthropic 的 `max_tokens` 是必填字段，模型没配输出上限时总得给一个值。
///
/// 与 provider 路径共用同一个口径（`provider/anthropic.rs` 的
/// `DEFAULT_MAX_OUTPUT_TOKENS`）：取在役 Anthropic 模型都接受的最小公共上限，而不是按
/// 最大的模型取值——400 不可重试，用户只会看到"max_tokens 超限"，联想不到"模型里留空了"。
/// 它只是必填字段的兜底，不是上限：客户端自己给了值就听客户端的。
const DEFAULT_ANTHROPIC_MAX_TOKENS: u64 = 4_096;

pub fn translate_request(
    source: &Value,
    format: UpstreamFormat,
    model: &str,
    max_tokens: u64,
) -> Result<BridgeRequest> {
    translate_request_owned(source, format, model, Some(max_tokens), model)
}

pub(crate) fn translate_request_owned(
    source: &Value,
    format: UpstreamFormat,
    model: &str,
    configured_max_tokens: Option<u64>,
    owner: &str,
) -> Result<BridgeRequest> {
    let source = source
        .as_object()
        .ok_or_else(|| Error::Protocol("Codex request must be a JSON object".into()))?;
    reject_unsupported(source)?;
    let tools = ToolMap::parse(source.get("tools"))?;
    let items = input_items(source)?;
    validate_tool_history(&items)?;
    // 客户端可以要求更小的输出上限，但不能超过模型配置；模型没配上限时，客户端的
    // 要求原样发出——以前这里凭空按 4096 夹取，配置留空的模型连客户端更大的请求
    // 都被压低，长回答在 4096 处被截断，而目录里没有任何地方说过这个上限。
    let max_tokens = match (
        source.get("max_output_tokens").and_then(Value::as_u64),
        configured_max_tokens,
    ) {
        (Some(client), Some(limit)) => Some(client.min(limit)),
        (Some(client), None) => Some(client),
        (None, limit) => limit,
    }
    .map(|value| value.max(1));
    let body = match format {
        UpstreamFormat::Chat => chat_body(source, &items, model, max_tokens, &tools, owner)?,
        UpstreamFormat::Anthropic => anthropic_body(
            source,
            &items,
            model,
            max_tokens.unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS),
            &tools,
            owner,
        )?,
    };
    Ok(BridgeRequest { body, tools })
}

fn reject_unsupported(source: &Map<String, Value>) -> Result<()> {
    for key in ["previous_response_id", "conversation", "background"] {
        if source.get(key).is_some_and(|value| !value.is_null()) {
            return Err(Error::Protocol(format!(
                "{key} is not supported: the bridge always sends the complete Codex history"
            )));
        }
    }
    if source.get("store").and_then(Value::as_bool) == Some(true) {
        return Err(Error::Protocol(
            "store is not supported: the bridge keeps no server-side response state".into(),
        ));
    }
    if source
        .get("compaction_trigger")
        .is_some_and(|value| !value.is_null())
    {
        return Err(Error::Protocol(
            "compaction_trigger is not supported: remote compaction is declared unsupported, Codex must compact locally".into(),
        ));
    }
    Ok(())
}

fn input_items(source: &Map<String, Value>) -> Result<Vec<Value>> {
    match source.get("input") {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(vec![
            json!({"type": "message", "role": "user", "content": text}),
        ]),
        Some(Value::Array(items)) if !items.is_empty() => Ok(items.clone()),
        _ => Err(Error::Protocol(
            "Codex request input must be a non-empty string or array".into(),
        )),
    }
}

/// 模型指令：`instructions` 加历史里的 system/developer 消息，按出现顺序拼接。
fn system_parts(source: &Map<String, Value>, items: &[Value]) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    if let Some(text) = source
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        parts.push(text.to_string());
    }
    for item in items {
        if item_type(item) == "message"
            && matches!(
                item.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        {
            let text = message_text(item)?.trim().to_string();
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    Ok(parts)
}

/// 系统指令按工具声明裁剪：`base.md` 的目录模板会在每个模型上静态提到 `update_plan`，
/// 但 Codex 在默认模式/压缩后的回合可能不声明这个工具。模型按提示词调用一个不存在的
/// 工具时，bridge 只能报 “upstream returned undeclared tool”，所以这里先删掉相关段落。
fn system_text(source: &Map<String, Value>, items: &[Value], tools: &ToolMap) -> Result<String> {
    let mut text = system_parts(source, items)?.join("\n\n");
    if !tools.declares("update_plan") {
        text = strip_update_plan_instructions(&text);
    }
    Ok(text)
}

fn strip_update_plan_instructions(text: &str) -> String {
    let mut kept = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        if line.trim_start().starts_with("## ") {
            skipping = matches!(line.trim(), "## Planning" | "## `update_plan`");
        }
        if !skipping && !line.contains("update_plan") {
            kept.push(line);
        }
    }
    kept.join("\n").trim().to_string()
}

// ===================== 回合累积 =====================

#[derive(Clone)]
struct Call {
    call_id: String,
    /// 已经是上游可用的工具名。
    name: String,
    arguments: String,
}

#[derive(Default)]
struct Turn {
    text: Option<String>,
    reasoning_text: Option<String>,
    calls: Vec<Call>,
    results: Vec<(String, Value)>,
}

fn item_type(item: &Value) -> &str {
    item.get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
}

/// `create_thread` 是 Codex 的跨会话委派入口：新会话的第一项不是普通用户消息，
/// 而是没有 `call_id` 的 `function_call_output`。它的 output 就是可见的用户任务，
/// 可以作为普通用户消息重放；不能用工具结果分支要求一个并不存在的调用。
fn create_thread_input(item: &Value) -> Option<&str> {
    if !matches!(
        item_type(item),
        "function_call_output" | "custom_tool_call_output"
    ) || item
        .get("call_id")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
        || item.get("name").and_then(Value::as_str) != Some("create_thread")
        || item.get("namespace").and_then(Value::as_str) != Some("codex_app")
    {
        return None;
    }
    item.get("output")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

const AGENT_MESSAGE_REQUIRES_RESPONSES: &str =
    "this conversation contains Codex subagent messages (agent_message) whose encrypted payload \
     can only be replayed to an OpenAI Responses model; switch back to the original model or \
     start a new conversation";

fn history_call(item: &Value, tools: &ToolMap, kind: &str) -> Result<Call> {
    let name = required_str(item, "name")?;
    let upstream = tools.upstream_name(name, item.get("namespace").and_then(Value::as_str));
    let call_id = required_str(item, "call_id")?;
    let arguments = if kind == "custom_tool_call" {
        let input = item.get("input").and_then(Value::as_str).ok_or_else(|| {
            Error::Protocol("Codex custom_tool_call requires a string input".into())
        })?;
        serde_json::to_string(&json!({"input": input}))?
    } else {
        required_str(item, "arguments")?.to_string()
    };
    Ok(Call {
        call_id: call_id.to_string(),
        name: upstream,
        arguments,
    })
}

fn validate_tool_history(items: &[Value]) -> Result<()> {
    use std::collections::{HashMap, HashSet};
    let mut pending = HashMap::new();
    let mut seen = HashSet::new();
    for item in items {
        if create_thread_input(item).is_some() {
            if !pending.is_empty() {
                return Err(Error::Protocol(
                    "tool calls are missing results before the next message".into(),
                ));
            }
            continue;
        }
        match item_type(item) {
            kind @ ("function_call" | "custom_tool_call") => {
                let id = required_str(item, "call_id")?;
                if !seen.insert(id) {
                    return Err(Error::Protocol("duplicate call_id in Codex history".into()));
                }
                pending.insert(id, kind);
            }
            "agent_message" => {
                return Err(Error::Protocol(AGENT_MESSAGE_REQUIRES_RESPONSES.into()))
            }
            kind @ ("function_call_output" | "custom_tool_call_output") => {
                let id = required_str(item, "call_id")?;
                let expected = if kind == "function_call_output" {
                    "function_call"
                } else {
                    "custom_tool_call"
                };
                if pending.remove(id) != Some(expected) {
                    return Err(Error::Protocol(
                        "tool result has no matching call in Codex history".into(),
                    ));
                }
                if item.get("output").is_none() {
                    return Err(Error::Protocol("tool result is missing output".into()));
                }
            }
            "message"
                if !pending.is_empty()
                    && !matches!(
                        item.get("role").and_then(Value::as_str),
                        Some("system" | "developer")
                    ) =>
            {
                return Err(Error::Protocol(
                    "tool calls are missing results before the next message".into(),
                ))
            }
            _ => {}
        }
    }
    if !pending.is_empty() {
        return Err(Error::Protocol(
            "tool calls are missing results; send a complete history or start a new conversation"
                .into(),
        ));
    }
    Ok(())
}

fn push_tool_result(turn: &mut Turn, item: &Value) {
    let call_id = required_str(item, "call_id")
        .expect("history was validated")
        .to_string();
    turn.results.push((call_id, item["output"].clone()));
}

// ===================== Chat 请求 =====================

fn chat_body(
    source: &Map<String, Value>,
    items: &[Value],
    model: &str,
    max_tokens: Option<u64>,
    tools: &ToolMap,
    owner: &str,
) -> Result<Value> {
    let system = system_text(source, items, tools)?;
    let mut messages: Vec<Value> = Vec::new();
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    let mut turn = Turn::default();
    let mut pending_reasoning: Option<String> = None;
    for item in items {
        if let Some(text) = create_thread_input(item) {
            flush_chat(&mut messages, &mut turn, &mut pending_reasoning)?;
            messages.push(json!({"role": "user", "content": text}));
            continue;
        }
        match item_type(item) {
            "message" => match item.get("role").and_then(Value::as_str) {
                Some("system" | "developer") => {}
                Some("user") => {
                    flush_chat(&mut messages, &mut turn, &mut pending_reasoning)?;
                    messages.push(json!({"role": "user", "content": chat_content(item)?}));
                }
                Some("assistant") => {
                    flush_chat(&mut messages, &mut turn, &mut pending_reasoning)?;
                    let text = message_text(item)?;
                    if !text.trim().is_empty() {
                        turn.text = Some(text);
                    }
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "unsupported Codex message role: {other:?}"
                    )))
                }
            },
            "reasoning" => {
                if let Some(text) = chat_reasoning_text(item, owner)? {
                    pending_reasoning = Some(match pending_reasoning.take() {
                        Some(previous) => format!("{previous}\n\n{text}"),
                        None => text,
                    });
                }
            }
            "function_call" | "custom_tool_call" => {
                if let Some(reasoning) = item
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    turn.reasoning_text
                        .get_or_insert_with(|| reasoning.to_string());
                }
                let call = history_call(item, tools, item_type(item))?;
                turn.calls.push(call);
            }
            kind @ ("function_call_output" | "custom_tool_call_output") => {
                let _ = kind;
                push_tool_result(&mut turn, item);
            }
            other => {
                return Err(Error::Protocol(format!(
                    "unsupported Codex input item: {other}"
                )))
            }
        }
    }
    flush_chat(&mut messages, &mut turn, &mut pending_reasoning)?;
    if let Some(reasoning) = pending_reasoning.take() {
        attach_trailing_reasoning(&mut messages, reasoning);
    }
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    // 模型没配输出上限、客户端也没要求时干脆不写：交给上游自己的默认值，
    // 这才和"留空使用默认值"的说法一致。
    if let Some(max_tokens) = max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    let definitions = tools.definitions(false);
    if !definitions.is_empty() {
        body["tools"] = json!(definitions);
        if let Some(choice) = source.get("tool_choice") {
            body["tool_choice"] = chat_tool_choice(choice, tools)?;
        }
        if let Some(parallel) = source.get("parallel_tool_calls").and_then(Value::as_bool) {
            body["parallel_tool_calls"] = json!(parallel);
        }
    }
    if let Some(effort) = source
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
    {
        let effort = effort.trim();
        if !matches!(effort, "" | "none" | "off" | "disabled") {
            body["reasoning_effort"] = json!(effort);
        }
    }
    for key in ["temperature", "top_p", "user"] {
        if let Some(value) = source.get(key).filter(|value| !value.is_null()) {
            body[key] = value.clone();
        }
    }
    Ok(body)
}

fn flush_chat(
    messages: &mut Vec<Value>,
    turn: &mut Turn,
    pending_reasoning: &mut Option<String>,
) -> Result<()> {
    let calls = std::mem::take(&mut turn.calls);
    let results = std::mem::take(&mut turn.results);
    let text = turn.text.take();
    let mut reasoning = turn.reasoning_text.take();
    if !calls.is_empty() {
        let mut message =
            json!({"role": "assistant", "content": text.map(Value::String).unwrap_or(Value::Null)});
        message["tool_calls"] = Value::Array(
            calls
                .iter()
                .map(|call| {
                    json!({"id": call.call_id, "type": "function",
                        "function": {"name": call.name, "arguments": call.arguments}})
                })
                .collect(),
        );
        if let Some(reasoning) = reasoning.take().or_else(|| pending_reasoning.take()) {
            message["reasoning_content"] = json!(reasoning);
        }
        messages.push(message);
        for call in &calls {
            if let Some((_, output)) = results.iter().find(|(call_id, _)| call_id == &call.call_id)
            {
                messages.push(json!({"role": "tool", "tool_call_id": call.call_id, "content": tool_output(output,false)?}));
            }
        }
    } else if let Some(text) = text {
        let mut message = json!({"role": "assistant", "content": text});
        if let Some(reasoning) = reasoning.take().or_else(|| pending_reasoning.take()) {
            message["reasoning_content"] = json!(reasoning);
        }
        messages.push(message);
    }
    Ok(())
}

fn attach_trailing_reasoning(messages: &mut Vec<Value>, reasoning: String) {
    let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|message| message["role"] == "assistant")
    else {
        messages.push(json!({"role":"assistant","content":null,"reasoning_content":reasoning}));
        return;
    };
    let existing = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .map(str::to_string);
    message["reasoning_content"] = json!(match existing {
        Some(previous) => format!("{previous}\n\n{reasoning}"),
        None => reasoning,
    });
}

fn chat_tool_choice(value: &Value, tools: &ToolMap) -> Result<Value> {
    match value {
        Value::String(choice) => match choice.as_str() {
            "auto" | "none" | "required" => Ok(value.clone()),
            other => Err(Error::Protocol(format!("unsupported tool_choice: {other}"))),
        },
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("function" | "custom") => Ok(json!({
                "type": "function",
                "function": {"name": tools.upstream_name_for_tool_choice(value)?}
            })),
            other => Err(Error::Protocol(format!(
                "unsupported tool_choice shape: {other:?}; forced hosted tools are not implemented"
            ))),
        },
        other => Err(Error::Protocol(format!(
            "unsupported tool_choice value: {other}"
        ))),
    }
}

// ===================== Anthropic 请求 =====================

fn anthropic_body(
    source: &Map<String, Value>,
    items: &[Value],
    model: &str,
    max_tokens: u64,
    tools: &ToolMap,
    owner: &str,
) -> Result<Value> {
    let system = system_text(source, items, tools)?;
    let mut messages: Vec<Value> = Vec::new();
    let mut turn = Turn::default();
    let mut pending_thinking: Vec<Value> = Vec::new();
    for item in items {
        if let Some(text) = create_thread_input(item) {
            flush_anthropic(&mut messages, &mut turn, &mut pending_thinking)?;
            push_anthropic(
                &mut messages,
                "user",
                vec![json!({"type": "text", "text": text})],
            );
            continue;
        }
        match item_type(item) {
            "message" => match item.get("role").and_then(Value::as_str) {
                Some("system" | "developer") => {}
                Some("user") => {
                    flush_anthropic(&mut messages, &mut turn, &mut pending_thinking)?;
                    let blocks = anthropic_blocks(item)?;
                    push_anthropic(&mut messages, "user", blocks);
                }
                Some("assistant") => {
                    flush_anthropic(&mut messages, &mut turn, &mut pending_thinking)?;
                    let text = message_text(item)?;
                    if !text.trim().is_empty() {
                        turn.text = Some(text);
                    }
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "unsupported Codex message role: {other:?}"
                    )))
                }
            },
            "reasoning" => pending_thinking.extend(anthropic_thinking_blocks(item, owner)?),
            "function_call" | "custom_tool_call" => {
                let call = history_call(item, tools, item_type(item))?;
                turn.calls.push(call);
            }
            "function_call_output" | "custom_tool_call_output" => push_tool_result(&mut turn, item),
            other => {
                return Err(Error::Protocol(format!(
                    "unsupported Codex input item: {other}"
                )))
            }
        }
    }
    flush_anthropic(&mut messages, &mut turn, &mut pending_thinking)?;
    drop_empty_messages(&mut messages);
    if messages.is_empty() {
        return Err(Error::Protocol(
            "Codex history produced no Anthropic messages".into(),
        ));
    }
    trim_trailing_assistant_prefill(&mut messages);
    let mut body =
        json!({"model": model, "max_tokens": max_tokens, "stream": true, "messages": messages});
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    let definitions = tools.definitions(true);
    let mut tool_choice = None;
    if !definitions.is_empty() {
        body["tools"] = json!(definitions);
        if let Some(value) = source.get("tool_choice") {
            tool_choice = Some(anthropic_tool_choice(value, tools)?);
        }
        if let Some(parallel) = source.get("parallel_tool_calls").and_then(Value::as_bool) {
            let choice = tool_choice.get_or_insert_with(|| json!({"type": "auto"}));
            choice["disable_parallel_tool_use"] = json!(!parallel);
        }
    }
    let forced_choice = tool_choice
        .as_ref()
        .is_some_and(|choice| matches!(choice["type"].as_str(), Some("any" | "tool")));
    if let Some(choice) = tool_choice {
        body["tool_choice"] = choice;
    }
    let thinking = thinking_config(source, max_tokens, &body["messages"], forced_choice);
    match &thinking {
        Some(thinking) => body["thinking"] = thinking.clone(),
        None => {
            // thinking 与采样参数互斥：只有不开启 thinking 时才发 temperature/top_p。
            for key in ["temperature", "top_p"] {
                if let Some(value) = source.get(key).filter(|value| !value.is_null()) {
                    body[key] = value.clone();
                }
            }
        }
    }
    Ok(body)
}

/// 把累积的一个回合写进 Anthropic 消息序列。
///
/// 签名 thinking 的唯一来源是 `pending_thinking`：解析历史时它就直接记在那里，回合结构
/// 里没有第二份。
fn flush_anthropic(
    messages: &mut Vec<Value>,
    turn: &mut Turn,
    pending_thinking: &mut Vec<Value>,
) -> Result<()> {
    let calls = std::mem::take(&mut turn.calls);
    let results = std::mem::take(&mut turn.results);
    let text = turn.text.take();
    if !calls.is_empty() {
        let mut blocks = std::mem::take(pending_thinking);
        if let Some(text) = text {
            blocks.push(json!({"type": "text", "text": text}));
        }
        for call in &calls {
            blocks.push(
                json!({"type": "tool_use", "id": call.call_id, "name": call.name,
                "input": tool_use_input(call)?}),
            );
        }
        push_anthropic(messages, "assistant", blocks);
        let result_blocks = calls.iter().map(|call| {
            let (_,output)=results.iter().find(|(call_id,_)|call_id==&call.call_id).expect("complete history was validated");
            Ok(json!({"type":"tool_result","tool_use_id":call.call_id,"content":tool_output(output,true)?}))
        }).collect::<Result<Vec<Value>>>()?;
        push_anthropic(messages, "user", result_blocks);
    } else if let Some(text) = text {
        let mut blocks = std::mem::take(pending_thinking);
        blocks.push(json!({"type": "text", "text": text}));
        push_anthropic(messages, "assistant", blocks);
    }
    Ok(())
}

fn push_anthropic(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if messages
        .last()
        .is_some_and(|message| message["role"] == role)
    {
        if let Some(content) = messages
            .last_mut()
            .and_then(|message| message["content"].as_array_mut())
        {
            content.extend(blocks);
            return;
        }
    }
    messages.push(json!({"role": role, "content": blocks}));
}

fn tool_use_input(call: &Call) -> Result<Value> {
    let value: Value = serde_json::from_str(&call.arguments).map_err(|_| {
        Error::Protocol(format!(
            "Codex tool call {} carries arguments that are not JSON",
            call.call_id
        ))
    })?;
    if !value.is_object() {
        return Err(Error::Protocol(format!(
            "Anthropic tool_use input must be a JSON object for tool call {}",
            call.call_id
        )));
    }
    Ok(value)
}

fn drop_empty_messages(messages: &mut Vec<Value>) {
    messages.retain(|message| {
        message["content"]
            .as_array()
            .is_some_and(|blocks| !blocks.is_empty())
    });
}

/// Anthropic 允许最后一条是 assistant 预填，但不能以空白结尾；这里只修剪这种尾巴。
fn trim_trailing_assistant_prefill(messages: &mut Vec<Value>) {
    let Some(last) = messages.last_mut() else {
        return;
    };
    if last["role"] != "assistant" {
        return;
    }
    let Some(blocks) = last["content"].as_array_mut() else {
        return;
    };
    if !blocks.iter().all(|block| block["type"] == "text") {
        return;
    }
    for block in blocks.iter_mut() {
        if let Some(text) = block["text"].as_str() {
            block["text"] = json!(text.trim_end());
        }
    }
    blocks.retain(|block| block["text"].as_str().is_some_and(|text| !text.is_empty()));
    if blocks.is_empty() {
        messages.pop();
    }
}

fn anthropic_tool_choice(value: &Value, tools: &ToolMap) -> Result<Value> {
    match value {
        Value::String(choice) => match choice.as_str() {
            "auto" => Ok(json!({"type": "auto"})),
            "none" => Ok(json!({"type": "none"})),
            "required" => Ok(json!({"type": "any"})),
            other => Err(Error::Protocol(format!("unsupported tool_choice: {other}"))),
        },
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("function" | "custom") => Ok(json!({
                "type": "tool",
                "name": tools.upstream_name_for_tool_choice(value)?
            })),
            other => Err(Error::Protocol(format!(
                "unsupported tool_choice shape: {other:?}; forced hosted tools are not implemented"
            ))),
        },
        other => Err(Error::Protocol(format!(
            "unsupported tool_choice value: {other}"
        ))),
    }
}

// ===================== Anthropic thinking 决策 =====================

fn thinking_config(
    source: &Map<String, Value>,
    max_tokens: u64,
    messages: &Value,
    forced_choice: bool,
) -> Option<Value> {
    if forced_choice {
        // Anthropic 拒绝在 thinking 开启时强制 tool_choice；保留调用方的强制语义，关掉 thinking。
        return None;
    }
    let effort = source
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)?;
    let mut budget = effort_to_thinking_budget(effort)?;
    if !trailing_turn_supports_thinking(messages) {
        return None;
    }
    budget = budget.min(max_tokens / 2);
    if budget < 1024 {
        return None;
    }
    Some(json!({"type": "enabled", "budget_tokens": budget}))
}

fn effort_to_thinking_budget(effort: &str) -> Option<u64> {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" | "low" => Some(2048),
        "medium" => Some(8192),
        "high" => Some(16384),
        "xhigh" | "max" | "ultra" => Some(24576),
        _ => None,
    }
}

/// Anthropic 要求工具续轮里回放上一条 assistant 的签名 thinking；
/// 只有可见摘要（没有签名）时不能继续 thinking，否则上游直接 400。
fn trailing_turn_supports_thinking(messages: &Value) -> bool {
    let Some(messages) = messages.as_array() else {
        return true;
    };
    let Some(blocks) = messages
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant")
        .and_then(|message| message["content"].as_array())
    else {
        return true;
    };
    if !blocks.iter().any(|block| block["type"] == "tool_use") {
        return true;
    }
    blocks.iter().any(|block| {
        matches!(
            block["type"].as_str(),
            Some("thinking" | "redacted_thinking")
        )
    })
}

// ===================== 推理回放 =====================

fn chat_reasoning_text(item: &Value, owner: &str) -> Result<Option<String>> {
    match replay::decode(item, UpstreamFormat::Chat, owner)? {
        Some(Value::String(text)) => Ok((!text.trim().is_empty()).then(|| text.trim().to_string())),
        Some(other) => Err(Error::Protocol(format!(
            "Codex chat reasoning state must be text, got {other}"
        ))),
        // 没有密文时退回可见摘要；上游未声明 thinking 也能接受。
        None => Ok(summary_text(item)),
    }
}

fn anthropic_thinking_blocks(item: &Value, owner: &str) -> Result<Vec<Value>> {
    match replay::decode(item, UpstreamFormat::Anthropic, owner)? {
        Some(Value::Array(blocks)) => Ok(blocks),
        Some(other) => Err(Error::Protocol(format!(
            "Codex Anthropic reasoning state must be a list of thinking blocks, got {other}"
        ))),
        // 只有可见摘要：没有签名，不能冒充 thinking 回放。
        None => Ok(Vec::new()),
    }
}

fn summary_text(item: &Value) -> Option<String> {
    let mut parts = Vec::new();
    for key in ["summary", "content"] {
        if let Some(items) = item.get(key).and_then(Value::as_array) {
            for part in items {
                if let Some(text) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    parts.push(text.to_string());
                }
            }
        }
    }
    if let Some(text) = item
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        parts.push(text.to_string());
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

// ===================== 正文与图像 =====================

enum Part {
    Text(String),
    Image(Value),
}

fn content_parts(item: &Value) -> Result<Vec<Part>> {
    match item.get("content") {
        Some(Value::String(text)) => Ok(vec![Part::Text(text.clone())]),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text" | "input_text" | "output_text") => Ok(Part::Text(
                    part.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                )),
                Some("input_image") => Ok(Part::Image(part.clone())),
                other => Err(Error::Protocol(format!(
                    "unsupported Codex content part: {other:?}"
                ))),
            })
            .collect(),
        _ => Err(Error::Protocol(
            "Codex message content must be text or an array".into(),
        )),
    }
}

fn message_text(item: &Value) -> Result<String> {
    let mut texts = Vec::new();
    for part in content_parts(item)? {
        match part {
            Part::Text(text) => texts.push(text),
            Part::Image(_) => {
                return Err(Error::Protocol(
                    "Codex system/developer/assistant messages cannot carry images".into(),
                ))
            }
        }
    }
    Ok(texts.join("\n\n"))
}

fn chat_content(item: &Value) -> Result<Value> {
    let parts = content_parts(item)?;
    let mut blocks = Vec::new();
    let mut text_only = true;
    for part in parts {
        match part {
            Part::Text(text) => blocks.push(json!({"type": "text", "text": text})),
            Part::Image(image) => {
                text_only = false;
                blocks.push(chat_image_part(&image)?);
            }
        }
    }
    if text_only {
        let text = blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        return Ok(json!(text));
    }
    Ok(Value::Array(blocks))
}

fn anthropic_blocks(item: &Value) -> Result<Vec<Value>> {
    let mut blocks = Vec::new();
    for part in content_parts(item)? {
        match part {
            Part::Text(text) => {
                if !text.trim().is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
            Part::Image(image) => blocks.push(image_block(&image)?),
        }
    }
    Ok(blocks)
}

fn chat_image_part(part: &Value) -> Result<Value> {
    let url = image_url(part)?;
    let mut image_url = json!({"url": url});
    if let Some(detail) = part.get("detail").and_then(Value::as_str) {
        image_url["detail"] = json!(detail);
    }
    Ok(json!({"type": "image_url", "image_url": image_url}))
}

fn image_block(part: &Value) -> Result<Value> {
    let url = image_url(part)?;
    if let Some(data) = url.strip_prefix("data:") {
        let (media_type, payload) = data.split_once(";base64,").ok_or_else(|| {
            Error::Protocol("Codex input_image data URL must be base64 encoded".into())
        })?;
        return Ok(
            json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": payload}}),
        );
    }
    Ok(json!({"type": "image", "source": {"type": "url", "url": url}}))
}

fn image_url(part: &Value) -> Result<String> {
    part.get("image_url")
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.get("url").and_then(Value::as_str).map(str::to_string))
        })
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| Error::Protocol("Codex input_image requires an image_url".into()))
}

/// 工具结果保留文本与图像块，不能降级成只包含 text 的字符串。
fn tool_output(output: &Value, anthropic: bool) -> Result<Value> {
    if output.is_array() {
        let item = json!({"content":output});
        if anthropic {
            Ok(json!(anthropic_blocks(&item)?))
        } else {
            chat_content(&item)
        }
    } else if output.is_string() {
        Ok(output.clone())
    } else {
        Ok(json!(output.to_string()))
    }
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| Error::Protocol(format!("Codex item is missing {key}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模型没配输出上限时上游请求正文里的 `max_tokens`：Chat 缺省就是不写这个字段，
    /// Anthropic 是必填字段，用与 provider 路径同一个兜底。
    fn mapped(format: UpstreamFormat, source: Value, configured: Option<u64>) -> Value {
        translate_request_owned(&source, format, "vendor-model", configured, "owner")
            .unwrap()
            .body
    }

    #[test]
    fn an_unconfigured_output_limit_does_not_cap_the_clients_request() {
        let source = json!({"input": "go", "max_output_tokens": 128_000});
        assert_eq!(
            mapped(UpstreamFormat::Chat, source.clone(), None)["max_tokens"],
            json!(128_000),
            "模型里留空时客户端更大的要求必须原样发出，不能被凭空造的 4096 压低"
        );
        assert_eq!(
            mapped(UpstreamFormat::Anthropic, source, None)["max_tokens"],
            json!(128_000)
        );
    }

    #[test]
    fn an_unconfigured_chat_limit_is_omitted_and_the_anthropic_one_falls_back() {
        let source = json!({"input": "go"});
        let chat = mapped(UpstreamFormat::Chat, source.clone(), None);
        assert!(
            chat.get("max_tokens").is_none(),
            "留空就是交给上游默认值，不该自己造一个上限：{chat}"
        );
        // Anthropic 的 max_tokens 必填：给最小的公共上限，而不是按最大的模型取值。
        assert_eq!(
            mapped(UpstreamFormat::Anthropic, source, None)["max_tokens"],
            json!(DEFAULT_ANTHROPIC_MAX_TOKENS)
        );
    }

    #[test]
    fn a_configured_output_limit_still_caps_the_clients_request() {
        let source = json!({"input": "go", "max_output_tokens": 128_000});
        assert_eq!(
            mapped(UpstreamFormat::Chat, source.clone(), Some(8_192))["max_tokens"],
            json!(8_192)
        );
        assert_eq!(
            mapped(UpstreamFormat::Anthropic, source, Some(8_192))["max_tokens"],
            json!(8_192)
        );
    }

    #[test]
    fn a_configured_output_limit_is_used_when_the_client_asks_for_nothing() {
        let source = json!({"input": "go"});
        assert_eq!(
            mapped(UpstreamFormat::Chat, source.clone(), Some(8_192))["max_tokens"],
            json!(8_192)
        );
        assert_eq!(
            mapped(UpstreamFormat::Anthropic, source, Some(8_192))["max_tokens"],
            json!(8_192)
        );
    }

    /// 公开入口的语义不变：调用方给的就是"模型配置里的输出上限"。
    #[test]
    fn the_public_entry_point_treats_its_argument_as_the_configured_limit() {
        let source = json!({"input": "go", "max_output_tokens": 128_000});
        assert_eq!(
            translate_request(&source, UpstreamFormat::Chat, "vendor-model", 4_096)
                .unwrap()
                .body["max_tokens"],
            json!(4_096)
        );
    }

    #[test]
    fn update_plan_instructions_are_removed_when_the_tool_is_not_declared() {
        let text = "intro\n\n## Planning\nuse update_plan to plan\n\n## Other\nkeep this\n\n## `update_plan`\nnot available";
        let stripped = strip_update_plan_instructions(text);
        assert!(!stripped.contains("update_plan"), "{stripped}");
        assert!(stripped.contains("## Other"), "{stripped}");
        assert!(stripped.contains("keep this"), "{stripped}");

        let source = json!({"instructions": text});
        let tools = ToolMap::parse(Some(&json!([]))).unwrap();
        let system = system_text(source.as_object().unwrap(), &[], &tools).unwrap();
        assert!(!system.contains("update_plan"), "{system}");

        let tools = ToolMap::parse(Some(&json!([
            {"type": "function", "name": "update_plan"}
        ])))
        .unwrap();
        let system = system_text(source.as_object().unwrap(), &[], &tools).unwrap();
        assert!(system.contains("update_plan"), "{system}");
    }
}
