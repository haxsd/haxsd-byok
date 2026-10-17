//! 一次 Cursor 动作编译出来的消息批次，以及它以什么方式进入会话。

use crate::model::{CanonicalMessage, RunId};

/// 编译结果进入会话的方式：忽略、插入，或先打断当前周期再插入。
///
/// 由编译动作的调用方按动作类型直接选定（见 `cursor/conversation/`）：每个动作类型
/// 只有一条路径会编译它，所以"动作 → 交付方式"的映射写在调用点，而不是在这里再维护
/// 一张与调用点并列、却没有任何调用方查的表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageDelivery {
    Ignore,
    InsertMessages,
    BreakMessages,
}

#[derive(Clone, Debug)]
pub struct CompiledMessages {
    pub event_id: String,
    pub target_run_id: Option<RunId>,
    pub messages: Vec<CanonicalMessage>,
    pub delivery: MessageDelivery,
}
