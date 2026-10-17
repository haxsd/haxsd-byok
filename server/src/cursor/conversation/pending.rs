//! Stores messages waiting across Run lifecycle boundaries.

use std::collections::{HashSet, VecDeque};

use super::CompiledMessages;

/// 单个会话最多排多少条待投递消息。
///
/// 投递方是子代理完成通知与插话，正常会话几条就够。无界只会在"客户端再也不开口"
/// （会话被关掉）时积累：那些条目的唯一消费者是下一次 run 的开头，永远不会来。
/// 超过上限丢最旧的一条——丢的是最不可能还被需要的那条。
const MAX_PENDING_MESSAGES: usize = 256;

#[derive(Default)]
pub struct PendingMessages {
    queued: VecDeque<CompiledMessages>,
    event_ids: HashSet<String>,
}

impl PendingMessages {
    pub fn push(&mut self, messages: CompiledMessages) -> bool {
        if !self.event_ids.insert(messages.event_id.clone()) {
            return false;
        }
        self.queued.push_back(messages);
        while self.queued.len() > MAX_PENDING_MESSAGES {
            let Some(dropped) = self.queued.pop_front() else {
                break;
            };
            self.event_ids.remove(&dropped.event_id);
            tracing::warn!(
                event_id = %dropped.event_id,
                limit = MAX_PENDING_MESSAGES,
                "dropping the oldest message waiting for the next Run"
            );
        }
        true
    }

    pub fn drain(&mut self) -> impl Iterator<Item = CompiledMessages> + '_ {
        self.event_ids.clear();
        self.queued.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingMessages, MAX_PENDING_MESSAGES};
    use crate::cursor::conversation::{CompiledMessages, MessageDelivery};

    fn message(event_id: &str) -> CompiledMessages {
        CompiledMessages {
            event_id: event_id.into(),
            target_run_id: None,
            messages: Vec::new(),
            delivery: MessageDelivery::InsertMessages,
        }
    }

    /// 同一身份只排一次。
    #[test]
    fn a_repeated_event_is_not_queued_twice() {
        let mut pending = PendingMessages::default();
        assert!(pending.push(message("wakeup-1")));
        assert!(!pending.push(message("wakeup-1")));
        assert_eq!(pending.drain().count(), 1);
    }

    /// 超过上限丢最旧：不开口的会话不能把待投递条目堆成无界。
    #[test]
    fn the_oldest_message_is_dropped_at_the_cap() {
        let mut pending = PendingMessages::default();
        for index in 0..(MAX_PENDING_MESSAGES + 2) {
            assert!(pending.push(message(&format!("wakeup-{index}"))));
        }
        let kept = pending
            .drain()
            .map(|entry| entry.event_id)
            .collect::<Vec<_>>();
        assert_eq!(kept.len(), MAX_PENDING_MESSAGES);
        assert_eq!(kept.first().map(String::as_str), Some("wakeup-2"));
        let expected_last = format!("wakeup-{}", MAX_PENDING_MESSAGES + 1);
        assert_eq!(
            kept.last().map(String::as_str),
            Some(expected_last.as_str())
        );
    }
}
