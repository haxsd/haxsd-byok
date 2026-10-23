//! Defines normalized provider streaming events.
use crate::model::{ProviderReplayState, Usage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolUse,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelEvent {
    Start {
        model_call_id: String,
    },
    TextStart,
    TextDelta(String),
    TextEnd,
    ThinkingStart,
    ThinkingDelta(String),
    /// Anthropic 的思考块签名。适配器在 `ThinkingEnd` **之前**发出，入站网关据它
    /// 生成 `signature_delta`；其它消费者忽略。
    ThinkingSignature(String),
    ThinkingEnd,
    ToolCallStart {
        index: usize,
        call_id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        index: usize,
        delta: String,
    },
    ToolCallEnd {
        index: usize,
    },
    ProviderReplayState(ProviderReplayState),
    Usage(Usage),
    /// 上游明确报告了命中的停止序列（Anthropic 的 `delta.stop_sequence`）。
    ///
    /// 只有入站 Anthropic 网关会把它翻回 `stop_reason: "stop_sequence"` 与
    /// `stop_sequence` 字段；其它消费者（Cursor / Devin / Codex 记录）显式忽略——
    /// 它们的协议里没有这个字段，`Done(Stop)` 已经是全部语义。
    StopSequence(String),
    Done(FinishReason),
}

/// Returns whether an event represents the first valid upstream response.
/// Transport markers, replay metadata, usage, completion, and provider heartbeats
/// are intentionally excluded; empty text/reasoning/tool deltas are valid events.
pub fn is_valid_response_event(event: &ModelEvent) -> bool {
    matches!(
        event,
        ModelEvent::TextDelta(_)
            | ModelEvent::ThinkingStart
            | ModelEvent::ThinkingDelta(_)
            | ModelEvent::ThinkingEnd
            | ModelEvent::ToolCallStart { .. }
            | ModelEvent::ToolCallArgumentsDelta { .. }
            | ModelEvent::ToolCallEnd { .. }
    )
}
