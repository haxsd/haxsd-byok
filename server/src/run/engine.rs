//! Executes one Run across model cycles, Tool rounds, and message commits.
use std::collections::HashSet;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::{
    model::{
        CanonicalMessage, MessageContent, Origin, PreparedRun, Role, RunAction, ToolRoundAssistant,
        ToolRoundId, Usage,
    },
    provider::Provider,
    store::{BlobId, ContextUsageAnchor, RunStatus, Store},
};

use super::{
    consume_model_cycle,
    model_retry::{should_retry, MODEL_RETRY_DELAY},
    CommitBarrier, CommitCause, MessageBatch, MessagesCommitted, ModelCycleFailure, RunCommand,
    RunEvent, RunFailure, RunOutcome, RunPort,
};

pub struct RunEngine {
    store: Store,
    provider: Arc<dyn Provider>,
    images: ImageCache,
}

/// 同一次 Run 内按 `BlobId` 复用图片字节。
///
/// 每个模型轮都会按 checkpoint 重建整条 provider 可见历史，图片内容块却是同一批
/// 字节——内容寻址，写进 `blobs` 之后不再变。没有缓存时，一轮里每张图都要回库读一次
/// （截图类工具结果是 MB 级的），工具轮越多读得越多。缓存随 Run 一起消失，不会跨 run
/// 常驻，也不需要失效逻辑。
#[derive(Default)]
struct ImageCache {
    blobs: parking_lot::Mutex<std::collections::HashMap<BlobId, Arc<Vec<u8>>>>,
}

impl ImageCache {
    async fn data(&self, store: &Store, id: &BlobId) -> crate::Result<Arc<Vec<u8>>> {
        if let Some(data) = self.blobs.lock().get(id).cloned() {
            return Ok(data);
        }
        // 锁不跨 await：读库在锁外，取完再放进去（同一张图并发读到两次也无害，内容是
        // 同一份字节）。
        let data = Arc::new(store.get_blob(id).await?.ok_or_else(|| {
            crate::Error::Protocol(format!("Read image Blob is missing: {}", id.to_base64()))
        })?);
        self.blobs.lock().insert(id.clone(), data.clone());
        Ok(data)
    }

    async fn hydrate(
        &self,
        store: &Store,
        messages: &mut [crate::model::ProjectedMessage],
    ) -> crate::Result<()> {
        use crate::model::{ContentPart, ProjectedContent};

        for message in messages {
            let ProjectedContent::ToolResult(result) = &mut message.content else {
                continue;
            };
            let Some(image) = &result.image else {
                continue;
            };
            let id = BlobId::from_base64(&image.blob_id)?;
            let data = self.data(store, &id).await?;
            result.provider_parts = vec![
                ContentPart::Text {
                    text: result.content.clone(),
                },
                ContentPart::Image {
                    mime_type: image.mime_type.clone(),
                    data: (*data).clone(),
                },
            ];
        }
        Ok(())
    }
}

/// Provider-visible tail appended when the committed history ends with the
/// assistant, which happens when Cursor resumes a turn that had already
/// finished (for example after a cancelled follow-up).
const CONTINUE_MESSAGE_ID: &str = crate::model::CONTINUE_TAIL_MESSAGE_ID;
const CONTINUE_INSTRUCTION: &str = "Continue from where you left off.";

/// 父轮等后台子代理时重查"还有子代理在跑吗"的间隔。
const BACKGROUND_WORK_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// 子运行落成终态之后再宽限这么久，等它那条完成消息投进来再收尾。
const BACKGROUND_COMPLETION_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// 等后台子代理的上限：超时就照常收尾。宁可少一次唤醒，也不能把父轮永久挂住。
const BACKGROUND_WORK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// 父轮收尾前等待后台子代理的结果。
enum BackgroundWait {
    /// 等到了完成消息并已写进历史：模型循环继续。
    Continued,
    /// 没有在跑的子代理了（或超时）：照常收尾。
    Drained,
}

/// 溢出压缩阶梯的计数：**按"当前溢出 episode"**，不按整个 run 累计。
///
/// 阶梯本身保证同一个 episode 内最多压缩两次：第一次保留近期轮（provider 拒绝往往只是
/// 比估算器多算了一点）、第二次只留摘要、第三次才认定当前输入本身放不下而失败。
/// 一次成功的模型调用说明这次溢出已经解决，episode 随之结束——长轮次里几十分钟后的
/// 另一次溢出必须重新从"保留近期轮"开始，否则会因为本 run 早先溢出过就直接丢掉全部
/// 逐字历史（审计 09/F8）。
#[derive(Default)]
struct OverflowEpisode {
    compactions: u8,
}

impl OverflowEpisode {
    /// 这次拒绝该用哪种保留策略；`None` = 不再压缩，直接失败。
    fn retention(&self) -> Option<super::compaction::Retention> {
        match self.compactions {
            0 => Some(super::compaction::Retention::Window),
            1 => Some(super::compaction::Retention::None),
            _ => None,
        }
    }

    /// 模型调用成功：当前 episode 结束，计数回到起点。
    fn model_call_succeeded(&mut self) {
        self.compactions = 0;
    }

    /// 上游拒绝了这次请求：当前 episode 内往下走一级。
    fn provider_refused(&mut self) {
        self.compactions = self.compactions.saturating_add(1);
    }
}

/// `auto_compact` 的输入。
///
/// 这些值来自不同生命周期（准备好的轮次、压缩基准、流端口、取消令牌），打包成结构体只是
/// 为了不让调用点排七个位置参数；就绪后立刻解构成局部变量，函数体不必到处写 `request.`。
struct CompactionRequest<'a> {
    prepared: &'a PreparedRun,
    checkpoint: crate::model::CheckpointId,
    messages: &'a [CanonicalMessage],
    client: &'a mut RunPort,
    cancellation: &'a CancellationToken,
    retention: super::compaction::Retention,
    /// Actual provider tokens per estimated token for the anchored prefix.
    calibration: f64,
    settings: &'a crate::store::CompactionSettings,
    /// Why this compaction ran, for the structured metric log.
    trigger: &'static str,
    /// Pressure estimate that opened the preflight path, when there was one.
    estimated_tokens: Option<u64>,
    accounted_tool_results: &'a HashSet<String>,
}

impl RunEngine {
    pub fn new(store: Store, provider: Arc<dyn Provider>) -> Self {
        Self {
            store,
            provider,
            images: ImageCache::default(),
        }
    }

    #[tracing::instrument(
        skip_all,
        fields(run_id = %prepared.run_id, conversation_id = %prepared.conversation_id)
    )]
    pub async fn run(
        &self,
        prepared: PreparedRun,
        mut client: RunPort,
        cancellation: CancellationToken,
    ) -> RunOutcome {
        let claimed = match self.store.claim_run(&prepared).await {
            Ok(claimed) => claimed,
            Err(error) => {
                let outcome = RunOutcome::Failed(error.into());
                client.phase.finish();
                let _ = client.events.send(RunEvent::Ended(outcome.clone())).await;
                tracing::info!(outcome = ?outcome, "Run claim failed");
                return outcome;
            }
        };
        let outcome = self
            .run_claimed(
                &prepared,
                claimed.head_checkpoint_id,
                &mut client,
                &cancellation,
            )
            .await;
        let usage = outcome.1;
        let outcome = outcome.0;
        let (status, failure) = match &outcome {
            RunOutcome::Completed => (RunStatus::Completed, None),
            RunOutcome::Cancelled => (RunStatus::Cancelled, None),
            RunOutcome::Failed(failure) => (
                RunStatus::Failed,
                Some((failure.category(), failure_message(failure))),
            ),
        };
        let failure_ref = failure
            .as_ref()
            .map(|(category, summary)| (*category, summary.as_str()));
        if let Err(error) = self
            .store
            .finish_run(&prepared.run_id, status, usage, failure_ref)
            .await
        {
            // runs 行停在 `running` 会让 UI 与列表永远显示"进行中"，还会因为
            // `store/retention.rs` 的 running 守卫推迟整个会话的回收。重试一次（同参数），
            // 再失败就写一个最小终态：终态本身比终态的精确内容更重要。
            tracing::warn!(
                run_id = %prepared.run_id,
                conversation_id = %prepared.conversation_id,
                %error,
                "failed to persist the Run outcome; retrying once"
            );
            if let Err(error) = self
                .store
                .finish_run(&prepared.run_id, status, usage, failure_ref)
                .await
            {
                tracing::error!(
                    run_id = %prepared.run_id,
                    conversation_id = %prepared.conversation_id,
                    %error,
                    "retry failed as well; writing a bare failed status"
                );
                if let Err(error) = self
                    .store
                    .finish_run(&prepared.run_id, RunStatus::Failed, None, None)
                    .await
                {
                    tracing::error!(
                        run_id = %prepared.run_id,
                        conversation_id = %prepared.conversation_id,
                        %error,
                        "Run status could not be persisted at all; the row stays running"
                    );
                }
            }
        }
        client.phase.finish();
        let _ = client.events.send(RunEvent::Ended(outcome.clone())).await;
        tracing::info!(outcome = ?outcome, usage = ?usage, "Run ended");
        outcome
    }

    async fn run_claimed(
        &self,
        prepared: &PreparedRun,
        mut checkpoint: crate::model::CheckpointId,
        client: &mut RunPort,
        cancellation: &CancellationToken,
    ) -> (RunOutcome, Option<Usage>) {
        let mut usage = None;
        // 这一轮里已经落库的工具调用（正常回报的，以及轮次超时后被补成 interrupted
        // 的）。客户端可能在那之后才把真正的结果送上来——那条结果不能算协议错误：
        // 它的调用已经收过尾了，再提交一次只会被 store 判成重复。
        let mut accounted_tool_results = HashSet::new();
        let mut context_usage_anchor = match self
            .store
            .latest_context_usage(prepared.conversation_id.as_str())
            .await
        {
            Ok(anchor) => anchor,
            Err(error) => return (RunOutcome::Failed(error.into()), usage),
        };
        let compaction_settings = match self.store.compaction_settings().await {
            Ok(settings) => settings,
            Err(error) => return (RunOutcome::Failed(error.into()), usage),
        };
        // Compaction budgets follow the provider's real limit when an
        // over-limit refusal reveals it, without mutating the user's model
        // configuration. The rest of the run keeps the routed model spec.
        let mut compaction_run = prepared.clone();
        tracing::info!(
            checkpoint_id = checkpoint.0,
            "Run claimed conversation ownership"
        );
        if !prepared.initial_messages.is_empty() {
            let mut changed = false;
            // 这一批要提交的消息：一次提交整批（store 侧只装载一次链、一个事务，审计 09/F2），
            // 所以先收集再一次性写。
            let mut pending: Vec<crate::model::CanonicalMessage> = Vec::new();
            for message in &prepared.initial_messages {
                // 后台完成通知是幂等投递：身份相同就说明这条通知已经送达过（服务端
                // 自己发的，或客户端发来的那一份）。这时不该跑模型——否则同一次子
                // 代理完成会得到两轮 follow-up，而且两份通知正文的差别会被 store 判
                // 成"同一身份内容不同"的冲突，把这一轮直接判死。
                if matches!(prepared.action, RunAction::BackgroundCompletion) {
                    match self
                        .store
                        .message(&prepared.conversation_id, &message.message_id)
                        .await
                    {
                        Ok(Some(_)) => {
                            tracing::info!(
                                conversation_id = %prepared.conversation_id,
                                message_id = %message.message_id,
                                "background completion was already delivered; this turn has nothing to do"
                            );
                            // 收集到这里的消息仍然要提交（与逐条提交时的顺序一致：
                            // 已经收下的那几条在遇到重复身份之前就写进去了）。
                            if !pending.is_empty() {
                                checkpoint = match self
                                    .store
                                    .append_messages_once(
                                        &prepared.conversation_id,
                                        &prepared.run_id,
                                        checkpoint,
                                        &pending,
                                    )
                                    .await
                                {
                                    Ok(appended) => {
                                        appended.into_iter().fold(checkpoint, |_, (next, _)| next)
                                    }
                                    Err(error) => return (RunOutcome::Failed(error.into()), usage),
                                };
                            }
                            // 仍然推一次已收敛的状态：客户端这一轮要看到一个
                            // "没有待办"的终态，否则它会把这一轮当成没结束。
                            //
                            // 这次提交是 `InitialMessages`（settled）而不是 `FinalTurn`：
                            // 这一轮没有新消息，收尾的 `Final` 提交会把"最后一条消息"当成
                            // 刚生成的 assistant（`checkpoint::worker::build_final`），
                            // 于是少算一个根、报 "Cursor stable history shrank"。终态帧
                            // 由 output 的 `Completed` 兜底补上（见
                            // `ConversationOutput::run_inner`）。
                            let (barrier, ready) = CommitBarrier::before_continue();
                            if emit(
                                client,
                                RunEvent::MessagesCommitted(MessagesCommitted {
                                    checkpoint_id: checkpoint,
                                    tool_round_version: 0,
                                    cause: CommitCause::InitialMessages,
                                    barrier,
                                }),
                            )
                            .await
                            .is_err()
                            {
                                return (client_failure(), usage);
                            }
                            if let Err(outcome) = wait_for_state_ready(ready, cancellation).await {
                                return (outcome, usage);
                            }
                            return (RunOutcome::Completed, usage);
                        }
                        Ok(None) => {}
                        Err(error) => return (RunOutcome::Failed(error.into()), usage),
                    }
                }
                pending.push(message.clone());
            }
            // 整批一次提交：过去每条 initial message 都要整链装载 + 一个事务，
            // 一批客户端初始消息的开销按条数放大（审计 09/F2）。
            if !pending.is_empty() {
                match self
                    .store
                    .append_messages_once(
                        &prepared.conversation_id,
                        &prepared.run_id,
                        checkpoint,
                        &pending,
                    )
                    .await
                {
                    Ok(appended) => {
                        for (next, inserted) in appended {
                            checkpoint = next;
                            changed |= inserted;
                        }
                    }
                    Err(error) => return (RunOutcome::Failed(error.into()), usage),
                }
            }
            if changed {
                let (barrier, ready) = CommitBarrier::before_continue();
                if emit(
                    client,
                    RunEvent::MessagesCommitted(MessagesCommitted {
                        checkpoint_id: checkpoint,
                        tool_round_version: 0,
                        cause: CommitCause::InitialMessages,
                        barrier,
                    }),
                )
                .await
                .is_err()
                {
                    return (client_failure(), usage);
                }
                if let Err(outcome) = wait_for_state_ready(ready, cancellation).await {
                    return (outcome, usage);
                }
            }
        }

        if let RunAction::Resume {
            pending_tool_round: Some(round),
        } = &prepared.action
        {
            checkpoint = match super::tool_round::execute(
                &self.store,
                prepared,
                client,
                cancellation,
                checkpoint,
                super::tool_round::ToolRound {
                    id: ToolRoundId::new(format!("{}:round:resume", prepared.run_id)),
                    assistant: round.assistant.clone(),
                    calls: round.calls.clone(),
                    recovered_started_at_ms: Some(round.started_at_ms),
                },
                Vec::new(),
                &mut accounted_tool_results,
            )
            .await
            {
                Ok(checkpoint) => checkpoint,
                Err(outcome) => return (outcome, usage),
            };
        }

        // A provider refusal for an over-limit prompt compacts twice at most:
        // the first compaction keeps the recent turns verbatim, and a second
        // refusal compacts with the summary alone — the replacement 1.0.22
        // sent. That ladder keeps the worst case equal to the old behaviour (a
        // short summary that recovers the conversation) instead of a failed
        // run. A third refusal means the current input itself does not fit, and
        // compacting again would only destroy history.
        let mut overflow = OverflowEpisode::default();
        'model: loop {
            if cancellation.is_cancelled() {
                return (RunOutcome::Cancelled, usage);
            }
            let messages = match self.store.load_checkpoint_messages(checkpoint).await {
                Ok(messages) => messages,
                Err(error) => return (RunOutcome::Failed(error.into()), usage),
            };
            let history = match crate::model::project_messages(&messages) {
                Ok(history) => history,
                Err(error) => return (RunOutcome::Failed(error.into()), usage),
            };
            let compaction_estimate = (prepared.action != RunAction::Compact
                && compaction_settings.auto)
                .then(|| {
                    super::compaction::compaction_estimate(
                        &compaction_run,
                        &history,
                        context_usage_anchor,
                        &compaction_settings,
                    )
                })
                .flatten();
            if let Some(estimate) = compaction_estimate {
                if emit(
                    client,
                    RunEvent::UsageSnapshot(context_usage_snapshot(estimate.tokens)),
                )
                .await
                .is_err()
                {
                    return (client_failure(), usage);
                }
                match self
                    .auto_compact(CompactionRequest {
                        prepared: &compaction_run,
                        checkpoint,
                        messages: &messages,
                        client: &mut *client,
                        cancellation,
                        retention: super::compaction::Retention::Window,
                        calibration: estimate.calibration,
                        settings: &compaction_settings,
                        trigger: "preflight",
                        estimated_tokens: Some(estimate.tokens),
                        accounted_tool_results: &accounted_tool_results,
                    })
                    .await
                {
                    Ok((next_checkpoint, compaction_usage)) => {
                        checkpoint = next_checkpoint;
                        context_usage_anchor = None;
                        if let Some(compaction_usage) = compaction_usage {
                            accumulate_usage(&mut usage, compaction_usage);
                        }
                        continue 'model;
                    }
                    Err(outcome) => return (outcome, usage),
                }
            }
            let provider_call_index = match self.store.begin_provider_call(&prepared.run_id).await {
                Ok(index) => index,
                Err(error) => return (RunOutcome::Failed(error.into()), usage),
            };
            tracing::debug!(
                provider_call_index,
                checkpoint_id = checkpoint.0,
                "starting model call"
            );
            let mut history = history;
            // The manual `/summarize` path trims its replayed history with the
            // same calibrated budget as automatic compaction; keep the factor
            // for the replacement validation below.
            let manual_calibration = if prepared.action == RunAction::Compact {
                super::compaction::estimate_calibration(
                    &compaction_run,
                    &history,
                    context_usage_anchor,
                )
            } else {
                1.0
            };
            if let Err(error) = self.images.hydrate(&self.store, &mut history).await {
                return (RunOutcome::Failed(error.into()), usage);
            }
            // 观测层的上下文用量锚点只数持久化消息：下面追加的尾巴只对 provider 可见、
            // 不进库（`ModelInvocation::persisted_message_count` 按消息身份把它排除）。
            let anchored_messages = history.len();
            let history = if prepared.action == RunAction::Compact {
                // 手动 `/summarize` 与自动压缩用同一套按窗口裁剪的逻辑，否则大历史
                // 会把摘要调用本身撑爆，provider 直接拒绝这一轮。
                super::compaction::compaction_history(
                    history,
                    compaction_run.model.context_window_tokens,
                    manual_calibration,
                    &compaction_settings,
                )
            } else {
                super::history::user_terminated(history, CONTINUE_MESSAGE_ID, CONTINUE_INSTRUCTION)
            };
            let model = if prepared.action == RunAction::Compact {
                self.summary_model_spec(prepared, &compaction_settings)
                    .await
            } else {
                cycle_model(
                    &prepared.action,
                    &prepared.model,
                    super::compaction::OUTPUT_TOKENS,
                )
            };
            let request = crate::model::ModelRequest {
                prompt: prepared.prompt.clone(),
                // 手动 `/summarize`（Compact）与自动压缩产出的是同一种东西——一段之后顶替
                // 历史、对模型可见的摘要——所以两者用同一份摘要规格，不能走调用方原样的模型
                // 规格（那会把思考打开并且不设输出上限）。
                model,
                history,
            };
            let mut retries = 0_u32;
            let mut pending_insertions = Vec::new();
            let cycle = 'attempt: loop {
                let call_id = if retries == 0 {
                    format!("{}:{provider_call_index}", prepared.run_id)
                } else {
                    format!("{}:{provider_call_index}:retry-{retries}", prepared.run_id)
                };
                let invocation = crate::model::ModelInvocation {
                    client_source: crate::model::CallClient::Cursor,
                    call_id,
                    run_id: prepared.run_id.to_string(),
                    conversation_id: prepared.conversation_id.to_string(),
                    provider_call_index,
                    request: request.clone(),
                };
                let cycle_cancellation = cancellation.child_token();
                let cycle_events = client.events.clone();
                let cycle = consume_model_cycle(
                    self.provider.stream(invocation, cycle_cancellation.clone()),
                    &cycle_events,
                    &cycle_cancellation,
                );
                tokio::pin!(cycle);
                let cycle = loop {
                    tokio::select! {
                        biased;
                        command = client.commands.recv() => {
                            let interruption = match command {
                                Some(RunCommand::InsertMessages(insertion)) => {
                                    pending_insertions.push(insertion);
                                    continue;
                                }
                                Some(RunCommand::BreakMessages(messages)) => messages,
                                Some(RunCommand::Cancel) => {
                                    cycle_cancellation.cancel();
                                    let _ = cycle.await;
                                    let _ = emit(client, RunEvent::CycleInterrupted).await;
                                    return (RunOutcome::Cancelled, usage);
                                }
                                Some(RunCommand::ToolResult(result)) => {
                                    if late_tool_result(&result, &accounted_tool_results) {
                                        continue;
                                    }
                                    cycle_cancellation.cancel();
                                    let _ = cycle.await;
                                    let _ = emit(client, RunEvent::CycleInterrupted).await;
                                    return (
                                        RunOutcome::Failed(RunFailure::Protocol(
                                            "received a tool result while the model was running".into(),
                                        )),
                                        usage,
                                    );
                                }
                                None => {
                                    cycle_cancellation.cancel();
                                    let _ = cycle.await;
                                    let _ = emit(client, RunEvent::CycleInterrupted).await;
                                    return (client_failure(), usage);
                                }
                            };
                            cycle_cancellation.cancel();
                            let interrupted = cycle.await;
                            match interrupted {
                                Ok(cycle) => {
                                    if let Some(cycle_usage) = cycle.usage {
                                        update_context_usage_anchor(
                                            &mut context_usage_anchor,
                                            cycle_usage,
                                            anchored_messages,
                                        );
                                        accumulate_usage(&mut usage, cycle_usage);
                                    }
                                }
                                Err(failure) => {
                                    if let Some(cycle_usage) = failure.usage {
                                        update_context_usage_anchor(
                                            &mut context_usage_anchor,
                                            cycle_usage,
                                            anchored_messages,
                                        );
                                        accumulate_usage(&mut usage, cycle_usage);
                                    }
                                }
                            }
                            if emit(client, RunEvent::CycleInterrupted).await.is_err() {
                                return (client_failure(), usage);
                            }
                            checkpoint = match super::messages::append_batches(
                                &self.store,
                                prepared,
                                client,
                                cancellation,
                                checkpoint,
                                std::mem::take(&mut pending_insertions),
                            )
                            .await
                            {
                                Ok((checkpoint, _)) => checkpoint,
                                Err(outcome) => return (outcome, usage),
                            };
                            checkpoint = match super::messages::append_batches(
                                &self.store,
                                prepared,
                                client,
                                cancellation,
                                checkpoint,
                                vec![interruption],
                            )
                            .await
                            {
                                Ok((checkpoint, _)) => checkpoint,
                                Err(outcome) => return (outcome, usage),
                            };
                            continue 'model;
                        },
                        result = &mut cycle => break result,
                    }
                };
                match cycle {
                    Ok(cycle) => {
                        // 这一轮模型调用成了：当前溢出 episode 到此结束，阶梯计数回到
                        // 起点（`OverflowEpisode::model_call_succeeded`）。
                        overflow.model_call_succeeded();
                        break 'attempt cycle;
                    }
                    Err(cycle_failure) => {
                        if let Some(cycle_usage) = cycle_failure.usage {
                            update_context_usage_anchor(
                                &mut context_usage_anchor,
                                cycle_usage,
                                anchored_messages,
                            );
                            accumulate_usage(&mut usage, cycle_usage);
                        }
                        if cancellation.is_cancelled() {
                            let _ = emit(client, RunEvent::CycleInterrupted).await;
                            return (RunOutcome::Cancelled, usage);
                        }
                        // The estimate that cleared the compaction check can
                        // still land over the real limit: it trails the
                        // provider's own count by whatever the request adds
                        // after the anchor was taken, and CJK-heavy content is
                        // counted far lower by the estimator than by the
                        // provider. The provider is the authority, so treat its
                        // refusal as the trigger the estimate missed. Without
                        // this a conversation that crosses the line is wedged:
                        // every retry rebuilds the same prompt and gets the
                        // same refusal.
                        let window_exhausted = exhausted_context_window(
                            &cycle_failure.failure,
                            cycle_failure.usage.as_ref(),
                            super::compaction::input_budget(&compaction_run, &compaction_settings),
                        );
                        let overflow_retention = overflow.retention();
                        // A provider error whose reported input usage already
                        // reached the budget is an overflow even when its prose
                        // matches none of the known phrases: some gateways wrap
                        // or translate the upstream message.
                        let provider_refusal = matches!(
                            &cycle_failure.failure,
                            RunFailure::Provider(message)
                                if super::compaction::is_context_overflow(message)
                        );
                        // The provider is the authority on the real window; a
                        // refused prompt often carries it as prose. Learning it
                        // for this run keeps the next budget honest when the
                        // configured window was larger than the provider's.
                        if let RunFailure::Provider(message) = &cycle_failure.failure {
                            if let Some(limit) = super::compaction::provider_context_limit(message)
                            {
                                if compaction_run.model.context_window_tokens != Some(limit) {
                                    tracing::warn!(
                                        previous = ?compaction_run.model.context_window_tokens,
                                        provider_limit = limit,
                                        "provider reported its real context limit; using it for compaction budgets"
                                    );
                                    compaction_run.model.context_window_tokens = Some(limit);
                                }
                            }
                        }
                        let usage_exhausted =
                            matches!(&cycle_failure.failure, RunFailure::Provider(_))
                                && cycle_failure
                                    .usage
                                    .as_ref()
                                    .and_then(|usage| {
                                        usage.context_input_tokens.or(usage.input_tokens)
                                    })
                                    .is_some_and(|tokens| {
                                        super::compaction::input_budget(
                                            &compaction_run,
                                            &compaction_settings,
                                        )
                                        .is_some_and(|budget| tokens >= budget)
                                    });
                        let overflow_refusal = prepared.action != RunAction::Compact
                            && (window_exhausted || provider_refusal || usage_exhausted);
                        if let Some(retention) = overflow_retention.filter(|_| overflow_refusal) {
                            tracing::warn!(
                                provider_call_index,
                                checkpoint_id = checkpoint.0,
                                keep_recent_turns = retention == super::compaction::Retention::Window,
                                window_exhausted,
                                usage_exhausted,
                                "provider rejected the prompt as over-limit; compacting and retrying"
                            );
                            overflow.provider_refused();
                            checkpoint = match super::messages::append_batches(
                                &self.store,
                                prepared,
                                client,
                                cancellation,
                                checkpoint,
                                std::mem::take(&mut pending_insertions),
                            )
                            .await
                            {
                                Ok((checkpoint, _)) => checkpoint,
                                Err(outcome) => return (outcome, usage),
                            };
                            let messages =
                                match self.store.load_checkpoint_messages(checkpoint).await {
                                    Ok(messages) => messages,
                                    Err(error) => return (RunOutcome::Failed(error.into()), usage),
                                };
                            let calibration = match crate::model::project_messages(&messages) {
                                Ok(history) => super::compaction::estimate_calibration(
                                    &compaction_run,
                                    &history,
                                    context_usage_anchor,
                                ),
                                Err(error) => {
                                    return (RunOutcome::Failed(error.into()), usage);
                                }
                            };
                            match self
                                .auto_compact(CompactionRequest {
                                    prepared: &compaction_run,
                                    checkpoint,
                                    messages: &messages,
                                    client: &mut *client,
                                    cancellation,
                                    retention,
                                    calibration,
                                    settings: &compaction_settings,
                                    trigger: "context-overflow",
                                    estimated_tokens: None,
                                    accounted_tool_results: &accounted_tool_results,
                                })
                                .await
                            {
                                Ok((next_checkpoint, compaction_usage)) => {
                                    checkpoint = next_checkpoint;
                                    context_usage_anchor = None;
                                    if let Some(compaction_usage) = compaction_usage {
                                        accumulate_usage(&mut usage, compaction_usage);
                                    }
                                    continue 'model;
                                }
                                Err(outcome) => return (outcome, usage),
                            }
                        }
                        if !should_retry(&cycle_failure, retries) {
                            let message = failure_message(&cycle_failure.failure);
                            tracing::error!(
                                provider_call_index,
                                retries,
                                max_retries = super::model_retry::MAX_MODEL_RETRIES,
                                checkpoint_id = checkpoint.0,
                                %message,
                                "model call failed for good; ending the run"
                            );
                            // 上游彻底失败是用户最容易看到的那类报错，抓一份现场：
                            // 里面有系统代理指纹、Cursor 的代理配置与当时的日志尾部。
                            crate::diagnostics::capture(
                                "model_call_failed",
                                serde_json::json!({
                                    "provider_call_index": provider_call_index,
                                    "attempts": retries + 1,
                                    "retryable": cycle_failure.retryable,
                                    "category": cycle_failure.failure.category(),
                                    "checkpoint_id": checkpoint.0,
                                    "message": message,
                                }),
                            );
                            // 口径（2026-10-16 裁定，按默认口径落地）：`max_tokens` 截断
                            // **不丢文本**。已经流给客户端的半段回答要落成 assistant 消息：
                            // 用户看到的就是它，服务端历史也要与之一致——否则下一轮模型
                            // 看到的是一轮"什么都没发生"，而用户明明看见了半截回答。
                            // 落库之后仍按失败收尾（客户端据此知道这轮被截断了）。
                            if matches!(cycle_failure.failure, RunFailure::Truncated(_))
                                && !cycle_failure.partial_text.trim().is_empty()
                            {
                                if let Err(outcome) = self
                                    .persist_truncated_assistant(
                                        prepared,
                                        checkpoint,
                                        client,
                                        cancellation,
                                        &cycle_failure,
                                        provider_call_index,
                                    )
                                    .await
                                {
                                    return (outcome, usage);
                                }
                            }
                            return (RunOutcome::Failed(cycle_failure.failure), usage);
                        }
                        retries += 1;
                        let message = failure_message(&cycle_failure.failure);
                        tracing::warn!(
                            provider_call_index,
                            retries,
                            max_retries = super::model_retry::MAX_MODEL_RETRIES,
                            delay_ms = MODEL_RETRY_DELAY.as_millis() as u64,
                            %message,
                            checkpoint_id = checkpoint.0,
                            "model attempt failed; retrying from current checkpoint"
                        );
                        if emit(
                            client,
                            RunEvent::ModelAttemptFailed {
                                attempt: retries,
                                message,
                            },
                        )
                        .await
                        .is_err()
                        {
                            return (client_failure(), usage);
                        }

                        let delay = tokio::time::sleep(MODEL_RETRY_DELAY);
                        tokio::pin!(delay);
                        loop {
                            tokio::select! {
                                biased;
                                command = client.commands.recv() => {
                                    let interruption = match command {
                                        Some(RunCommand::InsertMessages(insertion)) => {
                                            pending_insertions.push(insertion);
                                            continue;
                                        }
                                        Some(RunCommand::BreakMessages(messages)) => messages,
                                        Some(RunCommand::Cancel) => {
                                            let _ = emit(client, RunEvent::CycleInterrupted).await;
                                            return (RunOutcome::Cancelled, usage);
                                        }
                                        Some(RunCommand::ToolResult(result)) => {
                                            if late_tool_result(&result, &accounted_tool_results) {
                                                continue;
                                            }
                                            return (
                                                RunOutcome::Failed(RunFailure::Protocol(
                                                    "received a tool result while waiting to retry the model".into(),
                                                )),
                                                usage,
                                            );
                                        }
                                        None => return (client_failure(), usage),
                                    };
                                    if emit(client, RunEvent::CycleInterrupted).await.is_err() {
                                        return (client_failure(), usage);
                                    }
                                    checkpoint = match super::messages::append_batches(
                                        &self.store,
                                        prepared,
                                        client,
                                        cancellation,
                                        checkpoint,
                                        std::mem::take(&mut pending_insertions),
                                    )
                                    .await
                                    {
                                        Ok((checkpoint, _)) => checkpoint,
                                        Err(outcome) => return (outcome, usage),
                                    };
                                    checkpoint = match super::messages::append_batches(
                                        &self.store,
                                        prepared,
                                        client,
                                        cancellation,
                                        checkpoint,
                                        vec![interruption],
                                    )
                                    .await
                                    {
                                        Ok((checkpoint, _)) => checkpoint,
                                        Err(outcome) => return (outcome, usage),
                                    };
                                    continue 'model;
                                }
                                _ = cancellation.cancelled() => {
                                    let _ = emit(client, RunEvent::CycleInterrupted).await;
                                    return (RunOutcome::Cancelled, usage);
                                }
                                _ = &mut delay => break,
                            }
                        }
                    }
                }
            };
            if let Some(cycle_usage) = cycle.usage {
                update_context_usage_anchor(
                    &mut context_usage_anchor,
                    cycle_usage,
                    anchored_messages,
                );
                accumulate_usage(&mut usage, cycle_usage);
            }

            if prepared.action == RunAction::Compact {
                if !cycle.calls.is_empty() {
                    return (
                        RunOutcome::Failed(RunFailure::Protocol(
                            "compaction model returned tool calls".into(),
                        )),
                        usage,
                    );
                }
                let summary = cycle.text.trim().to_string();
                if summary.is_empty() {
                    return (
                        RunOutcome::Failed(RunFailure::Protocol(
                            "compaction model returned an empty summary".into(),
                        )),
                        usage,
                    );
                }
                let event_id = format!("summary:{}", prepared.run_id);
                let summary = super::compaction::with_context_anchors(&summary, &messages);
                let archive = crate::model::archive_compaction_history(
                    &prepared.run_id.to_string(),
                    &messages,
                );
                let summary = super::compaction::with_archive_anchor(&summary, archive.as_deref());
                let summary_message = super::compaction::summary_message(&event_id, &summary);
                let summary_message_id = summary_message.message_id.clone();
                let replacement = match super::compaction::replacement(
                    &compaction_run,
                    &messages,
                    summary_message,
                    super::compaction::Retention::Window,
                    manual_calibration,
                    &compaction_settings,
                ) {
                    Ok(replacement) => replacement,
                    Err(error) => return (RunOutcome::Failed(error.into()), usage),
                };
                checkpoint = match self
                    .store
                    .replace_checkpoint(
                        &prepared.conversation_id,
                        &prepared.run_id,
                        checkpoint,
                        &replacement,
                    )
                    .await
                {
                    Ok(checkpoint) => checkpoint,
                    Err(error) => return (RunOutcome::Failed(error.into()), usage),
                };
                if !pending_insertions.is_empty() {
                    checkpoint = match super::messages::append_batches(
                        &self.store,
                        prepared,
                        client,
                        cancellation,
                        checkpoint,
                        pending_insertions,
                    )
                    .await
                    {
                        Ok((next, _)) => next,
                        Err(outcome) => return (outcome, usage),
                    };
                }
                client.phase.begin_finalizing();
                let closing_insertions = match super::messages::drain_accepted(client) {
                    Ok(insertions) => insertions,
                    Err(outcome) => return (outcome, usage),
                };
                if !closing_insertions.is_empty() {
                    checkpoint = match super::messages::append_batches(
                        &self.store,
                        prepared,
                        client,
                        cancellation,
                        checkpoint,
                        closing_insertions,
                    )
                    .await
                    {
                        Ok((next, _)) => next,
                        Err(outcome) => return (outcome, usage),
                    };
                }
                let (barrier, ready) = CommitBarrier::before_continue();
                if emit(
                    client,
                    RunEvent::MessagesCommitted(MessagesCommitted {
                        checkpoint_id: checkpoint,
                        tool_round_version: 0,
                        cause: CommitCause::Compaction {
                            summary,
                            summary_message_id,
                        },
                        barrier,
                    }),
                )
                .await
                .is_err()
                {
                    return (client_failure(), usage);
                }
                if let Err(outcome) = wait_for_state_ready(ready, cancellation).await {
                    return (outcome, usage);
                }
                return (RunOutcome::Completed, usage);
            }

            if cycle.calls.is_empty() {
                let assistant = CanonicalMessage {
                    message_id: format!("{}:assistant:{provider_call_index}", prepared.run_id),
                    role: Role::Assistant,
                    origin: Origin::Assistant,
                    content: MessageContent::Assistant {
                        text: cycle.text,
                        thinking: cycle.reasoning,
                        tool_round_id: None,
                        replay_state: cycle.replay_state,
                        tool_calls: Vec::new(),
                    },
                    runtime_event_id: None,
                };
                checkpoint = match self
                    .store
                    .append_checkpoint(
                        &prepared.conversation_id,
                        &prepared.run_id,
                        checkpoint,
                        &[assistant],
                    )
                    .await
                {
                    Ok(checkpoint) => checkpoint,
                    Err(error) => return (RunOutcome::Failed(error.into()), usage),
                };
                if !pending_insertions.is_empty() {
                    let inserted = match super::messages::append_batches(
                        &self.store,
                        prepared,
                        client,
                        cancellation,
                        checkpoint,
                        pending_insertions,
                    )
                    .await
                    {
                        Ok((next, inserted)) => {
                            checkpoint = next;
                            inserted
                        }
                        Err(outcome) => return (outcome, usage),
                    };
                    if inserted {
                        continue 'model;
                    }
                }
                // 还有后台子代理在跑：这一轮先不收尾。父轮挂在这里等它们的完成消息
                // 被投递进来（InsertMessages），写进历史后回到模型循环继续干活。
                if self.has_background_work(prepared).await {
                    let mut waiting = Vec::new();
                    match self
                        .await_background_subagents(
                            prepared,
                            client,
                            cancellation,
                            &mut checkpoint,
                            &mut waiting,
                        )
                        .await
                    {
                        Ok(BackgroundWait::Continued) => continue 'model,
                        Ok(BackgroundWait::Drained) => {}
                        Err(outcome) => return (outcome, usage),
                    }
                }
                client.phase.begin_finalizing();
                let closing_insertions = match super::messages::drain_accepted(client) {
                    Ok(insertions) => insertions,
                    Err(outcome) => return (outcome, usage),
                };
                if !closing_insertions.is_empty() {
                    checkpoint = match super::messages::append_batches(
                        &self.store,
                        prepared,
                        client,
                        cancellation,
                        checkpoint,
                        closing_insertions,
                    )
                    .await
                    {
                        Ok((next, _)) => next,
                        Err(outcome) => return (outcome, usage),
                    };
                    client.phase.resume_running();
                    continue 'model;
                }
                let (barrier, ready) = CommitBarrier::before_continue();
                if emit(
                    client,
                    RunEvent::MessagesCommitted(MessagesCommitted {
                        checkpoint_id: checkpoint,
                        tool_round_version: 0,
                        cause: CommitCause::FinalTurn,
                        barrier,
                    }),
                )
                .await
                .is_err()
                {
                    return (client_failure(), usage);
                }
                if let Err(outcome) = wait_for_state_ready(ready, cancellation).await {
                    return (outcome, usage);
                }
                return (RunOutcome::Completed, usage);
            }

            let round_id =
                ToolRoundId::new(format!("{}:round:{provider_call_index}", prepared.run_id));
            checkpoint = match super::tool_round::execute(
                &self.store,
                prepared,
                client,
                cancellation,
                checkpoint,
                super::tool_round::ToolRound {
                    id: round_id,
                    assistant: ToolRoundAssistant {
                        text: cycle.text,
                        thinking: cycle.reasoning,
                        model_call_id: cycle.model_call_id,
                        replay_state: cycle.replay_state,
                    },
                    calls: cycle.calls,
                    recovered_started_at_ms: None,
                },
                pending_insertions,
                &mut accounted_tool_results,
            )
            .await
            {
                Ok(checkpoint) => checkpoint,
                Err(outcome) => return (outcome, usage),
            };
        }
    }

    /// 这个 run 还有在跑的后台子代理吗。
    ///
    /// 查库失败按"没有"处理：宁可早收尾（完成通知会排进父会话的 pending，用户下次
    /// 开口时看到），也不能因为一次查询失败把父轮挂到 `BACKGROUND_WORK_TIMEOUT`。
    /// 但失败必须留痕——这条路径过去连日志都没有，收尾早了完全看不出为什么。
    async fn has_background_work(&self, prepared: &PreparedRun) -> bool {
        match self
            .store
            .has_running_background_subagents(&prepared.run_id)
            .await
        {
            Ok(running) => running,
            Err(error) => {
                tracing::warn!(
                    run_id = %prepared.run_id,
                    conversation_id = %prepared.conversation_id,
                    %error,
                    "failed to check for running background subagents; finishing this turn now"
                );
                false
            }
        }
    }

    /// 把被 `max_tokens` 截断的半段回答落成 assistant 消息，并推一次终态。
    ///
    /// 口径（按默认口径落地）：截断不丢文本。已经流给客户端的半段回答要进服务端历史，
    /// 下一轮模型看到的就是用户看到的那一段；提交用 `FinalTurn`——这条消息正是刚生成的
    /// assistant（`checkpoint::worker::build_final` 把最后一条当新消息），与正常收尾同形。
    /// 提交完成后调用方仍按 `Failed(Truncated)` 收尾：客户端要能看出这一轮是被截断的，
    /// 而不是正常结束。
    async fn persist_truncated_assistant(
        &self,
        prepared: &PreparedRun,
        checkpoint: crate::model::CheckpointId,
        client: &mut RunPort,
        cancellation: &CancellationToken,
        cycle_failure: &ModelCycleFailure,
        provider_call_index: u64,
    ) -> Result<(), RunOutcome> {
        let message = CanonicalMessage {
            // 与正常收尾同一条身份规则：同一次 provider 调用只可能产出一条 assistant。
            message_id: format!("{}:assistant:{provider_call_index}", prepared.run_id),
            role: Role::Assistant,
            origin: Origin::Assistant,
            content: MessageContent::Assistant {
                text: cycle_failure.partial_text.clone(),
                thinking: cycle_failure.partial_reasoning.clone(),
                tool_round_id: None,
                replay_state: cycle_failure.replay_state.clone(),
                tool_calls: Vec::new(),
            },
            runtime_event_id: None,
        };
        let next = self
            .store
            .append_checkpoint(
                &prepared.conversation_id,
                &prepared.run_id,
                checkpoint,
                &[message],
            )
            .await
            .map_err(|error| RunOutcome::Failed(error.into()))?;
        tracing::warn!(
            run_id = %prepared.run_id,
            conversation_id = %prepared.conversation_id,
            provider_call_index,
            characters = cycle_failure.partial_text.chars().count(),
            "persisted the truncated assistant message before ending the run"
        );
        let (barrier, ready) = CommitBarrier::before_continue();
        emit(
            client,
            RunEvent::MessagesCommitted(MessagesCommitted {
                checkpoint_id: next,
                tool_round_version: 0,
                cause: CommitCause::FinalTurn,
                barrier,
            }),
        )
        .await
        .map_err(|_| client_failure())?;
        wait_for_state_ready(ready, cancellation).await
    }

    /// 收尾前等后台子代理：父轮停在这里，等它们的完成消息被投递进来。
    ///
    /// 消息由上层（子代理运行结束时的唤醒）通过 `InsertMessages` 投进这条运行；
    /// 写进历史后回到模型循环，父代就带着子代理的结果继续工作。等不到也有两条出路：
    /// 子运行都结束了、或等超时——都按"没有后台活"收尾，最坏情况等于没有这个特性。
    async fn await_background_subagents(
        &self,
        prepared: &PreparedRun,
        client: &mut RunPort,
        cancellation: &CancellationToken,
        checkpoint: &mut crate::model::CheckpointId,
        pending: &mut Vec<MessageBatch>,
    ) -> Result<BackgroundWait, RunOutcome> {
        let deadline = tokio::time::Instant::now() + BACKGROUND_WORK_TIMEOUT;
        let mut drained_since: Option<tokio::time::Instant> = None;
        let mut announced = false;
        loop {
            if !pending.is_empty() {
                let batches = std::mem::take(pending);
                let (next, inserted) = super::messages::append_batches(
                    &self.store,
                    prepared,
                    client,
                    cancellation,
                    *checkpoint,
                    batches,
                )
                .await?;
                *checkpoint = next;
                if inserted {
                    return Ok(BackgroundWait::Continued);
                }
            }
            match self
                .store
                .has_running_background_subagents(&prepared.run_id)
                .await
            {
                Ok(true) => drained_since = None,
                Ok(false) => {
                    // 子代理的 run 落成终态、与它的完成消息投到我们这儿之间有一个很短的
                    // 窗口；直接收尾会把那条消息留到父会话的下一轮，所以先宽限一瞬。
                    let since = *drained_since.get_or_insert_with(tokio::time::Instant::now);
                    if since.elapsed() >= BACKGROUND_COMPLETION_GRACE {
                        return Ok(BackgroundWait::Drained);
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        run_id = %prepared.run_id,
                        %error,
                        "failed to check for running subagents; ending the turn"
                    );
                    return Ok(BackgroundWait::Drained);
                }
            }
            if !announced {
                announced = true;
                tracing::info!(
                    conversation_id = %prepared.conversation_id,
                    run_id = %prepared.run_id,
                    "parent turn is waiting for background subagents"
                );
            }
            tracing::debug!(
                run_id = %prepared.run_id,
                drained = drained_since.is_some(),
                "background wait check"
            );
            let wait = match drained_since {
                Some(since) => BACKGROUND_COMPLETION_GRACE.saturating_sub(since.elapsed()),
                None => BACKGROUND_WORK_POLL,
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    let _ = emit(client, RunEvent::CycleInterrupted).await;
                    return Err(RunOutcome::Cancelled);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        run_id = %prepared.run_id,
                        timeout_secs = BACKGROUND_WORK_TIMEOUT.as_secs(),
                        "background subagents did not report back in time; ending the turn"
                    );
                    return Ok(BackgroundWait::Drained);
                }
                _ = tokio::time::sleep(wait) => {}
                command = client.commands.recv() => match command {
                    Some(
                        RunCommand::InsertMessages(batch) | RunCommand::BreakMessages(batch),
                    ) => pending.push(batch),
                    Some(RunCommand::Cancel) => {
                        let _ = emit(client, RunEvent::CycleInterrupted).await;
                        return Err(RunOutcome::Cancelled);
                    }
                    // 收尾前没有待处理的工具调用，这里不该出现工具结果；忽略即可。
                    Some(RunCommand::ToolResult(_)) => {}
                    None => return Err(client_failure()),
                },
            }
        }
    }

    /// Output budget for the summarizer call.
    ///
    /// The summary stays provider-visible until the next compaction, so it is
    /// capped well below what the model could produce. Providers reject a
    /// `max_tokens` above the model's own output limit, so the constant is
    /// clamped by the configured model limit when one is set.
    async fn summary_output_tokens(
        &self,
        prepared: &PreparedRun,
        settings: &crate::store::CompactionSettings,
    ) -> u64 {
        let summary_limit = self
            .configured_model(&settings.summary_model_hash)
            .await
            .and_then(|model| model.max_output_tokens());
        let run_limit = self
            .store
            .model(&prepared.model.model_id)
            .await
            .ok()
            .flatten()
            .and_then(|model| model.max_output_tokens());
        let requested = settings
            .summary_output_tokens
            .unwrap_or(super::compaction::OUTPUT_TOKENS);
        summary_limit
            .or(run_limit)
            .map_or(requested, |limit| requested.min(limit))
    }

    async fn configured_model(&self, hash: &str) -> Option<crate::model::ModelConfig> {
        let hash = hash.trim();
        if hash.is_empty() {
            return None;
        }
        match self.store.model(hash).await {
            Ok(model) => model,
            Err(error) => {
                tracing::warn!(%error, model_hash = hash, "failed to load the summary model");
                None
            }
        }
    }

    /// Model spec for summarize calls: the configured independent summary model
    /// when present, otherwise the routed conversation model, with reasoning
    /// explicitly off and the output cap applied.
    async fn summary_model_spec(
        &self,
        prepared: &PreparedRun,
        settings: &crate::store::CompactionSettings,
    ) -> crate::model::ModelSpec {
        let output_tokens = self.summary_output_tokens(prepared, settings).await;
        let mut model = summary_model(&prepared.model, output_tokens);
        if let Some(target) = self.configured_model(&settings.summary_model_hash).await {
            // The router resolves built-in models by this stable hash.
            model.model_id = target.model_hash.clone();
            model.display_name = Some(target.display_name.clone());
            if let Some(limit) = target.max_output_tokens() {
                model.max_output_tokens = Some(output_tokens.min(limit));
            }
        }
        model
    }

    async fn auto_compact(
        &self,
        request: CompactionRequest<'_>,
    ) -> std::result::Result<(crate::model::CheckpointId, Option<Usage>), RunOutcome> {
        let CompactionRequest {
            prepared,
            checkpoint,
            messages,
            client,
            cancellation,
            retention,
            calibration,
            settings,
            trigger,
            estimated_tokens,
            accounted_tool_results,
        } = request;
        let current_ids = prepared
            .initial_messages
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<HashSet<_>>();
        let (compactable, _) = super::compaction::partition(messages, &current_ids);
        if compactable.is_empty() {
            let projected = crate::model::project_messages(messages)
                .map_err(|error| RunOutcome::Failed(error.into()))?;
            let message = super::compaction::validate_compacted(
                prepared,
                &projected,
                calibration,
                settings,
            )
            .err()
            .unwrap_or_else(|| {
                "context overflow after compaction: no conversation history can be compacted".into()
            });
            return Err(RunOutcome::Failed(RunFailure::Protocol(message)));
        }

        emit(client, RunEvent::AutoCompactionStarted)
            .await
            .map_err(|_| client_failure())?;
        let model = self.summary_model_spec(prepared, settings).await;
        let mut pending_insertions = Vec::new();
        let mut break_messages = None;
        // Second overflow step: when a summary slot already exists, drop the
        // retained turns around it instead of paying for another summarize call
        // over nearly the same history. The digest keeps the newest concrete
        // messages that this step drops.
        let reused_summary = if retention == super::compaction::Retention::None {
            super::compaction::latest_summary_message(messages)
                .and_then(super::compaction::summary_text)
        } else {
            None
        };
        let (summary, compaction_usage, event_id) = if let Some(existing) = reused_summary {
            tracing::warn!(
                "overflow ladder: reusing the existing summary and dropping the retained turns"
            );
            let summary = match super::compaction::digest_of(&compactable) {
                Some(digest) => format!(
                    "{existing}\n\nRecent context dropped by the summary-only compaction:\n{digest}"
                ),
                None => existing,
            };
            (
                summary,
                None,
                format!("summary:auto:{}:reuse", prepared.run_id),
            )
        } else {
            let mut summary_usage: Option<Usage> = None;
            let mut summary_call_index: Option<u64> = None;
            let mut attempts = 0_u8;
            let (summary, compaction_usage) = 'summary: loop {
                let retry = attempts > 0;
                attempts = attempts.saturating_add(1);
                // A provider call per attempt keeps each request observable.
                let provider_call_index = self
                    .store
                    .begin_provider_call(&prepared.run_id)
                    .await
                    .map_err(|error| RunOutcome::Failed(error.into()))?;
                if summary_call_index.is_none() {
                    summary_call_index = Some(provider_call_index);
                }
                let history = crate::model::project_messages(&compactable)
                    .map(|history| {
                        // First attempt: the calibrated safe budget. Retry after a
                        // failed summarize call: cut the replayed history roughly in
                        // half so an estimator overshoot cannot repeat the failure.
                        if retry {
                            super::compaction::compaction_history_tight(
                                history,
                                prepared.model.context_window_tokens,
                                calibration,
                                settings,
                            )
                        } else {
                            super::compaction::compaction_history(
                                history,
                                prepared.model.context_window_tokens,
                                calibration,
                                settings,
                            )
                        }
                    })
                    .map_err(|error| RunOutcome::Failed(error.into()))?;
                let invocation = crate::model::ModelInvocation {
                    client_source: crate::model::CallClient::Cursor,
                    call_id: format!("{}:{provider_call_index}", prepared.run_id),
                    run_id: prepared.run_id.to_string(),
                    conversation_id: prepared.conversation_id.to_string(),
                    provider_call_index,
                    request: crate::model::ModelRequest {
                        prompt: crate::model::PromptSpec {
                            instructions: super::compaction::INSTRUCTIONS.into(),
                            tools: Vec::new(),
                        },
                        model: model.clone(),
                        history,
                    },
                };
                let cycle_cancellation = cancellation.child_token();
                let (silent_events, mut discarded_events) = tokio::sync::mpsc::channel(256);
                let drain =
                    tokio::spawn(async move { while discarded_events.recv().await.is_some() {} });
                let cycle = {
                    let cycle = consume_model_cycle(
                        self.provider.stream(invocation, cycle_cancellation.clone()),
                        &silent_events,
                        &cycle_cancellation,
                    );
                    tokio::pin!(cycle);
                    loop {
                        tokio::select! {
                            biased;
                            command = client.commands.recv() => match command {
                                Some(RunCommand::InsertMessages(insertion)) => {
                                    pending_insertions.push(insertion);
                                }
                                Some(RunCommand::BreakMessages(messages)) => {
                                    cycle_cancellation.cancel();
                                    break_messages = Some(messages);
                                    break cycle.await;
                                }
                                Some(RunCommand::Cancel) => {
                                    cycle_cancellation.cancel();
                                    let _ = cycle.await;
                                    return Err(RunOutcome::Cancelled);
                                }
                                Some(RunCommand::ToolResult(result)) => {
                                    if late_tool_result(&result, accounted_tool_results) {
                                        continue;
                                    }
                                    cycle_cancellation.cancel();
                                    let _ = cycle.await;
                                    return Err(RunOutcome::Failed(RunFailure::Protocol(
                                        "received a tool result while automatic compaction was running".into(),
                                    )));
                                }
                                None => {
                                    cycle_cancellation.cancel();
                                    let _ = cycle.await;
                                    return Err(client_failure());
                                }
                            },
                            result = &mut cycle => break result,
                        }
                    }
                };
                drop(silent_events);
                let _ = drain.await;
                // A user interruption ends the compaction attempt: keep what was
                // summarized so far and use the fallback slot, exactly as before.
                if break_messages.is_some() {
                    let outcome = match cycle {
                        Ok(cycle) => cycle.usage,
                        Err(failure) => failure.usage,
                    };
                    if let Some(usage) = outcome {
                        accumulate_usage(&mut summary_usage, usage);
                    }
                    break 'summary (
                        super::compaction::fallback_summary(&compactable),
                        summary_usage,
                    );
                }
                let outcome: Result<(), (String, bool)> = match cycle {
                    Ok(cycle) if cycle.calls.is_empty() && !cycle.text.trim().is_empty() => {
                        let usage = cycle.usage;
                        break 'summary (cycle.text.trim().to_string(), {
                            if let Some(usage) = usage {
                                accumulate_usage(&mut summary_usage, usage);
                            }
                            summary_usage
                        });
                    }
                    Ok(cycle) => {
                        if let Some(usage) = cycle.usage {
                            accumulate_usage(&mut summary_usage, usage);
                        }
                        Err((
                            "automatic compaction returned no usable summary".to_string(),
                            true,
                        ))
                    }
                    Err(failure) => {
                        if let Some(usage) = failure.usage {
                            accumulate_usage(&mut summary_usage, usage);
                        }
                        // A tighter replay only helps when the prompt was over
                        // the limit (which the normal retry policy classifies
                        // as non-retryable). Transient provider failures stay
                        // retryable; auth/quota/bad-request failures must not
                        // buy a second summarize call just to fail again.
                        let retryable = failure.retryable
                            || matches!(
                                &failure.failure,
                                RunFailure::Provider(message)
                                    if super::compaction::is_context_overflow(message)
                            );
                        Err((
                            format!("automatic compaction model failed: {:?}", failure.failure),
                            retryable,
                        ))
                    }
                };
                let retryable = outcome
                    .as_ref()
                    .err()
                    .is_some_and(|(_, retryable)| *retryable);
                if attempts < 2 && retryable {
                    tracing::warn!(
                        attempt = attempts,
                        error = outcome.as_ref().err().map(|(message, _)| message),
                        "automatic compaction failed; retrying once with a tighter history"
                    );
                    continue 'summary;
                }
                tracing::warn!(
                    attempts,
                    retryable,
                    error = outcome.as_ref().err().map(|(message, _)| message),
                    "automatic compaction failed; using the fallback summary"
                );
                break 'summary (
                    super::compaction::fallback_summary(&compactable),
                    summary_usage,
                );
            };
            let event_id = format!(
                "summary:auto:{}:{}",
                prepared.run_id,
                summary_call_index.unwrap_or(0)
            );
            (summary, compaction_usage, event_id)
        };
        // Exact file anchors recovered from the shadowed tool calls survive
        // even when the summary model omitted them.
        let summary = super::compaction::with_context_anchors(&summary, messages);
        let archive =
            crate::model::archive_compaction_history(&prepared.run_id.to_string(), &compactable);
        let summary = super::compaction::with_archive_anchor(&summary, archive.as_deref());
        let summary_message = super::compaction::summary_message(&event_id, &summary);
        let summary_message_id = summary_message.message_id.clone();
        let replacement = super::compaction::replacement(
            prepared,
            messages,
            summary_message,
            retention,
            calibration,
            settings,
        )
        .map_err(|error| RunOutcome::Failed(error.into()))?;
        let projected_replacement = crate::model::project_messages(&replacement)
            .map_err(|error| RunOutcome::Failed(error.into()))?;
        super::compaction::validate_compacted(
            prepared,
            &projected_replacement,
            calibration,
            settings,
        )
        .map_err(|message| RunOutcome::Failed(RunFailure::Protocol(message)))?;
        tracing::info!(
            run_id = %prepared.run_id,
            trigger,
            estimated_tokens = estimated_tokens.unwrap_or(0),
            calibration,
            retention = ?retention,
            compactable_messages = compactable.len(),
            replacement_messages = replacement.len(),
            summary_chars = summary.chars().count(),
            "compaction committed"
        );
        let mut checkpoint = self
            .store
            .replace_checkpoint(
                &prepared.conversation_id,
                &prepared.run_id,
                checkpoint,
                &replacement,
            )
            .await
            .map_err(|error| RunOutcome::Failed(error.into()))?;
        let (barrier, ready) = CommitBarrier::before_continue();
        emit(
            client,
            RunEvent::MessagesCommitted(MessagesCommitted {
                checkpoint_id: checkpoint,
                tool_round_version: 0,
                cause: CommitCause::Compaction {
                    summary,
                    summary_message_id,
                },
                barrier,
            }),
        )
        .await
        .map_err(|_| client_failure())?;
        wait_for_state_ready(ready, cancellation).await?;
        emit(client, RunEvent::AutoCompactionCompleted)
            .await
            .map_err(|_| client_failure())?;
        emit(client, RunEvent::UsageSnapshot(context_usage_snapshot(0)))
            .await
            .map_err(|_| client_failure())?;
        checkpoint = super::messages::append_batches(
            &self.store,
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
                &self.store,
                prepared,
                client,
                cancellation,
                checkpoint,
                vec![messages],
            )
            .await?
            .0;
        }
        Ok((checkpoint, compaction_usage))
    }
}

/// 摘要调用用的模型规格：显式关掉思考，并把输出预算收紧到摘要上限。
///
/// 关闭必须是**显式**的（`Some(false)`）：摘要请求走的还是 `ProviderRouter`，那里会用
/// `ModelConfig::configure` 兜底，而 `None` 在它眼里是"调用方没表态"，会被模型配置的
/// 思考强度（Anthropic 默认 xhigh）重新打开——thinking 与正文共享 `max_tokens`，
/// 8K 摘要预算就被思考吃掉了。
fn summary_model(model: &crate::model::ModelSpec, output_tokens: u64) -> crate::model::ModelSpec {
    let mut summary = model.clone();
    summary.max_output_tokens = Some(output_tokens);
    summary.reasoning.enabled = Some(false);
    summary.reasoning.effort = None;
    summary.reasoning.thinking_budget_tokens = None;
    summary
}

/// 这一轮模型调用该用哪份规格。
///
/// 手动 `/summarize` 是 `RunAction::Compact`，它和自动压缩的区别只在触发者：产出的摘要
/// 同样会顶替历史、同样对后续模型可见。所以它也要走 `summary_model`——否则思考（模型配置
/// 默认 xhigh）会与正文抢同一份 `max_tokens`，8K 摘要预算被思考吃掉；而"不写上限"又会让
/// 64k 级模型按整段输出计费、把一段摘要写成几千 token。其它动作原样透传调用方的规格。
fn cycle_model(
    action: &RunAction,
    model: &crate::model::ModelSpec,
    summary_output_tokens: u64,
) -> crate::model::ModelSpec {
    match action {
        RunAction::Compact => summary_model(model, summary_output_tokens),
        _ => model.clone(),
    }
}

fn context_usage_snapshot(tokens: u64) -> Usage {
    Usage {
        input_tokens: Some(tokens),
        context_input_tokens: Some(tokens),
        output_tokens: Some(0),
        total_tokens: Some(tokens),
        cache_read_tokens: Some(0),
        cache_write_tokens: Some(0),
        reasoning_tokens: Some(0),
    }
}

fn update_context_usage_anchor(
    anchor: &mut Option<ContextUsageAnchor>,
    usage: Usage,
    message_count: usize,
) {
    if let Some(context_input_tokens) = usage.context_input_tokens {
        *anchor = Some(ContextUsageAnchor {
            context_input_tokens,
            message_count,
        });
    }
}

fn accumulate_usage(total: &mut Option<Usage>, usage: Usage) {
    match total {
        Some(total) => *total += usage,
        None => *total = Some(usage),
    }
}

pub(super) async fn wait_for_state_ready(
    ready: tokio::sync::oneshot::Receiver<std::result::Result<(), String>>,
    cancellation: &CancellationToken,
) -> std::result::Result<(), RunOutcome> {
    let result = tokio::select! {
        biased;
        result = ready => result,
        _ = cancellation.cancelled() => return Err(RunOutcome::Cancelled),
    };
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(RunOutcome::Failed(RunFailure::Client(error))),
        Err(_) => Err(client_failure()),
    }
}

pub(super) async fn emit(client: &RunPort, event: RunEvent) -> Result<(), ()> {
    client.events.send(event).await.map_err(|_| ())
}

pub(super) fn client_failure() -> RunOutcome {
    RunOutcome::Failed(RunFailure::Client("client event channel closed".into()))
}

/// 迟到的工具结果：它的调用已经收过尾（正常回报，或轮次超时后被补成 interrupted）。
///
/// 超时收轮之后客户端仍可能把真正的结果送上来；那条结果不能当成协议错误，否则一次
/// 超时会被"迟到结果"二次放大成整轮失败。
fn late_tool_result(result: &crate::model::ToolResult, accounted: &HashSet<String>) -> bool {
    let late = accounted.contains(&result.call_id);
    if late {
        tracing::debug!(
            call_id = %result.call_id,
            "ignoring a tool result the round already accounted for"
        );
    }
    late
}

fn failure_message(failure: &RunFailure) -> String {
    match failure {
        RunFailure::Protocol(message)
        | RunFailure::Provider(message)
        | RunFailure::Truncated(message)
        | RunFailure::Store(message)
        | RunFailure::Client(message) => message.clone(),
    }
}

/// 撞长度上限的这一轮，是不是"输入已经把窗口打满"而不是"输出被截断"。
///
/// provider 层把 `max_tokens` 与 `model_context_window_exceeded` 归一成同一个
/// `FinishReason::Length`（`provider/anthropic.rs` 的 stop_reason 映射），run 层拿不到
/// 原文，只能按 provider 自己报的用量分流：prompt 已经吃满可用预算的那一次，压缩是唯一
/// 出路——不压缩的话下一轮带着同一份超限 prompt 再来一遍，用户看到的是同一个错误重复
/// 出现；prompt 远未吃满的那一次是输出被截断，压缩不解决问题，维持终态失败。
///
/// 用量或窗口预算缺失时一律按"输出被截断"处理：宁可保持旧的终态失败，也不要在信息
/// 不足时自动改写历史。
fn exhausted_context_window(
    failure: &RunFailure,
    usage: Option<&Usage>,
    input_budget: Option<u64>,
) -> bool {
    if !matches!(failure, RunFailure::Truncated(_)) {
        return false;
    }
    let (Some(usage), Some(budget)) = (usage, input_budget) else {
        return false;
    };
    usage
        .context_input_tokens
        .or(usage.input_tokens)
        .is_some_and(|tokens| tokens >= budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 溢出压缩阶梯（审计 09/F8）：计数按"当前溢出 episode"走——一次成功的模型调用就
    /// 清零，同一 run 里第二次**相距很远**的溢出仍然先用保留近期轮的压缩，而不是因为
    /// 本 run 早先溢出过就直接跳到"只留摘要"。
    #[test]
    fn a_successful_model_call_resets_the_overflow_ladder() {
        let mut episode = OverflowEpisode::default();
        assert_eq!(
            episode.retention(),
            Some(crate::run::compaction::Retention::Window),
            "第一次溢出保留近期轮"
        );
        episode.provider_refused();
        assert_eq!(
            episode.retention(),
            Some(crate::run::compaction::Retention::None),
            "同一 episode 内第二次只留摘要"
        );
        episode.provider_refused();
        assert_eq!(episode.retention(), None, "第三次不再压缩，直接失败");

        // 模型调用成功 = episode 结束：下一次溢出重新从"保留近期轮"开始。
        episode.model_call_succeeded();
        assert_eq!(
            episode.retention(),
            Some(crate::run::compaction::Retention::Window),
            "跨 episode 不再累计"
        );
        episode.provider_refused();
        assert_eq!(
            episode.retention(),
            Some(crate::run::compaction::Retention::None)
        );
    }

    /// 摘要调用必须拿到"显式关思考"的规格：这一轮之后还会经过 `ProviderRouter` 里的
    /// `ModelConfig::configure`，只有 `Some(false)` 能活过它；`None` 会被模型配置的
    /// 思考强度重新打开（见 `model::configuration` 的同名用例与 `provider::anthropic`
    /// 的发出侧断言）。
    #[test]
    fn the_summarizer_runs_without_reasoning() {
        let mut model = crate::model::ModelSpec::new("claude-sonnet-4");
        model.reasoning.enabled = Some(true);
        model.reasoning.effort = Some("xhigh".into());
        model.reasoning.thinking_budget_tokens = Some(8_192);

        let summary = summary_model(&model, 8_192);

        assert_eq!(summary.model_id, "claude-sonnet-4");
        assert_eq!(summary.max_output_tokens, Some(8_192));
        assert_eq!(summary.reasoning.enabled, Some(false));
        assert_eq!(summary.reasoning.effort, None);
        assert_eq!(summary.reasoning.thinking_budget_tokens, None);
        assert_eq!(model.reasoning.enabled, Some(true), "原规格不被改动");
    }

    /// 手动 `/summarize` 与自动压缩产出同一种摘要，因此必须拿到同一份规格：显式关思考，
    /// 并且把输出收紧到摘要预算——否则 64k 级模型会把"没写上限"当成可以写很长。
    #[test]
    fn a_manual_summarize_uses_the_summary_spec() {
        let mut model = crate::model::ModelSpec::new("claude-sonnet-4");
        model.reasoning.enabled = Some(true);
        model.reasoning.effort = Some("xhigh".into());
        model.reasoning.thinking_budget_tokens = Some(32_000);
        model.max_output_tokens = Some(64_000);

        let compact = cycle_model(&RunAction::Compact, &model, 8_192);

        assert_eq!(compact.model_id, "claude-sonnet-4");
        assert_eq!(compact.max_output_tokens, Some(8_192));
        assert_eq!(compact.reasoning.enabled, Some(false));
        assert_eq!(compact.reasoning.effort, None);
        assert_eq!(compact.reasoning.thinking_budget_tokens, None);
        assert_eq!(model.max_output_tokens, Some(64_000), "原规格不被改动");
    }

    /// 普通 cycle 不套摘要规则：调用方的规格（思考强度、64k 上限）原样进入请求。
    #[test]
    fn a_normal_cycle_keeps_the_caller_spec() {
        let mut model = crate::model::ModelSpec::new("claude-sonnet-4");
        model.reasoning.enabled = Some(true);
        model.reasoning.effort = Some("xhigh".into());
        model.max_output_tokens = Some(64_000);

        let normal = cycle_model(&RunAction::Start, &model, 8_192);

        assert_eq!(normal.max_output_tokens, Some(64_000));
        assert_eq!(normal.reasoning.enabled, Some(true));
        assert_eq!(normal.reasoning.effort.as_deref(), Some("xhigh"));
    }

    fn usage(tokens: Option<u64>) -> Usage {
        Usage {
            input_tokens: tokens,
            context_input_tokens: tokens,
            ..Default::default()
        }
    }

    /// `max_tokens` 与 `model_context_window_exceeded` 在 provider 层归一成同一个
    /// `FinishReason::Length`；这里按 provider 自己报的用量分流：prompt 吃满预算的
    /// 那次回到压缩阶梯，其余情况维持终态失败。
    #[test]
    fn a_length_stop_only_compacts_when_the_prompt_filled_the_budget() {
        let budget = Some(super::super::compaction::context_budget(200_000));
        let truncated = RunFailure::Truncated("model stopped".into());

        assert!(exhausted_context_window(
            &truncated,
            Some(&usage(Some(199_000))),
            budget
        ));
        assert!(!exhausted_context_window(
            &truncated,
            Some(&usage(Some(12_000))),
            budget
        ));
        // 信息不足时不改道：没有用量、没有窗口预算。
        assert!(!exhausted_context_window(&truncated, None, budget));
        assert!(!exhausted_context_window(
            &truncated,
            Some(&usage(Some(199_000))),
            None
        ));
        // 其它失败类型（含上游拒绝的文案）不走这条分流。
        assert!(!exhausted_context_window(
            &RunFailure::Provider("Anthropic 400 Bad Request: prompt is too long".into()),
            Some(&usage(Some(199_000))),
            budget
        ));
    }

    /// 同一张图片在一次 Run 里只回库读一次。
    ///
    /// 水化第二次之前把内容块删掉：缓存命中就不该再回库——旧实现每个模型轮都会重新读
    /// 一遍，这里会以 "Read image Blob is missing" 失败。
    #[tokio::test]
    async fn hydrated_images_are_read_from_the_store_once_per_run() {
        use crate::model::{
            ContentPart, ProjectedContent, ProjectedMessage, Role, ToolImageReference,
            ToolResultContent,
        };

        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let data = vec![7_u8; 4_096];
        let blob = store.put_blob(&data, &[]).await.unwrap();
        let messages = || {
            vec![ProjectedMessage {
                message_id: "tool-result".into(),
                role: Role::Tool,
                content: ProjectedContent::ToolResult(ToolResultContent {
                    call_id: "call".into(),
                    name: "Read".into(),
                    content: "captured".into(),
                    is_error: false,
                    image: Some(ToolImageReference {
                        blob_id: blob.to_base64(),
                        mime_type: "image/png".into(),
                        path: "/tmp/a.png".into(),
                    }),
                    provider_parts: Vec::new(),
                }),
            }]
        };
        let image_data = |messages: &[ProjectedMessage]| {
            let ProjectedContent::ToolResult(result) = &messages[0].content else {
                panic!("expected a tool result")
            };
            let [ContentPart::Text { .. }, ContentPart::Image { data, .. }] =
                result.provider_parts.as_slice()
            else {
                panic!("expected hydrated text and image parts")
            };
            data.clone()
        };

        let cache = ImageCache::default();
        let mut first = messages();
        cache.hydrate(&store, &mut first).await.unwrap();
        assert_eq!(image_data(&first), data);

        sqlx::query("DELETE FROM blobs WHERE blob_id = ?")
            .bind(blob.as_bytes().as_slice())
            .execute(store.pool())
            .await
            .unwrap();
        let mut second = messages();
        cache
            .hydrate(&store, &mut second)
            .await
            .expect("第二次水化应该命中缓存，而不是回库");
        assert_eq!(image_data(&second), data);
    }

    /// 轮次收尾后客户端才送到的结果按"已收尾"处理，不能升级成协议错误。
    #[test]
    fn results_for_calls_the_round_already_settled_are_late() {
        let accounted = HashSet::from(["call-1".to_string()]);
        let result = |call_id: &str| crate::model::ToolResult {
            call_id: call_id.into(),
            content: "late".into(),
            is_error: false,
            image: None,
        };

        assert!(late_tool_result(&result("call-1"), &accounted));
        assert!(!late_tool_result(&result("call-2"), &accounted));
    }
}
