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
    // 工具结果由客户端回传，链路卡住时这里会静默等到 30 分钟超时。每 30 秒留一条
    // "还在等谁"的日志：出问题时能立刻从应用日志看出是哪个工具没有回报，而不是只能
    // 等超时后再猜。
    let mut wait_notice = tokio::time::interval(Duration::from_secs(30));
    wait_notice.tick().await;
    let timed_out = loop {
        let command = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(RunOutcome::Cancelled),
            _ = tokio::time::sleep_until(deadline) => break true,
            _ = wait_notice.tick() => {
                let outstanding = calls
                    .iter()
                    .filter(|call| !completed_call_ids.contains(&call.call_id))
                    .map(|call| call.name.as_str())
                    .collect::<Vec<_>>();
                tracing::warn!(
                    round_id = %round_id,
                    outstanding = ?outstanding,
                    "tool round is still waiting for tool results"
                );
                continue;
            }
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
                // 更早轮次已经收过尾的调用（30 分钟超时补的 interrupted，或前一轮已提交）：
                // 客户端补发/重复上报时不能再提交。store 只认本轮调用，遇到这种 call_id
                // 会报 `unknown tool result call_id`，工具轮失败不经过重试，整轮 run 会被判死；
                // 模型轮里的同类迟到结果本来就是忽略的，这里保持一致。
                //
                // 但 call_id 只在轮内有效：客户端会跨轮复用同一个 id（见 interrupt 用例
                // `a_later_round_may_reuse_an_interrupted_call_id`）。属于本轮 `calls` 的
                // 结果必须接收，否则这一轮永远等不到结果。
                let belongs_to_round = calls.iter().any(|call| call.call_id == call_id);
                if !belongs_to_round && already_accounted(&call_id, accounted) {
                    tracing::debug!(
                        round_id = %round_id,
                        call_id,
                        "ignoring a tool result the run already accounted for"
                    );
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
                            settled: committed.settled,
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
                    settled: committed.settled,
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

/// 这个结果是否属于**更早轮次**：`accounted` 在轮次之间共享，本轮正常提交的结果也会
/// 进集合（`completed_call_ids` 已先挡住同轮重复），所以这里命中的只可能是迟到结果。
fn already_accounted(call_id: &str, accounted: &HashSet<String>) -> bool {
    accounted.contains(call_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归(C-03)：上一轮（含 30 分钟超时补成的 interrupted）已收尾的 call_id 不能
    /// 再提交给本轮——store 会报未知 call_id 并把整轮判死。
    #[test]
    fn results_already_accounted_by_an_earlier_round_are_ignored() {
        let mut accounted = HashSet::new();
        accounted.insert("call-1".to_owned());
        assert!(already_accounted("call-1", &accounted));
        assert!(!already_accounted("call-2", &accounted));
        assert!(!already_accounted("call-1", &HashSet::new()));
    }
}
