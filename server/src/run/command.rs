//! Defines Run commands and their explicit delivery results.

use tokio::sync::oneshot;

use crate::model::{CanonicalMessage, ToolResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandResult {
    Applied,
    Duplicate,
    RunClosing,
    RunEnded,
    /// 这个会话是被用户停止（客户端断开）的：唤醒类消息直接丢弃，不再把它拉起来。
    Suppressed,
    StaleTarget,
}

#[derive(Debug)]
pub struct MessageBatch {
    pub event_id: String,
    pub messages: Vec<CanonicalMessage>,
    pub result: oneshot::Sender<CommandResult>,
}

impl MessageBatch {
    pub fn complete(self, result: CommandResult) {
        let _ = self.result.send(result);
    }
}

#[derive(Debug)]
pub enum RunCommand {
    ToolResult(ToolResult),
    InsertMessages(MessageBatch),
    BreakMessages(MessageBatch),
    Cancel,
}
