//! Buffers, replays, broadcasts, and atomically closes downstream output.

use bytes::Bytes;
use tokio::sync::mpsc;

/// 每个订阅者能积压的帧数上界。
///
/// RunSSE 的读取速度由 hyper 按 socket 反压决定：客户端读得慢、连接半死、或者干脆
/// 不再 poll 响应体时，帧就堆在订阅队列里。无界队列于是让内存随 run 的输出总量增长，
/// 而且没有任何信号。满了就断开这个订阅者：它的 SSE 流正常结束（EOF），客户端重连时
/// 从 `history` 的重放里补齐——断线重连本来就是这份重放存在的原因。
const SUBSCRIBER_QUEUE_FRAMES: usize = 1024;

#[derive(Default)]
pub struct OutputHub {
    state: parking_lot::Mutex<OutputState>,
}

#[derive(Default)]
struct OutputState {
    history: Vec<Bytes>,
    subscribers: Vec<mpsc::Sender<Bytes>>,
    closed: bool,
}

impl OutputHub {
    pub fn emit(&self, frame: Bytes) -> bool {
        let mut state = self.state.lock();
        if state.closed {
            return false;
        }
        state.history.push(frame.clone());
        let mut dropped = 0_usize;
        state
            .subscribers
            .retain(|subscriber| match subscriber.try_send(frame.clone()) {
                Ok(()) => true,
                // 队列满：这个订阅者已经跟不上，断开它比让它无界攒下去强。它的接收端
                // 收完已排队的帧就看到 EOF，重连后从 history 重放里补齐。
                Err(mpsc::error::TrySendError::Full(_)) => {
                    dropped += 1;
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            });
        drop(state);
        if dropped > 0 {
            tracing::warn!(
                dropped,
                queue_frames = SUBSCRIBER_QUEUE_FRAMES,
                "disconnecting Cursor output subscribers that cannot keep up"
            );
        }
        true
    }

    pub fn subscribe(&self) -> mpsc::Receiver<Bytes> {
        let mut state = self.state.lock();
        // 重放必须无损：容量至少覆盖当前 history，否则晚到的订阅者会拿到一段被截断的
        // run（它不是"慢"，只是来晚了）。帧是引用计数的 `Bytes`，队列里多出来的只是
        // 槽位而不是正文的又一份拷贝；运行期的积压仍由 `emit` 的 `try_send` 管。`
        let (sender, receiver) = mpsc::channel(SUBSCRIBER_QUEUE_FRAMES.max(state.history.len()));
        for frame in &state.history {
            let _ = sender.try_send(frame.clone());
        }
        if !state.closed {
            state.subscribers.push(sender);
        }
        receiver
    }

    pub fn close(&self) -> bool {
        let mut state = self.state.lock();
        if state.closed {
            return false;
        }
        state.closed = true;
        state.subscribers.clear();
        // 终止帧已经发给当时在场的每一个订阅者，它们的流到 EOF 收尾；但 history 留着：
        // `subscribe` 在 `closed` 之后仍然重放一次，而那个消费者是真实存在的——客户端
        // 是在 BidiAppend 之后才开 RunSSE 的，一轮很快就失败的 run（缺 blob、协议错误）
        // 会在 RunSSE 附着之前就收尾。它的帧随条目一起释放（`local` 里的条目在 actor
        // 退出后立刻被回收，这才是 history 的生命周期）。
        drop(state);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(text: &str) -> Bytes {
        Bytes::from(text.to_owned())
    }

    /// 晚到的订阅者先拿到已经发出去的帧，再跟上实时帧。
    #[test]
    fn a_late_subscriber_replays_the_history_and_then_follows_the_stream() {
        let hub = OutputHub::default();
        hub.emit(frame("first"));
        hub.emit(frame("second"));

        let mut subscriber = hub.subscribe();
        hub.emit(frame("third"));

        for expected in ["first", "second", "third"] {
            assert_eq!(subscriber.try_recv().unwrap(), frame(expected));
        }
    }

    /// 读不动的订阅者不能再被无限喂帧：队列满了就断开它（接收端收完已排队的帧就看到
    /// EOF）。断开的是订阅者，不是这条 run。
    #[test]
    fn a_subscriber_that_cannot_keep_up_is_disconnected_instead_of_buffered() {
        let hub = OutputHub::default();
        let mut slow = hub.subscribe();
        for index in 0..SUBSCRIBER_QUEUE_FRAMES + 10 {
            assert!(hub.emit(frame(&index.to_string())), "emit 不收尾就一直在跑");
        }

        assert_eq!(hub.state.lock().subscribers.len(), 0, "慢订阅者必须被摘掉");
        let mut delivered = 0;
        while slow.try_recv().is_ok() {
            delivered += 1;
        }
        assert_eq!(
            delivered, SUBSCRIBER_QUEUE_FRAMES,
            "已经排队的帧仍然要交付,之后是 EOF"
        );
        assert!(matches!(
            slow.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    /// 晚到不等于慢：重放那一段必须无损，否则客户端会拿到一段被截断的 run。
    #[test]
    fn replay_is_lossless_even_when_the_history_exceeds_the_live_queue() {
        let hub = OutputHub::default();
        for index in 0..SUBSCRIBER_QUEUE_FRAMES * 2 {
            hub.emit(frame(&index.to_string()));
        }

        let mut late = hub.subscribe();
        let mut replayed = 0;
        while late.try_recv().is_ok() {
            replayed += 1;
        }
        assert_eq!(replayed, SUBSCRIBER_QUEUE_FRAMES * 2);
    }

    /// 收尾：终止帧已经交给当时在场的订阅者，它们的流到 EOF；此后不再接受新帧。
    /// 但收尾之后到达的 RunSSE 仍要能重放一次——客户端在 BidiAppend 之后才开 RunSSE，
    /// 一轮快速失败的 run 会在附着之前就收尾，那时它唯一的现场就是这份 history。
    #[test]
    fn closing_ends_the_stream_but_still_replays_once_for_a_late_subscriber() {
        let hub = OutputHub::default();
        let mut subscriber = hub.subscribe();
        hub.emit(frame("only"));

        assert!(hub.close());
        assert!(!hub.close(), "收尾只做一次");
        assert!(!hub.emit(frame("after-close")), "收尾后不再接受帧");

        assert_eq!(subscriber.try_recv().unwrap(), frame("only"));
        assert!(matches!(
            subscriber.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));

        let mut late = hub.subscribe();
        assert_eq!(late.try_recv().unwrap(), frame("only"));
        assert!(matches!(
            late.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
