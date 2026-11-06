//! Anthropic 的共享判定：`stop_reason` 分类、思考预算边界。

use serde::{Deserialize, Serialize};

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
