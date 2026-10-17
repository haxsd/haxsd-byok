//! Orders Bidi messages by append sequence number.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// 缺口最长容忍时间：超过它就把已经攒下的消息按序交出去（缺口丢掉）并 warn。
///
/// 乱序只可能来自传输层的重排或丢包，而"等一个永远不会来的 seqno"会让整条 run 静默
/// 停住：没有错误、没有 END_STREAM、没有日志。宁可丢掉一条消息继续跑，也不要无声卡死。
const GAP_TIMEOUT: Duration = Duration::from_secs(10);

/// `pending` 的容量上限：缺口补不上时，消息不能再无界攒下去。攒到上限就与超时同样
/// 处理——放弃缺口、按序交付。
const PENDING_LIMIT: usize = 1024;

pub struct OrderedInbox<T> {
    next: i64,
    pending: BTreeMap<i64, T>,
    /// 当前这个缺口是什么时候开始的；没有缺口时是 `None`。
    gap_since: Option<Instant>,
}

impl<T> OrderedInbox<T> {
    pub fn starting_at(next: i64) -> Self {
        Self {
            next,
            pending: BTreeMap::new(),
            gap_since: None,
        }
    }

    /// 收下一条 append，返回现在可以按序交付的消息。
    ///
    /// 起点由首个 append 决定：transport 被替换（换模型、重新附着）后新 actor 收到的
    /// 第一条 append 是客户端当前的序号，`N > 0` 很正常。起点硬编码 0 时这条消息会落进
    /// 缺口里等 `0`，而 `0` 永远不会再来——之后每条 append（RunRequest、工具结果、KV
    /// 应答）都会滞留，客户端只看到一直转圈。
    pub fn push(&mut self, seqno: i64, value: T) -> Vec<(i64, T)> {
        self.push_at(Instant::now(), seqno, value)
    }

    /// [`push`](Self::push) 的本体；`now` 是唯一的外部读数，测试用它推进"缺口等了多久"。
    fn push_at(&mut self, now: Instant, seqno: i64, value: T) -> Vec<(i64, T)> {
        if seqno < self.next || self.pending.contains_key(&seqno) {
            return Vec::new();
        }
        if self.next == 0 && self.pending.is_empty() && seqno > 0 {
            self.next = seqno;
        }
        self.pending.insert(seqno, value);
        let ready = self.drain_contiguous();
        if !ready.is_empty() {
            return ready;
        }
        // 到手的消息构不成连续段：记下缺口的起点，补不上就放弃它。
        let since = *self.gap_since.get_or_insert(now);
        if self.pending.len() > PENDING_LIMIT {
            tracing::warn!(
                expected = self.next,
                pending = self.pending.len(),
                "Cursor append backlog exceeded its limit; delivering the messages after the gap"
            );
            return self.skip_gap();
        }
        if now.saturating_duration_since(since) < GAP_TIMEOUT {
            return Vec::new();
        }
        tracing::warn!(
            expected = self.next,
            waited_ms = now.saturating_duration_since(since).as_millis() as u64,
            pending = self.pending.len(),
            "Cursor append gap was never filled; delivering the messages after it"
        );
        self.skip_gap()
    }

    /// 交付 `next` 起的连续段。
    fn drain_contiguous(&mut self) -> Vec<(i64, T)> {
        let mut ready = Vec::new();
        while let Some(value) = self.pending.remove(&self.next) {
            ready.push((self.next, value));
            self.next = self.next.saturating_add(1);
        }
        if self.pending.is_empty() {
            self.gap_since = None;
        }
        ready
    }

    /// 放弃当前缺口：把 `pending` 里最小的序号当作新的 `next`，再交付连续段。
    fn skip_gap(&mut self) -> Vec<(i64, T)> {
        let Some(oldest) = self.pending.keys().next().copied() else {
            self.gap_since = None;
            return Vec::new();
        };
        self.next = oldest;
        self.gap_since = None;
        self.drain_contiguous()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 新 transport 的第一条 append 带着客户端当前的序号（而不是 0）：起点必须对齐到
    /// 它，否则这条消息等一个永远不会来的缺口，之后的每条 append 一起滞留。
    #[test]
    fn the_first_append_decides_the_starting_point() {
        let mut inbox = OrderedInbox::starting_at(0);

        assert_eq!(inbox.push(7, "resume"), vec![(7, "resume")]);
        assert_eq!(inbox.push(8, "next"), vec![(8, "next")]);
    }

    /// 起点仍从 0 开始：序号 0 是合法的第一条，不能因为没有更小的序号就把它当成缺口。
    #[test]
    fn a_stream_that_starts_at_zero_is_delivered_in_order() {
        let mut inbox = OrderedInbox::starting_at(0);

        assert_eq!(inbox.push(0, "first"), vec![(0, "first")]);
        assert!(inbox.push(2, "third").is_empty(), "缺 1：先等它");
        assert_eq!(inbox.push(1, "second"), vec![(1, "second"), (2, "third")]);
    }

    /// 重复与迟到的序号直接丢：它们已经被交付过。
    #[test]
    fn duplicates_and_late_appends_are_dropped() {
        let mut inbox = OrderedInbox::starting_at(0);
        assert_eq!(inbox.push(0, "a"), vec![(0, "a")]);
        assert!(inbox.push(0, "a-again").is_empty());
        assert!(inbox.push(-1, "stale").is_empty());
    }

    /// 缺口补不上就丢掉缺口继续跑：超时后已攒下的消息按序交付，run 不会静默停住。
    #[test]
    fn a_gap_that_is_never_filled_is_dropped_after_the_timeout() {
        let start = Instant::now();
        let mut inbox = OrderedInbox::starting_at(0);
        assert_eq!(inbox.push_at(start, 0, 0), vec![(0, 0)]);

        assert!(
            inbox.push_at(start, 2, 2).is_empty(),
            "缺口刚开始：再等等丢掉的 1"
        );
        assert!(
            inbox
                .push_at(start + GAP_TIMEOUT - Duration::from_millis(1), 3, 3)
                .is_empty(),
            "还没到点,继续等"
        );

        assert_eq!(
            inbox.push_at(start + GAP_TIMEOUT, 4, 4),
            vec![(2, 2), (3, 3), (4, 4)],
            "到点就把缺口之后的消息按序交出去"
        );
    }

    /// 缺口等超时之前补齐：一条不丢，也不会把后面的消息提前交出去。
    #[test]
    fn a_gap_that_is_filled_in_time_keeps_every_message() {
        let start = Instant::now();
        let mut inbox = OrderedInbox::starting_at(0);
        assert_eq!(inbox.push_at(start, 0, 0), vec![(0, 0)]);
        assert!(inbox.push_at(start, 2, 2).is_empty());

        assert_eq!(
            inbox.push_at(start + Duration::from_millis(1), 1, 1),
            vec![(1, 1), (2, 2)]
        );
    }

    /// 缺口补不上时消息不能无界攒下去：攒到上限就与超时同样处理。
    #[test]
    fn an_unfilled_gap_cannot_grow_the_backlog_without_bound() {
        let start = Instant::now();
        let mut inbox = OrderedInbox::starting_at(0);
        assert_eq!(inbox.push_at(start, 0, 0), vec![(0, 0)]);

        // 1 永远不来：2 起全部落在 pending 里，攒到超过上限的那一条触发交付。
        let trigger = PENDING_LIMIT as i64 + 2;
        for seqno in 2..=trigger {
            let ready = inbox.push_at(start, seqno, seqno);
            if seqno < trigger {
                assert!(ready.is_empty(), "seqno {seqno} 还没到上限");
                continue;
            }
            assert_eq!(
                ready.len(),
                PENDING_LIMIT + 1,
                "到上限就把 pending 全部按序交出去"
            );
            assert_eq!(ready[0], (2, 2));
            assert_eq!(ready[ready.len() - 1], (seqno, seqno));
        }
    }
}
