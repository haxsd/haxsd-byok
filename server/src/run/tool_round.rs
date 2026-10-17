//! Executes one logical round of Tool calls and results.
use std::collections::HashSet;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::{
    model::{CheckpointId, PreparedRun, ToolCall, ToolResult, ToolRoundAssistant, ToolRoundId},
    store::Store,
};

use super::{
    CommitBarrier, CommitCause, MessageBatch, MessagesCommitted, RunCommand, RunEvent, RunFailure,
    RunOutcome, RunPort,
};

/// 等这一轮每一个工具结果的上限。
///
/// 结果由 Cursor 客户端执行后回报；客户端既不回报、也不失败、也不关流时（例如 exec 的
/// 流被静默收掉——`codec::stream_closed` 的 `Ok(None)` 分支），这里就是唯一的出口：
/// 不设上限的话这一轮会一直停在 `RunPhase::Running`，界面显示"进行中"，也不会自动回收。
/// 取值与父轮等后台子代理的上限一致；合法长工具（阻塞式编译、长脚本）都在窗口内，
/// 超时后按"结果不会再来"补齐 interrupted 结果收轮，而不是把整轮判死。
const TOOL_ROUND_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub(super) struct ToolRound {
    pub id: ToolRoundId,
    pub assistant: ToolRoundAssistant,
    pub calls: Vec<ToolCall>,
    pub recovered_started_at_ms: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    store: &Store,
    prepared: &PreparedRun,
    client: &mut RunPort,
    cancellation: &CancellationToken,
    mut checkpoint: CheckpointId,
    round: ToolRound,
    insertions: Vec<MessageBatch>,
    accounted: &mut HashSet<String>,
) -> std::result::Result<CheckpointId, RunOutcome> {
    let ToolRound {
        id: round_id,
        assistant,
        calls,
        recovered_started_at_ms,
    } = round;
    store
        .create_tool_round(
            &round_id,
            &prepared.run_id,
            checkpoint,
            &assistant,
            &calls,
            recovered_started_at_ms,
        )
        .await
        .map_err(failed)?;
    tracing::info!(
        round_id = %round_id,
        checkpoint_id = checkpoint.0,
        calls = calls.len(),
        "tool round started"
    );
    send(
        client,
        RunEvent::MessagesCommitted(MessagesCommitted {
            checkpoint_id: checkpoint,
            tool_round_version: 0,
            cause: CommitCause::ToolRoundStarted(round_id.clone()),
            barrier: CommitBarrier::None,
        }),
    )
    .await?;
    send(
        client,
        RunEvent::ExecuteToolRound {
            round_id: round_id.clone(),
            calls: calls.clone(),
        },
    )
    .await?;

    let mut remaining = calls.len();
    let mut completed_call_ids = HashSet::new();
    let mut pending_insertions = insertions;
    let mut break_messages = None;
    let deadline = tokio::time::Instant::now() + TOOL_ROUND_TIMEOUT;
    let timed_out = loop {
        let command = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(RunOutcome::Cancelled),
            _ = tokio::time::sleep_until(deadline) => break true,
            command = client.commands.recv() => command,
        };
        let Some(command) = command else {
            return Err(client_failure());
        };
        match command {
            RunCommand::ToolResult(result) => {
                let call_id = result.call_id.clone();
                if completed_call_ids.contains(&call_id) {
                    continue;
                }
                let committed = store
                    .commit_tool_result(
                        &prepared.conversation_id,
                        &prepared.run_id,
                        &round_id,
                        &result,
                    )
                    .await
                    .map_err(failed)?;
                checkpoint = committed.checkpoint_id;
                completed_call_ids.insert(call_id.clone());
                accounted.insert(call_id.clone());
                tracing::info!(
                    round_id = %round_id,
                    call_id,
                    checkpoint_id = checkpoint.0,
                    tool_round_version = committed.tool_round_version,
                    completion_seq = committed.completion_seq,
                    settled = committed.settled,
                    "tool result committed"
                );
                remaining -= 1;
                let (barrier, ready) = if committed.settled {
                    let (barrier, ready) = CommitBarrier::before_continue();
                    (barrier, Some(ready))
                } else {
                    (CommitBarrier::None, None)
                };
                send(
                    client,
                    RunEvent::MessagesCommitted(MessagesCommitted {
                        checkpoint_id: checkpoint,
                        tool_round_version: committed.tool_round_version,
                        cause: CommitCause::ToolResult {
                            call_id,
                            interrupted: false,
                        },
                        barrier,
                    }),
                )
                .await?;
                if let Some(ready) = ready {
                    super::engine::wait_for_state_ready(ready, cancellation).await?;
                }
                if remaining == 0 {
                    break false;
                }
            }
            RunCommand::BreakMessages(messages) => {
                break_messages = Some(messages);
                break false;
            }
            RunCommand::InsertMessages(insertion) => pending_insertions.push(insertion),
            RunCommand::Cancel => return Err(RunOutcome::Cancelled),
        }
    };

    // 还没有回报的调用在这里补一条 interrupted 结果：收到更新的用户消息时按用户意图
    // 打断，等超时时按"结果不会再来"处理（见 `TOOL_ROUND_TIMEOUT`）。两条路都让轮次
    // 收在"每个调用都有结果"上，而不是留在 pending。
    let reason = if timed_out {
        let outstanding = calls
            .iter()
            .filter(|call| !completed_call_ids.contains(&call.call_id))
            .map(|call| call.call_id.as_str())
            .collect::<Vec<_>>();
        tracing::warn!(
            round_id = %round_id,
            outstanding = ?outstanding,
            timeout_secs = TOOL_ROUND_TIMEOUT.as_secs(),
            "tool round timed out waiting for results; completing it with interrupted results"
        );
        "Tool execution did not report a result in time."
    } else {
        "Tool execution was interrupted by a newer user message."
    };
    checkpoint = interrupt_remaining(
        store,
        prepared,
        client,
        cancellation,
        checkpoint,
        &round_id,
        &calls,
        &completed_call_ids,
        accounted,
        reason,
    )
    .await?;
    checkpoint = super::messages::append_batches(
        store,
        prepared,
        client,
        cancellation,
        checkpoint,
        pending_insertions,
    )
    .await?
    .0;
    if let Some(messages) = break_messages {
        checkpoint = super::messages::append_batches(
            store,
            prepared,
            client,
            cancellation,
            checkpoint,
            vec![messages],
        )
        .await?
        .0;
    }
    Ok(checkpoint)
}

/// 给还没有回报的调用各补一条 interrupted 工具结果。
#[allow(clippy::too_many_arguments)]
async fn interrupt_remaining(
    store: &Store,
    prepared: &PreparedRun,
    client: &mut RunPort,
    cancellation: &CancellationToken,
    mut checkpoint: CheckpointId,
    round_id: &ToolRoundId,
    calls: &[ToolCall],
    completed: &HashSet<String>,
    accounted: &mut HashSet<String>,
    reason: &str,
) -> std::result::Result<CheckpointId, RunOutcome> {
    for call in calls
        .iter()
        .filter(|call| !completed.contains(&call.call_id))
    {
        let result = ToolResult {
            call_id: call.call_id.clone(),
            content: reason.into(),
            is_error: true,
            image: None,
        };
        let committed = store
            .commit_tool_result(
                &prepared.conversation_id,
                &prepared.run_id,
                round_id,
                &result,
            )
            .await
            .map_err(failed)?;
        checkpoint = committed.checkpoint_id;
        accounted.insert(call.call_id.clone());
        let (barrier, ready) = if committed.settled {
            let (barrier, ready) = CommitBarrier::before_continue();
            (barrier, Some(ready))
        } else {
            (CommitBarrier::None, None)
        };
        send(
            client,
            RunEvent::MessagesCommitted(MessagesCommitted {
                checkpoint_id: checkpoint,
                tool_round_version: committed.tool_round_version,
                cause: CommitCause::ToolResult {
                    call_id: call.call_id.clone(),
                    interrupted: true,
                },
                barrier,
            }),
        )
        .await?;
        if let Some(ready) = ready {
            super::engine::wait_for_state_ready(ready, cancellation).await?;
        }
    }
    Ok(checkpoint)
}

async fn send(client: &RunPort, event: RunEvent) -> std::result::Result<(), RunOutcome> {
    client
        .events
        .send(event)
        .await
        .map_err(|_| client_failure())
}

fn failed(error: crate::Error) -> RunOutcome {
    RunOutcome::Failed(error.into())
}

fn client_failure() -> RunOutcome {
    RunOutcome::Failed(RunFailure::Client("client event channel closed".into()))
}
