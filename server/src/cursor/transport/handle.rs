//! Provides the request-scoped input, subscription, and terminal interface.

use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    cursor::{
        conversation::TransportCommand,
        protocol::{connect, proto::agent::v1 as pb},
        services::observability::CursorTraceRecorder,
    },
    Error, Result,
};

use super::{OutputHub, TransportAdmission, TransportLifecycle};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportParent {
    /// Cursor 会独立地发这两个头：子代理请求可能只带父请求 ID，或者只带父工具调用 ID。
    pub request_id: Option<String>,
    pub tool_call_id: Option<String>,
}

#[derive(Clone)]
pub struct TransportHandle {
    request_id: String,
    commands: mpsc::Sender<TransportCommand>,
    output: Arc<OutputHub>,
    conversation_id: Arc<OnceLock<String>>,
    parent: Arc<OnceLock<TransportParent>>,
    trace: CursorTraceRecorder,
    lifecycle: TransportLifecycle,
    disconnect: CancellationToken,
}

/// 一次性绑定：写入成功或"已经绑的就是同一个值"都算成功，绑过别的值就是冲突。
///
/// 不能写成"先 `get()` 判断再 `set()`"：两个并发 append 可能都读到空、都通过冲突检测，
/// 后到的 `set` 只会在 `OnceLock` 里静默失败（旧代码 `let _ = set(...)`），于是它带着
/// 一份没被采纳的 conversation/parent 继续跑——冲突检测等于没有。这里把判据放在 `set`
/// 的结果上：`OnceLock::set` 本身是原子的（只填空槽），失败说明槽已被占，比较的就是
/// 真正落下的那个值。
fn bind_once<T: PartialEq + Clone>(slot: &OnceLock<T>, value: &T) -> Result<(), ()> {
    if slot.set(value.clone()).is_ok() {
        return Ok(());
    }
    (slot.get() == Some(value)).then_some(()).ok_or(())
}

impl TransportHandle {
    pub(crate) fn new(
        request_id: String,
        commands: mpsc::Sender<TransportCommand>,
        output: Arc<OutputHub>,
        trace: CursorTraceRecorder,
    ) -> Self {
        Self {
            request_id,
            commands,
            output,
            conversation_id: Arc::new(OnceLock::new()),
            parent: Arc::new(OnceLock::new()),
            trace,
            lifecycle: TransportLifecycle::new(),
            disconnect: CancellationToken::new(),
        }
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn set_conversation_id(&self, conversation_id: &str) -> Result<()> {
        if conversation_id.is_empty() {
            return Err(Error::Protocol("Cursor conversation id is required".into()));
        }
        bind_once(&self.conversation_id, &conversation_id.to_owned()).map_err(|()| {
            Error::Protocol(format!(
                "conflicting conversation ids for request {}",
                self.request_id
            ))
        })
    }

    pub fn conversation_id(&self) -> Option<&str> {
        self.conversation_id.get().map(String::as_str)
    }

    pub fn set_parent(&self, parent: TransportParent) -> Result<()> {
        // 只有"两个都没有"或"给了空串"才拒绝：Cursor 允许只带其中一个头。
        if (parent.request_id.is_none() && parent.tool_call_id.is_none())
            || parent.request_id.as_ref().is_some_and(String::is_empty)
            || parent.tool_call_id.as_ref().is_some_and(String::is_empty)
        {
            return Err(Error::Protocol(
                "Cursor parent ids must not be empty".into(),
            ));
        }
        bind_once(&self.parent, &parent).map_err(|()| {
            Error::Protocol(format!(
                "conflicting parent ids for request {}",
                self.request_id
            ))
        })
    }

    pub fn parent(&self) -> Option<&TransportParent> {
        self.parent.get()
    }

    pub async fn command(&self, command: TransportCommand) -> Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| Error::RunNotFound(self.request_id.clone()))
    }

    pub async fn disconnect(&self) {
        let _ = self.commands.send(TransportCommand::Disconnect).await;
    }

    pub fn subscribe(&self) -> tokio::sync::mpsc::Receiver<Bytes> {
        self.output.subscribe()
    }

    pub fn emit_frame(&self, frame: Bytes) -> bool {
        self.output.emit(frame)
    }

    pub fn emit(&self, message: &pb::AgentServerMessage) -> Result<()> {
        if self.emit_frame(connect::encode_message(message)?) {
            Ok(())
        } else {
            Err(Error::RunNotFound(self.request_id.clone()))
        }
    }

    pub(crate) fn close_output(&self) -> bool {
        self.output.close()
    }

    pub(crate) fn trace(&self) -> Option<&CursorTraceRecorder> {
        Some(&self.trace)
    }

    pub(crate) fn accepting_appends(&self) -> bool {
        self.lifecycle.is_open()
    }

    pub(crate) fn admit(&self) -> Result<TransportAdmission> {
        self.lifecycle
            .admit()
            .ok_or_else(|| Error::RunNotFound(self.request_id.clone()))
    }

    pub(crate) fn begin_close(&self) {
        self.lifecycle.begin_close();
    }

    pub(crate) fn admissions_drained(&self) -> bool {
        self.lifecycle.admissions_drained()
    }

    pub(crate) async fn wait_admissions_drained(&self) {
        self.lifecycle.wait_admissions_drained().await;
    }

    pub(crate) fn mark_draining(&self) {
        self.lifecycle.mark_draining();
    }

    pub(crate) fn reopen(&self) {
        self.lifecycle.reopen();
    }

    pub(crate) fn close_transport(&self) {
        self.lifecycle.close();
    }

    pub(crate) async fn wait_transport_closed(&self) {
        self.lifecycle.wait_closed().await;
    }

    pub(crate) fn disconnect_token(&self) -> CancellationToken {
        self.disconnect.clone()
    }

    pub(crate) fn mark_disconnected(&self) {
        self.disconnect.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    /// 槽已被占、且占的不是同一个值时，`bind_once` 必须报冲突——旧代码正是在这里
    /// `let _ = set(...)` 把结果丢掉，于是并发落败的那一方也返回成功。
    #[test]
    fn binding_a_different_value_after_the_slot_is_taken_is_a_conflict() {
        let slot = OnceLock::new();
        assert_eq!(bind_once(&slot, &"A".to_owned()), Ok(()));
        assert_eq!(
            bind_once(&slot, &"A".to_owned()),
            Ok(()),
            "重复绑同一个值是正常路径（每次 append 都会走）"
        );
        assert_eq!(bind_once(&slot, &"B".to_owned()), Err(()));
        assert_eq!(slot.get().map(String::as_str), Some("A"), "先到的值不动");
    }

    /// 并发绑定同一个槽：无论怎么交错，只能有一方成功。
    ///
    /// 一轮里两个线程在 barrier 处对齐后同时写，跑很多轮——旧实现（先 get 判断、再 set
    /// 且丢弃结果）在这个窗口里会让两方都返回成功，冲突检测形同虚设。
    #[test]
    fn concurrent_binds_of_one_slot_let_exactly_one_side_win() {
        const ROUNDS: usize = 400;

        for round in 0..ROUNDS {
            let slot = Arc::new(OnceLock::<String>::new());
            let barrier = Arc::new(Barrier::new(2));
            let spawn = |value: &'static str| {
                let slot = slot.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    bind_once(&slot, &value.to_owned())
                })
            };
            let left = spawn("conversation-a");
            let right = spawn("conversation-b");

            let results = [left.join().unwrap(), right.join().unwrap()];
            let winners = results.iter().filter(|result| result.is_ok()).count();
            assert_eq!(
                winners, 1,
                "第 {round} 轮有两个成功者：{results:?}，绑定不是原子的"
            );
        }
    }

    /// 走公开接口：同一个 conversation id 重复绑定是常态，换一个就是冲突。
    #[tokio::test]
    async fn rebinding_a_conversation_to_another_id_is_refused() {
        let (_directory, registry) = crate::cursor::transport::test_registry().await;
        let handle = registry.get_or_create("handle-bind").await.unwrap();

        handle.set_conversation_id("conversation-a").unwrap();
        assert!(
            handle.set_conversation_id("conversation-a").is_ok(),
            "每次 append 都会重绑同一个 id，必须照常通过"
        );
        assert!(
            handle.set_conversation_id("conversation-b").is_err(),
            "换一个 id 必须报冲突"
        );
        assert_eq!(handle.conversation_id(), Some("conversation-a"));
    }
}
