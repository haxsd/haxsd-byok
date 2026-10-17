//! Defines retry policy for one logical model call.

use std::time::Duration;

use super::{ModelCycleFailure, RunFailure};

pub(super) const MAX_MODEL_RETRIES: u32 = 8;
pub(super) const MODEL_RETRY_DELAY: Duration = Duration::from_secs(5);

/// 协议违规（上游发的事件序列不自洽：Done 之后还有事件、引用了未知的工具下标……）
/// 最多重试一次。
///
/// 这类失败不是"上游偶尔抽风"：重试拿的是同一份请求与同一个适配器，同一个违规会照样
/// 复现，而每次重试都要等 `MODEL_RETRY_DELAY`，用户看到的是几十秒静默。留一次机会只
/// 为"畸形帧确实偶发"这一种情况——与网络/上游失败（允许跑满 8 次）区别对待。
const MAX_PROTOCOL_RETRIES: u32 = 1;

pub(super) fn should_retry(failure: &ModelCycleFailure, retries: u32) -> bool {
    if !failure.retryable {
        return false;
    }
    if matches!(failure.failure, RunFailure::Protocol(_)) {
        return retries < MAX_PROTOCOL_RETRIES;
    }
    retries < MAX_MODEL_RETRIES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{ModelCycleFailure, RunFailure};

    fn failure(retryable: bool) -> ModelCycleFailure {
        ModelCycleFailure {
            failure: RunFailure::Provider("failed".into()),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            replay_state: None,
            usage: None,
            retryable,
        }
    }

    fn protocol_failure() -> ModelCycleFailure {
        ModelCycleFailure {
            failure: RunFailure::Protocol("provider emitted an event after Done".into()),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            replay_state: None,
            usage: None,
            retryable: true,
        }
    }

    #[test]
    fn permits_eight_retries_after_the_initial_attempt() {
        let retryable = failure(true);
        for retries in 0..MAX_MODEL_RETRIES {
            assert!(should_retry(&retryable, retries));
        }
        assert!(!should_retry(&retryable, MAX_MODEL_RETRIES));
    }

    #[test]
    fn terminal_failures_never_retry() {
        assert!(!should_retry(&failure(false), 0));
    }

    /// 协议违规只给一次机会：同一个违规重试八次只是把错误推迟四十秒。
    #[test]
    fn protocol_violations_retry_at_most_once() {
        let violation = protocol_failure();
        assert!(should_retry(&violation, 0), "第一次违规给一次重试机会");
        assert!(!should_retry(&violation, MAX_PROTOCOL_RETRIES));
        assert!(!should_retry(&violation, MAX_MODEL_RETRIES));
    }
}
