//! Defines Run identity, preparation, and action types.
use serde::{Deserialize, Serialize};

use super::{
    CanonicalMessage, CheckpointId, ConversationId, ModelSpec, PromptSpec, RunId, ToolCall,
    ToolRoundAssistant,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SubagentKind {
    GeneralPurpose,
    Named(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunKind {
    Root,
    /// 由父会话派生的子代理运行。
    ///
    /// 父身份来自客户端随 append 带来的两个头（`x-parent-request-id` /
    /// `x-parent-agent-tool-call-id`），它们是**可选**的：Cursor 这两个头是分别发的，
    /// 只出现一个、或都不出现都是合法请求（准入不能因此拒绝，见 error_lifecycle 的用例）。
    /// 所以父身份也按"有就记、没有就留空"落库，而不是伪造一个父。
    Subagent {
        parent_run_id: Option<RunId>,
        parent_tool_call_id: Option<String>,
        kind: SubagentKind,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum SubagentModelOverride {
    Explicit(ModelSpec),
    Inherit,
    Disabled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum RunAction {
    Start,
    Compact,
    Resume {
        pending_tool_round: Option<RecoveredToolRound>,
    },
    /// 这一轮只是把一条后台完成通知带进会话。
    ///
    /// 同一条通知可能由服务端和客户端各发一次（身份相同、正文略有差别），所以
    /// 它必须是幂等投递：通知已经在历史里时，这一轮不该再跑模型。
    BackgroundCompletion,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RecoveredToolRound {
    pub assistant: ToolRoundAssistant,
    pub calls: Vec<ToolCall>,
    pub started_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PreparedRun {
    pub run_id: RunId,
    pub cursor_request_id: Option<String>,
    pub conversation_id: ConversationId,
    pub kind: RunKind,
    pub model: ModelSpec,
    pub prompt: PromptSpec,
    pub initial_messages: Vec<CanonicalMessage>,
    pub action: RunAction,
    pub base_checkpoint_id: CheckpointId,
}
