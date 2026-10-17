//! Hydrates request context blobs supplied by Cursor.
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use prost::Message;
use tokio::sync::watch;

use crate::{
    cursor::{protocol::proto::agent::v1 as pb, transport::TransportHandle},
    store::{BlobId, Store},
    Error, Result,
};

/// 等客户端回送请求上下文的时限。
const CONTEXT_TIMEOUT: Duration = Duration::from_secs(60);

/// 一次在飞加载的结果。
///
/// 用 `watch` 而不是 `oneshot` 是因为要共享：多个等待者拿同一份值。`Error` 不能克隆
/// （里面有 sqlx / io 的错误），所以失败只能带文本，等待者拿到 `Error::Protocol`。
#[derive(Clone)]
enum LoadOutcome {
    Context(Box<pb::RequestContext>),
    Failed(String),
}

/// 一次在飞的 `RequestContextArgs` 请求。
///
/// Cursor 只用 `id == 0` 标识这条 RPC，同一时刻只可能有一份在飞：后来的调用方共享同一份
/// 结果。以前第二个调用方直接拿到 "already being loaded"，而被 supersede 丢掉的那一代
/// 不清理槽位，于是下一个需要上下文的 run 整轮失败。
struct InFlight {
    result: watch::Sender<Option<LoadOutcome>>,
    /// 还没退出的参与者数量（发起者 + 共享结果的等待者）。
    waiters: AtomicUsize,
}

/// 参与一次在飞加载的凭证：最后一个退出的人负责把槽位放掉。
///
/// 请求发出去之后，结果什么时候来不由我们决定，所以槽位必须活到没有等待者的那一刻：
/// 被 supersede 丢掉的那一代用的是 `Drop`，如果它顺手把槽位留下，下一个 `load` 就会
/// 等一份永远不会被消费的结果（或者按老行为直接失败）。
struct Participant {
    slot: Arc<parking_lot::Mutex<Option<Arc<InFlight>>>>,
    in_flight: Arc<InFlight>,
}

impl Drop for Participant {
    fn drop(&mut self) {
        // 先拿锁再减计数：新加入的人也在同一把锁下加计数，两边不会擦肩而过
        // （擦肩会让计数瞬间归零，从而把还有人等的槽位清掉）。
        let mut slot = self.slot.lock();
        if self.in_flight.waiters.fetch_sub(1, Ordering::AcqRel) > 1 {
            return;
        }
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.in_flight))
        {
            *slot = None;
        }
    }
}

#[derive(Clone)]
pub(crate) struct RequestContextSynchronizer {
    handle: TransportHandle,
    store: Store,
    slot: Arc<parking_lot::Mutex<Option<Arc<InFlight>>>>,
}

impl RequestContextSynchronizer {
    pub(crate) fn new(handle: TransportHandle, store: Store) -> Self {
        Self {
            handle,
            store,
            slot: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    pub(crate) async fn refresh_if_missing(
        &self,
        references: &pb::RequestContextPartReferences,
        conversation_id: &str,
    ) -> Result<Option<pb::RequestContext>> {
        if !self.has_missing_part(references).await? {
            return Ok(None);
        }
        let context = self.load(conversation_id).await?;
        self.cache_parts(&context).await?;
        Ok(Some(context))
    }

    pub(crate) async fn get(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        self.store.get_blob(id).await
    }

    pub(crate) async fn load(&self, conversation_id: &str) -> Result<pb::RequestContext> {
        let (in_flight, issued) = {
            let mut slot = self.slot.lock();
            match slot.as_ref() {
                Some(in_flight) => {
                    in_flight.waiters.fetch_add(1, Ordering::AcqRel);
                    (in_flight.clone(), false)
                }
                None => {
                    let (result, _) = watch::channel(None);
                    let in_flight = Arc::new(InFlight {
                        result,
                        waiters: AtomicUsize::new(1),
                    });
                    *slot = Some(in_flight.clone());
                    (in_flight, true)
                }
            }
        };
        let _participant = Participant {
            slot: self.slot.clone(),
            in_flight: in_flight.clone(),
        };
        let mut receiver = in_flight.result.subscribe();

        if !issued {
            tracing::debug!(
                request_id = self.handle.request_id(),
                conversation_id,
                "sharing the in-flight Cursor context request"
            );
        } else {
            tracing::info!(
                request_id = self.handle.request_id(),
                conversation_id,
                "requesting uncached Cursor context"
            );

            if let Err(error) = self.handle.emit(&pb::AgentServerMessage {
                ttft_breakdown: None,
                message: Some(pb::agent_server_message::Message::ExecServerMessage(
                    pb::ExecServerMessage {
                        id: 0,
                        message: Some(pb::exec_server_message::Message::RequestContextArgs(
                            pb::RequestContextArgs {
                                notes_session_id: Some(conversation_id.into()),
                                ..Default::default()
                            },
                        )),
                        ..Default::default()
                    },
                )),
            }) {
                // 请求没发出去:让共享这份结果的等待者立刻失败,而不是等到超时。
                let _ = in_flight
                    .result
                    .send(Some(LoadOutcome::Failed(error.to_string())));
                return Err(error);
            }
        }

        wait_for_context(&self.handle, &mut receiver).await
    }

    pub(crate) async fn handle_client(&self, message: &pb::ExecClientMessage) -> bool {
        if message.id != 0 {
            return false;
        }
        let Some(pb::exec_client_message::Message::RequestContextResult(result)) =
            message.message.as_ref()
        else {
            return false;
        };
        // 取走槽位:结果已经到手,后面再来的人要重新发起。
        let in_flight = { self.slot.lock().take() };
        let Some(in_flight) = in_flight else {
            tracing::warn!(
                request_id = self.handle.request_id(),
                "Cursor request context result has no waiting load"
            );
            return true;
        };
        use pb::request_context_result::Result as ContextResult;
        let result = match result.result.as_ref() {
            Some(ContextResult::Success(success)) => success
                .request_context
                .clone()
                .ok_or_else(|| "Cursor returned empty request context".to_string()),
            Some(ContextResult::Error(error)) => {
                Err(format!("Cursor request context failed: {}", error.error))
            }
            Some(ContextResult::Rejected(rejected)) => Err(format!(
                "Cursor rejected request context: {}",
                rejected.reason
            )),
            None => Err("Cursor returned no request context result".to_string()),
        };
        let outcome = match result {
            Ok(context) => LoadOutcome::Context(Box::new(context)),
            Err(message) => LoadOutcome::Failed(message),
        };
        let _ = in_flight.result.send(Some(outcome));
        true
    }

    pub(crate) async fn handle_stream_close(&self, id: u32) -> bool {
        id == 0 && self.slot.lock().is_some()
    }

    pub(crate) async fn handle_throw(&self, id: u32, message: String) -> bool {
        let in_flight = if id == 0 {
            self.slot.lock().take()
        } else {
            None
        };
        let Some(in_flight) = in_flight else {
            return false;
        };
        let _ = in_flight.result.send(Some(LoadOutcome::Failed(message)));
        true
    }

    async fn has_missing_part(&self, parts: &pb::RequestContextPartReferences) -> Result<bool> {
        for raw_id in [
            parts.rules_blob_id.as_slice(),
            parts.skills_blob_id.as_slice(),
            parts.subagents_blob_id.as_slice(),
            parts.mcps_blob_id.as_slice(),
        ] {
            if raw_id.is_empty() {
                continue;
            }
            let id = BlobId::from_bytes(raw_id)?;
            if self.store.get_blob(&id).await?.is_none() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn cache_parts(&self, context: &pb::RequestContext) -> Result<()> {
        self.cache_part(&pb::RequestContextRulesPart {
            rules: context.rules.clone(),
            non_file_rules: context.non_file_rules.clone(),
            cloud_rule: context.cloud_rule.clone(),
        })
        .await?;
        self.cache_part(&pb::RequestContextSkillsPart {
            agent_skills: context.agent_skills.clone(),
            skill_options: context.skill_options.clone(),
        })
        .await?;
        self.cache_part(&pb::RequestContextSubagentsPart {
            custom_subagents: context.custom_subagents.clone(),
        })
        .await?;
        self.cache_part(&pb::RequestContextMcpsPart {
            tools: context.tools.clone(),
            mcp_instructions: context.mcp_instructions.clone(),
            mcp_file_system_options: context.mcp_file_system_options.clone(),
            mcp_meta_tool_options: context.mcp_meta_tool_options.clone(),
        })
        .await
    }

    async fn cache_part<T: Message>(&self, part: &T) -> Result<()> {
        let data = part.encode_to_vec();
        self.store.put_blob(&data, &[]).await?;
        Ok(())
    }
}

/// 等这一次加载的结局。
///
/// 每个等待者都看一眼当前值再等变化：加入一份"刚好已经有结果"的在飞请求时，
/// `changed()` 不会再触发（`watch` 的订阅者把这之前的值记为已读）。
async fn wait_for_context(
    handle: &TransportHandle,
    receiver: &mut watch::Receiver<Option<LoadOutcome>>,
) -> Result<pb::RequestContext> {
    let cancellation = handle.disconnect_token();
    let timeout = tokio::time::sleep(CONTEXT_TIMEOUT);
    tokio::pin!(timeout);
    loop {
        if let Some(outcome) = receiver.borrow_and_update().clone() {
            return match outcome {
                LoadOutcome::Context(context) => Ok(*context),
                LoadOutcome::Failed(message) => Err(Error::Protocol(message)),
            };
        }
        tokio::select! {
            changed = receiver.changed() => {
                if changed.is_err() {
                    return Err(Error::Protocol("request context response channel closed".into()));
                }
            }
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            _ = &mut timeout => return Err(Error::Protocol("request context timed out.".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;

    use crate::cursor::{services::observability::CursorTraceService, transport::OutputHub};

    use super::*;

    struct Fixture {
        sync: RequestContextSynchronizer,
        frames: mpsc::Receiver<bytes::Bytes>,
        _directory: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let (commands, _commands) = mpsc::channel(4);
        let output = Arc::new(OutputHub::default());
        let trace = CursorTraceService::new(store.clone()).recorder("request");
        let handle = TransportHandle::new("request".into(), commands, output, trace);
        let frames = handle.subscribe();
        let sync = RequestContextSynchronizer::new(handle, store);
        Fixture {
            sync,
            frames,
            _directory: directory,
        }
    }

    fn context(rules: usize) -> pb::RequestContext {
        pb::RequestContext {
            rules: (0..rules)
                .map(|index| pb::CursorRule {
                    content: format!("rule {index}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn client_result(context: pb::RequestContext) -> pb::ExecClientMessage {
        pb::ExecClientMessage {
            id: 0,
            message: Some(pb::exec_client_message::Message::RequestContextResult(
                pb::RequestContextResult {
                    result: Some(pb::request_context_result::Result::Success(
                        pb::RequestContextSuccess {
                            request_context: Some(context),
                            ..Default::default()
                        },
                    )),
                },
            )),
            ..Default::default()
        }
    }

    fn load(fixture: &Fixture) -> tokio::task::JoinHandle<Result<pb::RequestContext>> {
        let sync = fixture.sync.clone();
        tokio::spawn(async move { sync.load("conversation").await })
    }

    async fn next_frame(fixture: &mut Fixture) -> bytes::Bytes {
        tokio::time::timeout(Duration::from_secs(5), fixture.frames.recv())
            .await
            .expect("这次 load 必须发出请求（不能挂在等槽位上）")
            .expect("输出帧通道不能关闭")
    }

    /// 等第 N 个参与者真的加入在飞请求。测试要观察"共享"这件事，只能等它发生。
    async fn wait_for_waiters(fixture: &Fixture, waiters: usize) {
        for _ in 0..200 {
            let joined = fixture
                .sync
                .slot
                .lock()
                .as_ref()
                .is_some_and(|in_flight| in_flight.waiters.load(Ordering::Acquire) == waiters);
            if joined {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("没有等到 {waiters} 个等待者");
    }

    /// supersede 会丢掉在飞的那一代 `prepare`（连同它的 `load`）：槽位必须跟着释放，
    /// 否则下一个需要上下文的 run 直接失败。以前清理只发生在 `emit` 失败、
    /// select 返回错误这两条路径上，`Drop` 不清理。
    #[tokio::test]
    async fn a_dropped_load_releases_the_slot_for_the_next_run() {
        let mut fixture = fixture().await;
        let superseded = load(&fixture);
        next_frame(&mut fixture).await;

        superseded.abort();
        assert!(superseded.await.unwrap_err().is_cancelled());

        // 新一代的 load：必须能重新发起请求，而不是拿到 "already being loaded"。
        let restarted = load(&fixture);
        next_frame(&mut fixture).await;
        fixture.sync.handle_client(&client_result(context(1))).await;

        let context = tokio::time::timeout(Duration::from_secs(5), restarted)
            .await
            .expect("新一代的 load 不能挂着等一份没人消费的结果")
            .unwrap()
            .expect("supersede 之后必须能重新加载");
        assert_eq!(context.rules.len(), 1);
    }

    /// 并发调用共享同一份在飞请求：Cursor 只认 `id == 0`，不可能同时有两份在飞，
    /// 结果也只会回送一次。
    #[tokio::test]
    async fn concurrent_loads_share_one_in_flight_request() {
        let mut fixture = fixture().await;
        let first = load(&fixture);
        next_frame(&mut fixture).await;

        let second = load(&fixture);
        wait_for_waiters(&fixture, 2).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), fixture.frames.recv())
                .await
                .is_err(),
            "第二份 load 必须共享在飞请求，而不是再发一条 RequestContextArgs"
        );

        fixture.sync.handle_client(&client_result(context(2))).await;

        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.rules.len(), 2);
    }

    /// 客户端报错时等待者拿到原来的协议错误文本（以前是 oneshot 里的 `Error::Protocol`）。
    #[tokio::test]
    async fn a_client_side_error_reaches_every_waiter() {
        let mut fixture = fixture().await;
        let first = load(&fixture);
        next_frame(&mut fixture).await;
        let second = load(&fixture);
        wait_for_waiters(&fixture, 2).await;

        fixture
            .sync
            .handle_client(&pb::ExecClientMessage {
                id: 0,
                message: Some(pb::exec_client_message::Message::RequestContextResult(
                    pb::RequestContextResult {
                        result: Some(pb::request_context_result::Result::Rejected(
                            pb::RequestContextRejected {
                                reason: "no notes".into(),
                            },
                        )),
                    },
                )),
                ..Default::default()
            })
            .await;

        for result in [first.await.unwrap(), second.await.unwrap()] {
            let Err(Error::Protocol(message)) = result else {
                panic!("客户端拒绝必须变成协议错误");
            };
            assert_eq!(message, "Cursor rejected request context: no notes");
        }
    }
}
