//! Remembers how the official model-catalog fetch went.
//!
//! 混合模式的官方模型列表来自上游（`AvailableModels` / `GetUsableModels`）。上游
//! 拒绝或连不上时，我们只能退化成"只有本机模型"——那是用户看不见的静默降级，
//! 也是"为什么看不到官方模型"这类问题的根源。这份快照把它变成可读状态：最近一次
//! 成功、最近一次失败与原因。它只活在本次运行里，不落库：状态接口要回答的是
//! "现在这条链路通不通"。
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;

use crate::store::now_ms;

#[derive(Clone, Debug, Default, Serialize)]
pub struct CatalogHealth {
    pub last_success_ms: Option<i64>,
    pub last_failure_ms: Option<i64>,
    pub last_failure: Option<String>,
}

impl CatalogHealth {
    /// 最近一次结果是不是失败：没有成功过，或失败发生在最后一次成功之后。
    pub fn is_degraded(&self) -> bool {
        match (self.last_success_ms, self.last_failure_ms) {
            (Some(success), Some(failure)) => failure > success,
            (None, Some(_)) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Default)]
pub struct CatalogHealthTracker {
    health: Arc<Mutex<CatalogHealth>>,
}

impl CatalogHealthTracker {
    pub fn snapshot(&self) -> CatalogHealth {
        self.health.lock().clone()
    }

    pub fn record_success(&self) {
        let mut health = self.health.lock();
        health.last_success_ms = Some(next_stamp(&health));
    }

    pub fn record_failure(&self, reason: impl Into<String>) {
        let mut health = self.health.lock();
        health.last_failure_ms = Some(next_stamp(&health));
        health.last_failure = Some(reason.into());
    }
}

/// 记事件用的时间戳，保证严格递增。
///
/// `is_degraded` 靠时间戳分先后，而 `now_ms()` 只有毫秒精度：同一毫秒内先成功后失败
/// （刚探测成功就又收到 401 是常事）会被判成"失败是旧的"，状态就翻不过来了。
fn next_stamp(health: &CatalogHealth) -> i64 {
    let latest = [health.last_success_ms, health.last_failure_ms]
        .into_iter()
        .flatten()
        .max();
    now_ms().max(latest.map_or(i64::MIN, |value| value + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_after_a_success_is_degraded_and_a_later_success_recovers() {
        let tracker = CatalogHealthTracker::default();
        assert!(!tracker.snapshot().is_degraded());

        tracker.record_success();
        assert!(!tracker.snapshot().is_degraded());

        tracker.record_failure("401 Unauthorized");
        let degraded = tracker.snapshot();
        assert!(degraded.is_degraded());
        assert_eq!(degraded.last_failure.as_deref(), Some("401 Unauthorized"));

        tracker.record_success();
        assert!(!tracker.snapshot().is_degraded());
    }

    #[test]
    fn a_tracker_that_never_succeeded_is_degraded_once_it_fails() {
        let tracker = CatalogHealthTracker::default();
        tracker.record_failure("network: dns error");
        assert!(tracker.snapshot().is_degraded());
    }
}
