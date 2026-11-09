//! Defines Run events and terminal outcomes.

use std::time::Duration;

use tokio::sync::oneshot;

use crate::model::{CheckpointId, ToolCall, ToolRoundId, Usage};

/// 一次上游失败在 run 层仍然需要的分类与状态。
///
/// 文本保持原来的形状（重试与压缩判定按它判断），另外三个字段只用于最终成文：
/// 稳定码、真实 HTTP 状态和面向客户端的安全摘要。`Provider` 分支以前只有一个
/// `String`，到达客户端前分类就丢了。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderFailure {
    message: String,
    code: &'static str,
    status: Option<u16>,
    summary: String,
    /// provider 流里已经拿到的调用编号；run 层没有更新编号时用它定位。
    reference: Option<String>,
}

impl ProviderFailure {
    fn new(message: String, code: &'static str, status: Option<u16>, summary: String) -> Self {
        Self {
            message,
            code,
            status,
            summary,
            reference: None,
        }
    }

    fn with_reference(mut self, reference: Option<String>) -> Self {
        self.reference = reference;
        self
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn status(&self) -> Option<u16> {
        self.status
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// 恢复成面向客户端的错误：分类与定位编号一起保留。
    pub(crate) fn into_error(self, fallback_reference: &str) -> crate::Error {
        crate::Error::conversation_parts(
            self.reference.as_deref().unwrap_or(fallback_reference),
            self.code,
            self.status,
            self.summary,
            self.message,
        )
    }
}

impl std::ops::Deref for ProviderFailure {
    type Target = str;

    fn deref(&self) -> &str {
        &self.message
    }
}

impl From<String> for ProviderFailure {
    fn from(message: String) -> Self {
        let summary = provider_head(&message);
        Self::new(message, crate::error::code::STREAM, None, summary)
    }
}

impl From<&str> for ProviderFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

fn provider_head(message: &str) -> String {
    crate::error::provider_summary(message)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunFailure {
    Protocol(String),
    Provider(ProviderFailure),
    /// 模型在回答中途撞到长度上限就停了。
    ///
    /// 必须与 `Provider` 分开：provider 层把 `max_tokens`（输出预算用完）与
    /// `model_context_window_exceeded`（窗口在生成中途打满）归一成同一个
    /// `FinishReason::Length`，run 层看不到 stop_reason 原文，只有靠 provider 自己
    /// 报的用量才能把两者分开——窗口打满要回到溢出压缩阶梯，输出被截断则压缩无用。
    /// 归一成 `Provider` 会让"每次重试都撞同一份超限 prompt"变成硬失败。
    Truncated(String),
    Store(String),
    Client(String),
}

impl RunFailure {
    pub fn category(&self) -> &'static str {
        match self {
            Self::Protocol(_) => "protocol",
            Self::Provider(_) | Self::Truncated(_) => "provider",
            Self::Store(_) => "store",
            Self::Client(_) => "client",
        }
    }

    /// 落库/日志用的完整失败文本（与旧版 `Provider(String)` 的内容一致）。
    pub fn message(&self) -> &str {
        match self {
            Self::Protocol(message)
            | Self::Truncated(message)
            | Self::Store(message)
            | Self::Client(message) => message,
            Self::Provider(failure) => failure.message(),
        }
    }
}

impl From<crate::Error> for RunFailure {
    fn from(error: crate::Error) -> Self {
        use crate::error::code;
        use crate::Error;
        match error {
            Error::Protocol(message) | Error::Config(message) => Self::Protocol(message),
            Error::Provider(message) => Self::Provider(message.into()),
            Error::UpstreamRejected {
                label,
                status,
                body,
            } => {
                // 保持旧文案的形状（`<label> <status>: <body>`）：run 层的重试判定与
                // 上下文溢出判定都按这段文本判断，新分类不能改变它们的结论。
                Self::Provider(ProviderFailure::new(
                    crate::error::upstream_rejection_message(&label, status, &body),
                    code::UPSTREAM_HTTP,
                    Some(status),
                    format!("{label} returned HTTP {status}"),
                ))
            }
            Error::Network(message) => Self::Provider(ProviderFailure::new(
                message,
                code::NETWORK,
                None,
                "network error while contacting the provider".into(),
            )),
            Error::Timeout(message) => Self::Provider(ProviderFailure::new(
                message,
                code::TIMEOUT,
                None,
                "the provider did not respond before the timeout".into(),
            )),
            Error::Conversation {
                code,
                summary,
                reference,
                status,
                detail,
            } => Self::Provider(
                ProviderFailure::new(detail, code, status, summary).with_reference(Some(reference)),
            ),
            Error::Store(message) => Self::Store(message),
            Error::Cancelled => Self::Client("run was cancelled".into()),
            // 这里的字符串会一路进日志、进 UI、进重试提示，所以带上整条错误链：
            // 只留 `error sending request for url (...)` 是过去排查不出原因的根源。
            Error::Http(error) => {
                let message = crate::diagnostics::error_chain(&error);
                let timed_out = error.is_timeout();
                Self::Provider(ProviderFailure::new(
                    message,
                    if timed_out {
                        code::TIMEOUT
                    } else {
                        code::NETWORK
                    },
                    error.status().map(|status| status.as_u16()),
                    if timed_out {
                        "the provider did not respond before the timeout".into()
                    } else {
                        "network error while contacting the provider".into()
                    },
                ))
            }
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
    ToolResult {
        call_id: String,
        interrupted: bool,
        /// 这一次提交是否收尾了整轮工具（它是最后一个结果）。
        ///
        /// 必须随事件传递，不能让接收方事后查库：并行的工具结果可能在接收方处理前
        /// 全部提交，实时轮状态会把"非收尾提交"看成已收尾，从而要求一个不属于它的
        /// completion barrier（`settled tool round has no completion barrier`）。
        settled: bool,
    },
    FinalTurn,
    Compaction {
        summary: String,
        summary_message_id: String,
    },
    RuntimeEvent {
        event_id: String,
    },
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
    /// 等待后台子代理期间的保活帧：复用客户端已有的 HeartbeatUpdate，不新造协议。
    Heartbeat,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code;

    #[test]
    fn upstream_rejections_keep_the_status_and_the_old_message_shape() {
        let failure = RunFailure::from(crate::Error::UpstreamRejected {
            label: "OpenAI Chat".into(),
            status: 400,
            body: "invalid request".into(),
        });
        let RunFailure::Provider(failure) = &failure else {
            panic!("an upstream rejection stays a provider failure");
        };
        assert_eq!(failure.code(), code::UPSTREAM_HTTP);
        assert_eq!(failure.status(), Some(400));
        // 旧文案形状是 run 层重试/压缩判定的输入：`is_rejected_request` 靠冒号前的
        // 3 位状态数字识别 4xx，这里必须保持原样。
        assert_eq!(
            failure.message(),
            "OpenAI Chat 400 Bad Request: invalid request"
        );
    }

    #[test]
    fn network_and_timeout_keep_their_typed_codes() {
        let network = RunFailure::from(crate::Error::Network("connect refused".into()));
        let RunFailure::Provider(network) = &network else {
            panic!("network failures stay provider failures");
        };
        assert_eq!(network.code(), code::NETWORK);

        let timeout = RunFailure::from(crate::Error::Timeout("no response".into()));
        let RunFailure::Provider(timeout) = &timeout else {
            panic!("timeout failures stay provider failures");
        };
        assert_eq!(timeout.code(), code::TIMEOUT);
    }

    #[test]
    fn provider_text_keeps_its_previous_meaning() {
        let failure = RunFailure::from(crate::Error::Provider(
            "Anthropic 400 Bad Request: prompt is too long".into(),
        ));
        let RunFailure::Provider(failure) = &failure else {
            panic!("provider error");
        };
        assert_eq!(failure.code(), code::STREAM);
        assert_eq!(
            failure.message(),
            "Anthropic 400 Bad Request: prompt is too long"
        );
        assert_eq!(failure.status(), None);
    }

    /// 调用编号在 provider 流里已经拿到时，run 层不能把它换成入口请求编号。
    #[test]
    fn an_existing_call_reference_survives_the_run_layer() {
        let error =
            crate::Error::Provider("Anthropic 400 Bad Request: body".into()).conversation("run:3");
        let failure = RunFailure::from(error);
        let RunFailure::Provider(failure) = failure else {
            panic!("provider error");
        };
        assert_eq!(failure.message(), "Anthropic 400 Bad Request: body");
        assert_eq!(failure.summary(), "Anthropic 400 Bad Request");

        let error = failure.into_error("request-id");
        assert!(error.public_message().contains("ref run:3"));
        assert!(!error.public_message().contains("ref request-id"));
    }
}
