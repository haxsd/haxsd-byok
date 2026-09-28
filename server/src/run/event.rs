//! Defines Run events and terminal outcomes.

use std::time::Duration;

use tokio::sync::oneshot;

use crate::model::{CheckpointId, ToolCall, ToolRoundId, Usage};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunFailure {
    Protocol(String),
    Provider(String),
    Store(String),
    Client(String),
}

impl RunFailure {
    pub fn category(&self) -> &'static str {
        match self {
            Self::Protocol(_) => "protocol",
            Self::Provider(_) => "provider",
            Self::Store(_) => "store",
            Self::Client(_) => "client",
        }
    }
}

impl From<crate::Error> for RunFailure {
    fn from(error: crate::Error) -> Self {
        use crate::Error;
        match error {
            Error::Protocol(message) | Error::Config(message) => Self::Protocol(message),
            Error::Provider(message) => Self::Provider(message),
            Error::Store(message) => Self::Store(message),
            Error::Cancelled => Self::Client("run was cancelled".into()),
            // 这里的字符串会一路进日志、进 UI、进重试提示，所以带上整条错误链：
            // 只留 `error sending request for url (...)` 是过去排查不出原因的根源。
            Error::Http(error) => Self::Provider(crate::diagnostics::error_chain(&error)),
            Error::Database(error) => Self::Store(crate::diagnostics::error_chain(&error)),
            Error::Migration(error) => Self::Store(crate::diagnostics::error_chain(&error)),
            Error::MigrationTimeout {
                stage,
                timeout_seconds,
            } => Self::Store(format!(
                "database migration stage '{stage}' timed out after {timeout_seconds} seconds"
            )),
            Error::Io(error) => Self::Store(crate::diagnostics::error_chain(&error)),
            Error::Decode(error) => Self::Protocol(crate::diagnostics::error_chain(&error)),
            Error::Encode(error) => Self::Protocol(crate::diagnostics::error_chain(&error)),
            Error::Json(error) => Self::Protocol(crate::diagnostics::error_chain(&error)),
            Error::RunNotFound(run_id) => Self::Store(format!("run not found: {run_id}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Cancelled,
    Failed(RunFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitCause {
    InitialMessages,
    ToolRoundStarted(ToolRoundId),
    ToolResult { call_id: String, interrupted: bool },
    FinalTurn,
    Compaction { summary: String },
    RuntimeEvent { event_id: String },
}

#[derive(Debug)]
pub enum CommitBarrier {
    None,
    BeforeContinue(oneshot::Sender<std::result::Result<(), String>>),
}

impl CommitBarrier {
    pub fn before_continue() -> (Self, oneshot::Receiver<std::result::Result<(), String>>) {
        let (sender, receiver) = oneshot::channel();
        (Self::BeforeContinue(sender), receiver)
    }

    pub fn is_required(&self) -> bool {
        matches!(self, Self::BeforeContinue(_))
    }

    pub fn complete(self, result: std::result::Result<(), String>) {
        if let Self::BeforeContinue(sender) = self {
            let _ = sender.send(result);
        }
    }
}

#[derive(Debug)]
pub struct MessagesCommitted {
    pub checkpoint_id: CheckpointId,
    pub tool_round_version: u64,
    pub cause: CommitCause,
    pub barrier: CommitBarrier,
}

#[derive(Debug)]
pub enum RunEvent {
    AutoCompactionStarted,
    AutoCompactionCompleted,
    CycleInterrupted,
    ModelAttemptFailed {
        attempt: u32,
        message: String,
    },
    TextStart,
    TextDelta(String),
    TextEnd,
    ThinkingStart,
    ThinkingDelta(String),
    ThinkingEnd {
        duration: Duration,
    },
    ToolCallStart {
        index: usize,
        call_id: String,
        name: String,
        model_call_id: String,
    },
    ToolCallArgumentsDelta {
        index: usize,
        delta: String,
    },
    ToolCallEnd {
        index: usize,
    },
    UsageSnapshot(Usage),
    Usage(Usage),
    ExecuteToolRound {
        round_id: ToolRoundId,
        calls: Vec<ToolCall>,
    },
    MessagesCommitted(MessagesCommitted),
    Ended(RunOutcome),
}
