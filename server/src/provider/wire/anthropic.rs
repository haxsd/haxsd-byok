//! Anthropic 的共享判定：`stop_reason` 分类、思考预算边界、prompt 缓存断点。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// `stop_reason` 的语义分类。未知取值保留为 `Unknown`，由调用方决定报错还是按完成处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AnthropicStop {
    EndTurn,
    ToolUse,
    StopSequence,
    Pause,
    MaxTokens,
    ContextWindowExceeded,
    Refusal,
    Unknown,
}

pub(crate) fn anthropic_stop(reason: &str) -> AnthropicStop {
    match reason {
        "end_turn" => AnthropicStop::EndTurn,
        "tool_use" => AnthropicStop::ToolUse,
        "stop_sequence" => AnthropicStop::StopSequence,
        // `pause_turn` 是上游“这一轮先停在这里”的正常取值，不是失败。
        "pause_turn" => AnthropicStop::Pause,
        "max_tokens" => AnthropicStop::MaxTokens,
        "model_context_window_exceeded" => AnthropicStop::ContextWindowExceeded,
        "refusal" => AnthropicStop::Refusal,
        _ => AnthropicStop::Unknown,
    }
}

/// Anthropic 要求 `budget_tokens` 不小于 1024，且必须小于 `max_tokens`。
///
/// 两个条件在 `max_tokens <= 1024` 时无法同时满足，此时返回 `None`，调用方退回 adaptive。
pub(crate) fn thinking_budget(configured: Option<u64>, max_tokens: u64) -> Option<u64> {
    let configured = configured?;
    (max_tokens > 1_024).then(|| configured.max(1_024).min(max_tokens - 1))
}

/// 在最后一条 user 消息的最后一个可缓存块上放一个 ephemeral 断点。
///
/// thinking 块不能带 `cache_control`，所以只认 text / image / tool_result。
pub(crate) fn mark_cache_breakpoint(messages: &mut [Value]) {
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

/// 给一份完整的 Anthropic 请求体放上与 provider 路径同口径的三个缓存断点：
/// system、最后一个工具、最后一条 user 消息。Anthropic 不会自动缓存，没有断点
/// 每轮都按全价重算整段前缀。已经带断点的块不重复添加（上限 4 个）。
pub(crate) fn mark_request_cache_breakpoints(body: &mut Value) {
    if let Some(text) = body.get("system").and_then(Value::as_str) {
        if !text.is_empty() {
            let text = text.to_owned();
            body["system"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"}
            }]);
        }
    }
    if let Some(tool) = body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .and_then(|tools| tools.last_mut())
        .and_then(Value::as_object_mut)
    {
        tool.entry("cache_control")
            .or_insert_with(|| json!({"type": "ephemeral"}));
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        mark_cache_breakpoint(messages);
    }
}
