//! Commits runtime messages once and acknowledges their delivery result.

use tokio_util::sync::CancellationToken;

use crate::{
    model::{CheckpointId, PreparedRun},
    store::Store,
};

use super::{
    engine::{client_failure, emit, wait_for_state_ready},
    CommandResult, CommitBarrier, CommitCause, MessageBatch, MessagesCommitted, RunCommand,
    RunEvent, RunFailure, RunOutcome, RunPort,
};

pub(super) async fn append_batches(
    store: &Store,
    prepared: &PreparedRun,
    client: &mut RunPort,
    cancellation: &CancellationToken,
    mut checkpoint: CheckpointId,
    batches: Vec<MessageBatch>,
) -> Result<(CheckpointId, bool), RunOutcome> {
    let mut inserted_any = false;
    for batch in batches {
        // 身份先全部校验：整批走一个事务，混着一条没有事件身份的消息会让已经收下的
        // 那几条一起回滚，而这类消息是客户端协议错误，根本不该走到库里。
        for message in &batch.messages {
            if message.runtime_event_id.is_none() {
                return Err(RunOutcome::Failed(RunFailure::Protocol(
                    "runtime message has no event identity".into(),
                )));
            }
        }
        // 一次提交整批：store 侧只装载一次链、只占一次写锁、只开一个事务（审计 09/F2）。
        let appended = store
            .append_messages_once(
                &prepared.conversation_id,
                &prepared.run_id,
                checkpoint,
                &batch.messages,
            )
            .await
            .map_err(|error| RunOutcome::Failed(error.into()))?;
        let last_inserted = appended.iter().rposition(|(_, inserted)| *inserted);
        let mut batch_inserted = false;
        for (index, (next, inserted)) in appended.into_iter().enumerate() {
            checkpoint = next;
            if !inserted {
                continue;
            }
            batch_inserted = true;
            let event_id = batch.messages[index]
                .runtime_event_id
                .clone()
                .expect("runtime message identity was validated above");
            let (barrier, ready) = CommitBarrier::before_continue();
            emit(
                client,
                RunEvent::MessagesCommitted(MessagesCommitted {
                    checkpoint_id: checkpoint,
                    tool_round_version: 0,
                    cause: CommitCause::RuntimeEvent { event_id },
                    barrier,
                }),
            )
            .await
            .map_err(|_| client_failure())?;
            // 每条消息仍然各发一个提交事件（协议形态不变），但只对批内最后一条等屏障：
            // 事件按顺序送达客户端，等到最后一条就等于前面几条都已经落地；逐条等会把
            // 一轮的推进成本按注入条数放大（审计 09/F2：窗口从"每条"放大到"每批"）。
            if Some(index) == last_inserted {
                wait_for_state_ready(ready, cancellation).await?;
            }
        }
        inserted_any |= batch_inserted;
        let _ = batch.result.send(if batch_inserted {
            CommandResult::Applied
        } else {
            CommandResult::Duplicate
        });
    }
    Ok((checkpoint, inserted_any))
}

pub(super) fn drain_accepted(client: &mut RunPort) -> Result<Vec<MessageBatch>, RunOutcome> {
    let mut messages = Vec::new();
    loop {
        match client.commands.try_recv() {
            Ok(RunCommand::InsertMessages(batch) | RunCommand::BreakMessages(batch)) => {
                messages.push(batch);
            }
            Ok(RunCommand::Cancel) => return Err(RunOutcome::Cancelled),
            Ok(RunCommand::ToolResult(_)) => {}
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Ok(messages),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                return Err(client_failure())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::oneshot;

    use crate::{
        model::{
            CanonicalMessage, ModelSpec, Origin, PreparedRun, PromptSpec, Role, RunAction, RunKind,
        },
        store::Store,
    };

    use super::*;

    async fn fixture() -> (tempfile::TempDir, Store, PreparedRun, CheckpointId) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let conversation_id = crate::model::ConversationId::new("batch-commits");
        let run_id = crate::model::RunId::new("batch-run");
        let root = store.ensure_conversation(&conversation_id).await.unwrap();
        let prepared = PreparedRun {
            run_id: run_id.clone(),
            cursor_request_id: None,
            conversation_id,
            kind: RunKind::Root,
            model: ModelSpec::new("model"),
            prompt: PromptSpec {
                instructions: String::new(),
                tools: Vec::new(),
            },
            initial_messages: Vec::new(),
            action: RunAction::Start,
            base_checkpoint_id: root,
        };
        let claimed = store.claim_run(&prepared).await.unwrap();
        (directory, store, prepared, claimed.head_checkpoint_id)
    }

    fn runtime(id: &str) -> CanonicalMessage {
        let mut message = CanonicalMessage::text(id, Role::User, Origin::User, "injected");
        message.runtime_event_id = Some(format!("event:{id}"));
        message
    }

    fn batch(messages: Vec<CanonicalMessage>) -> MessageBatch {
        MessageBatch {
            event_id: "batch".into(),
            messages,
            result: oneshot::channel().0,
        }
    }

    async fn next_event(session: &mut crate::run::RunSession) -> RunEvent {
        tokio::time::timeout(Duration::from_secs(5), session.events.recv())
            .await
            .expect("一批消息必须把提交事件都发出来,不能等中间那条的屏障")
            .expect("event channel closed")
    }

    /// 一批运行时消息：一次提交、每条各发一个提交事件、**只等批内最后一条**的屏障。
    ///
    /// 这里故意不去完成前两条的屏障：逐条等待的实现会在第一条之后就卡住，第 2、3 条事件
    /// 永远发不出来（用例在 5s 超时里变红）。审计 09/F2 的口径是"协议形态不动，窗口从每条
    /// 放大到每批"。
    #[tokio::test]
    async fn a_runtime_batch_commits_every_message_but_waits_for_the_last_barrier() {
        let (_directory, store, prepared, checkpoint) = fixture().await;
        let (mut client, mut session, _handle) =
            crate::run::port::channel(crate::model::RunId::new("batch-run"), 16);

        let messages = vec![
            runtime("runtime:a"),
            runtime("runtime:b"),
            runtime("runtime:c"),
        ];
        let cancellation = CancellationToken::new();
        let mut checkpoints = Vec::new();
        let mut early_barriers = Vec::new();
        // 提交和收事件必须并发：函数会在批尾的屏障上等，而那个屏障就在事件里。
        let (result, ()) = tokio::join!(
            append_batches(
                &store,
                &prepared,
                &mut client,
                &cancellation,
                checkpoint,
                vec![batch(messages)],
            ),
            async {
                for index in 0..3 {
                    let RunEvent::MessagesCommitted(committed) = next_event(&mut session).await
                    else {
                        panic!("提交必须发 MessagesCommitted");
                    };
                    match &committed.cause {
                        CommitCause::RuntimeEvent { event_id } => {
                            assert!(event_id.starts_with("event:runtime:"), "{event_id}");
                        }
                        cause => panic!("unexpected commit cause: {cause:?}"),
                    }
                    checkpoints.push(committed.checkpoint_id);
                    if index < 2 {
                        // 前两条的屏障**不完成**：函数必须已经往下走了——第 2、3 条事件
                        // 能到就是证明。逐条等待的实现会在这里超时。
                        early_barriers.push(committed.barrier);
                    } else {
                        committed.barrier.complete(Ok(()));
                    }
                }
            }
        );
        let (head, inserted) = result.unwrap();
        assert!(inserted);
        assert_eq!(
            early_barriers.len(),
            2,
            "批内前两条的屏障在事件发出时都还没完成，函数却继续发了后面的提交"
        );
        assert!(
            checkpoints.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "每条消息各拿一个 checkpoint: {checkpoints:?}"
        );
        assert_eq!(head, *checkpoints.last().unwrap());
        assert_eq!(store.load_checkpoint_messages(head).await.unwrap().len(), 3);
    }

    /// 边界：单条批仍然等它自己的屏障（它既是第一条也是最后一条）。
    #[tokio::test]
    async fn a_single_message_batch_waits_for_its_own_barrier() {
        let (_directory, store, prepared, checkpoint) = fixture().await;
        let (mut client, mut session, _handle) =
            crate::run::port::channel(crate::model::RunId::new("batch-run"), 16);

        let cancellation = CancellationToken::new();
        let mut call = std::pin::pin!(append_batches(
            &store,
            &prepared,
            &mut client,
            &cancellation,
            checkpoint,
            vec![batch(vec![runtime("runtime:only")])],
        ));
        let event = tokio::select! {
            event = next_event(&mut session) => event,
            result = &mut call => panic!("提交在屏障完成前就返回了: {result:?}"),
        };
        let RunEvent::MessagesCommitted(committed) = event else {
            panic!("提交必须发 MessagesCommitted");
        };
        // 事件已经到了：继续推这个 future，它必须停在屏障上（200ms 内不返回）。
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut call)
                .await
                .is_err(),
            "单条批必须等自己的屏障"
        );
        committed.barrier.complete(Ok(()));
        let (head, inserted) = call.await.unwrap();
        assert!(inserted);
        assert_eq!(store.load_checkpoint_messages(head).await.unwrap().len(), 1);
    }

    /// 边界：空批什么都不做——不发事件、不装载、不推进，结果按"没有新消息"回给调用方。
    #[tokio::test]
    async fn an_empty_batch_emits_nothing_and_reports_duplicate() {
        let (_directory, store, prepared, checkpoint) = fixture().await;
        let (mut client, mut session, _handle) =
            crate::run::port::channel(crate::model::RunId::new("batch-run"), 16);

        let (ready, result) = oneshot::channel();
        let batches = vec![MessageBatch {
            event_id: "empty".into(),
            messages: Vec::new(),
            result: ready,
        }];
        let (head, inserted) = append_batches(
            &store,
            &prepared,
            &mut client,
            &CancellationToken::new(),
            checkpoint,
            batches,
        )
        .await
        .unwrap();
        assert_eq!(head, checkpoint, "空批不推进 checkpoint");
        assert!(!inserted);
        assert!(
            session.events.try_recv().is_err(),
            "空批不发提交事件，也不能让任何屏障等空"
        );
        assert!(matches!(result.await, Ok(CommandResult::Duplicate)));
    }

    /// 一批里有一条身份冲突时整批回滚：调用方拿不到返回值就不会发事件，"没提交"与
    /// "没宣布"一致。逐条提交的旧实现会把第一条留库里，这条用例会红。
    #[tokio::test]
    async fn a_conflict_inside_the_batch_rolls_the_whole_batch_back() {
        let (_directory, store, prepared, checkpoint) = fixture().await;
        let (mut client, _session, _handle) =
            crate::run::port::channel(crate::model::RunId::new("batch-run"), 16);

        let conflicting =
            CanonicalMessage::text("runtime:a", Role::User, Origin::User, "different");
        let batches = vec![batch(vec![runtime("runtime:a"), conflicting])];
        let outcome = append_batches(
            &store,
            &prepared,
            &mut client,
            &CancellationToken::new(),
            checkpoint,
            batches,
        )
        .await;
        assert!(outcome.is_err(), "同身份不同内容必须让这一批失败");
        assert!(
            store
                .message(&prepared.conversation_id, "runtime:a")
                .await
                .unwrap()
                .is_none(),
            "整批回滚:前面那条不能留在库里"
        );
    }
}
