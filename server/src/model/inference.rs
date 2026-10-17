//! Defines provider-independent model requests and streaming responses.
use serde::{Deserialize, Serialize};

use super::{ModelSpec, ProjectedContent, ProjectedMessage, ToolDefinition};

const PROVIDER_TOOL_CALL_ID_MAX_CHARS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PromptSpec {
    pub instructions: String,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelRequest {
    pub prompt: PromptSpec,
    pub model: ModelSpec,
    pub history: Vec<ProjectedMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelInvocation {
    pub client_source: super::CallClient,
    pub call_id: String,
    pub run_id: String,
    pub conversation_id: String,
    pub provider_call_index: u64,
    pub request: ModelRequest,
}

/// provider 可见、但**不会持久化**的瞬时尾巴消息身份。
///
/// run 侧在历史以 assistant 结尾时追加它（`run::history::user_terminated`）：provider 把
/// assistant 结尾的历史读成 prefill、Anthropic 直接拒绝，所以必须补一条 user 消息；但它
/// 不进库，所以它也不是 provider 历史与持久化历史的公共前缀的一部分。
pub const CONTINUE_TAIL_MESSAGE_ID: &str = "runtime:continue";
pub const COMPACTION_TAIL_MESSAGE_ID: &str = "compaction:instruction";

const TRANSIENT_TAIL_MESSAGE_IDS: [&str; 2] =
    [CONTINUE_TAIL_MESSAGE_ID, COMPACTION_TAIL_MESSAGE_ID];

impl ModelInvocation {
    /// 观测层写上下文用量锚点（`llm_calls.message_count`）用的"已持久化消息条数"。
    ///
    /// 锚点的语义是"这条**持久化**前缀 + 当时 provider 报的 input tokens"：下一个 run 拿
    /// 它当切片起点（`run::compaction::estimated_tokens`），只有计到持久化历史为止才对。
    /// `request.history.len()` 会把上面那条瞬时尾巴算进去，于是下标偏大一位——切片正好
    /// 从"锚点之后第一条新消息"的**下一条**开始，那条消息的 token 既不在锚点里、也不在
    /// 增量里（审计 09/F7）。尾巴只可能出现在末尾，所以这里只看最后一条。
    pub fn persisted_message_count(&self) -> usize {
        let history = &self.request.history;
        let transient = history.last().is_some_and(|message| {
            TRANSIENT_TAIL_MESSAGE_IDS.contains(&message.message_id.as_str())
        });
        history.len() - usize::from(transient)
    }
}

pub(crate) fn normalize_provider_tool_call_ids(history: &mut [ProjectedMessage]) {
    for message in history {
        match &mut message.content {
            ProjectedContent::Assistant { calls, .. } => {
                for call in calls {
                    truncate_tool_call_id(&mut call.call_id);
                }
            }
            ProjectedContent::ToolResult(result) => {
                truncate_tool_call_id(&mut result.call_id);
            }
            ProjectedContent::Parts(_) => {}
        }
    }
}

fn truncate_tool_call_id(call_id: &mut String) {
    if let Some((end, _)) = call_id.char_indices().nth(PROVIDER_TOOL_CALL_ID_MAX_CHARS) {
        call_id.truncate(end);
    }
}
