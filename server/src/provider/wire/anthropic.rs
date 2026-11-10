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
/// thinking 块不能带 `cache_control`，所以只认 text / image / tool_result / document。
/// 返回是否真的新增了断点；已经有断点的块保持原 TTL，不重复计数。
pub(crate) fn mark_cache_breakpoint(messages: &mut [Value]) -> bool {
    let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return false;
    };
    let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return false;
    };
    for block in content.iter_mut().rev() {
        let kind = block.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("text" | "image" | "tool_result" | "document")) {
            continue;
        }
        if let Some(block) = block.as_object_mut() {
            // 客户端自己放过的 1h 断点不能被我们覆盖成默认 5m。
            if block.contains_key("cache_control") {
                return false;
            }
            block.insert("cache_control".into(), json!({"type": "ephemeral"}));
            return true;
        }
    }
    false
}

/// 统计请求体里已有的 `cache_control` 断点，包括 system、tools 与历史消息。
/// Anthropic 一次请求最多接受 4 个；预算必须按整份请求算，不能只看 system。
fn cache_control_count(value: &Value) -> usize {
    match value {
        Value::Object(object) => {
            usize::from(
                object
                    .get("cache_control")
                    .is_some_and(|value| !value.is_null()),
            ) + object.values().map(cache_control_count).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(cache_control_count).sum(),
        _ => 0,
    }
}

/// 是否已经有客户端放入的 1 小时缓存断点。它可能出现在 system 或历史消息的
/// document 块上；只要在前面再插 5 分钟断点，就可能打乱 TTL 顺序。
pub(crate) fn has_one_hour_cache(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object
                .get("cache_control")
                .and_then(|cache| cache.get("ttl"))
                .and_then(Value::as_str)
                == Some("1h")
                || object.values().any(has_one_hour_cache)
        }
        Value::Array(items) => items.iter().any(has_one_hour_cache),
        _ => false,
    }
}

/// 给一份完整的 Anthropic 请求体补充缓存断点：system、最后一个工具、最后一条
/// user 消息。Anthropic 不会自动缓存，没有断点每轮都按全价重算整段前缀。
///
/// 整份请求最多 4 个断点；已经带断点的块不重复添加。system 自带断点时跳过工具
/// 断点，避免把默认 5 分钟排在客户端 1 小时断点之前。
pub(crate) fn mark_request_cache_breakpoints(body: &mut Value) {
    let mut budget = 4_usize.saturating_sub(cache_control_count(body));
    let one_hour = has_one_hour_cache(body);
    let client_system_has_cache = body.get("system").is_some_and(|system| match system {
        Value::Array(blocks) => blocks.iter().any(|block| {
            block
                .get("cache_control")
                .is_some_and(|value| !value.is_null())
        }),
        Value::Object(block) => block
            .get("cache_control")
            .is_some_and(|value| !value.is_null()),
        _ => false,
    });
    if let Some(text) = body.get("system").and_then(Value::as_str) {
        if !text.is_empty() && !one_hour && budget > 0 {
            let text = text.to_owned();
            body["system"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"}
            }]);
            budget -= 1;
        }
    }
    if !client_system_has_cache && !one_hour && budget > 0 {
        if let Some(tool) = body
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .and_then(|tools| tools.last_mut())
            .and_then(Value::as_object_mut)
        {
            if !tool.contains_key("cache_control") {
                tool.insert("cache_control".into(), json!({"type": "ephemeral"}));
                budget -= 1;
            }
        }
    }
    if budget > 0 {
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            mark_cache_breakpoint(messages);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_system_breakpoint_suppresses_the_tool_breakpoint() {
        let mut body = json!({
            "system": [{
                "type": "text",
                "text": "stable",
                "cache_control": {"type": "ephemeral", "ttl": "1h"},
            }],
            "tools": [{"name": "read"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        });
        mark_request_cache_breakpoints(&mut body);
        assert!(
            body["tools"][0].get("cache_control").is_none(),
            "1h system 断点已覆盖工具前缀，不能再插 5m 工具断点"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn a_document_breakpoint_keeps_its_one_hour_ttl() {
        let mut messages = vec![json!({
            "role": "user",
            "content": [{
                "type": "document",
                "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0="},
                "cache_control": {"type": "ephemeral", "ttl": "1h"},
            }],
        })];
        mark_cache_breakpoint(&mut messages);
        assert_eq!(
            messages[0]["content"][0]["cache_control"]["ttl"], "1h",
            "客户端文档断点不能被默认 5m 覆盖"
        );
    }

    #[test]
    fn a_full_four_breakpoint_request_is_left_alone() {
        let system = (0..4)
            .map(|index| {
                json!({
                    "type": "text",
                    "text": format!("block {index}"),
                    "cache_control": {"type": "ephemeral", "ttl": "1h"},
                })
            })
            .collect::<Vec<_>>();
        let mut body = json!({
            "system": system,
            "tools": [{"name": "read"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        });
        mark_request_cache_breakpoints(&mut body);
        assert_eq!(cache_control_count(&body), 4);
        assert!(body["tools"][0].get("cache_control").is_none());
        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
    }

    #[test]
    fn three_client_breakpoints_leave_room_for_the_message_breakpoint() {
        let system = (0..3)
            .map(|index| {
                json!({
                    "type": "text",
                    "text": format!("block {index}"),
                    "cache_control": {"type": "ephemeral", "ttl": "1h"},
                })
            })
            .collect::<Vec<_>>();
        let mut body = json!({
            "system": system,
            "tools": [{"name": "read"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        });
        mark_request_cache_breakpoints(&mut body);
        assert_eq!(cache_control_count(&body), 4);
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn a_message_one_hour_breakpoint_keeps_earlier_blocks_unmarked() {
        let mut body = json!({
            "system": "stable",
            "tools": [{"name": "read"}],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "document",
                    "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0="},
                    "cache_control": {"type": "ephemeral", "ttl": "1h"},
                }],
            }],
        });
        mark_request_cache_breakpoints(&mut body);
        assert!(
            body["system"].is_string(),
            "不能在前面插 5 分钟 system 断点"
        );
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
    }
}
