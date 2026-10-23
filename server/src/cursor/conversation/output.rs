//! Projects Run events to live Cursor output and checkpoint steps.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use prost::Message;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    cursor::{
        checkpoint::StepBuffer,
        checkpoint::{
            worker::{CheckpointJob, CheckpointKind, CheckpointWorker, FinalCheckpoints},
            CheckpointBuilder,
        },
        compile::{
            compile_injection, compile_user_message_action, CursorRunContext, RuntimeAction,
        },
        prompting::PromptCompiler,
        protocol::events,
        protocol::proto::agent::v1 as pb,
        services::blob_sync::BlobSynchronizer,
        tools::{
            codec, compat,
            runtime::CursorToolRuntime,
            stream::ToolCallStream,
            tool_call_result::{ToolCompletion, ToolResultReceiver},
            turn_steps::{self, TurnSteps},
            ToolBatchState, ToolDispatcher,
        },
    },
    model::{ConversationId, ToolCall, ToolRoundId, Usage},
    run::{CommandResult, CommitCause, RunEvent, RunFailure, RunHandle, RunOutcome, RunSession},
    store::{Store, ToolRoundStatus},
    Error, Result,
};

use super::{CompiledMessages, ConversationRegistry, MessageDelivery, RunFinish, TransportFinish};
use crate::cursor::transport::TransportHandle;

pub struct ConversationOutput {
    handle: TransportHandle,
    store: Store,
    context: CursorRunContext,
    core: RunSession,
    run: RunHandle,
    tools: ToolDispatcher,
    results: ToolResultReceiver,
    checkpoint: CheckpointBuilder,
    tool_runtime: CursorToolRuntime,
    runtime_actions: mpsc::UnboundedReceiver<RuntimeAction>,
    compiler: PromptCompiler,
    blob_sync: BlobSynchronizer,
    injection_ids: HashSet<String>,
    pending_injections: HashMap<String, PendingInjection>,
    deliveries: mpsc::UnboundedSender<InjectionDelivery>,
    superseded: CancellationToken,
    /// 当前 turn 已提交的 step 数,工具轮用它算 `UpdateCurrentStep` 的 `message_index`。
    ///
    /// 未知时装载一次历史数出来(`06/P3-5`:原来每个工具轮都整份装载,长会话里每轮一次)。
    turn_steps: TurnSteps,
}

/// 一条待投递的注入/插话消息。
struct InjectionDelivery {
    conversation_id: ConversationId,
    compiled: CompiledMessages,
}

/// 注入投递的串行队列：**入队顺序 = 投递顺序**。
///
/// 投递不能就地 `await`：`RunHandle::submit_messages` 要等核心把这批消息提交完，而核心
/// 的提交又要等 output 主循环完成 checkpoint 屏障——在循环里等会死锁。所以投递留在主
/// 循环之外，但必须由**同一个消费任务**逐条发：过去每条注入各 `tokio::spawn` 一个任务，
/// 多线程 runtime 上谁先 `commands.send` 由调度决定，两条注入的落库顺序就可能与用户输入
/// 顺序相反（顺序还会被写进 checkpoint 长期保留）。
fn spawn_injection_deliveries(
    registry: ConversationRegistry,
) -> mpsc::UnboundedSender<InjectionDelivery> {
    let (sender, mut receiver) = mpsc::unbounded_channel::<InjectionDelivery>();
    tokio::spawn(async move {
        while let Some(delivery) = receiver.recv().await {
            let _ = registry
                .deliver(&delivery.conversation_id, delivery.compiled)
                .await;
        }
    });
    sender
}

struct PendingInjection {
    user_message: Option<pb::UserMessage>,
    delivery_batch_id: String,
}

struct InjectionState<'a> {
    active_round: Option<&'a ToolRoundId>,
    active_tool_calls: &'a HashSet<String>,
    completions: &'a HashMap<String, ToolCompletion>,
    interrupted_rounds: &'a mut HashSet<ToolRoundId>,
    interrupted_tool_calls: &'a mut HashSet<String>,
}

pub(crate) struct ConversationOutputDependencies {
    pub superseded: CancellationToken,
    pub tools: ToolDispatcher,
    pub results: ToolResultReceiver,
    pub checkpoint: CheckpointBuilder,
    pub tool_runtime: CursorToolRuntime,
    pub runtime_actions: mpsc::UnboundedReceiver<RuntimeAction>,
    pub compiler: PromptCompiler,
    pub blob_sync: BlobSynchronizer,
}

impl ConversationOutput {
    pub(crate) fn new(
        handle: TransportHandle,
        store: Store,
        context: CursorRunContext,
        core: RunSession,
        run: RunHandle,
        registry: ConversationRegistry,
        runtime: ConversationOutputDependencies,
    ) -> Self {
        let deliveries = spawn_injection_deliveries(registry.clone());
        Self {
            handle,
            store,
            context,
            core,
            run,
            tools: runtime.tools,
            results: runtime.results,
            checkpoint: runtime.checkpoint,
            tool_runtime: runtime.tool_runtime,
            runtime_actions: runtime.runtime_actions,
            compiler: runtime.compiler,
            blob_sync: runtime.blob_sync,
            injection_ids: HashSet::new(),
            pending_injections: HashMap::new(),
            deliveries,
            superseded: runtime.superseded,
            turn_steps: TurnSteps::Unknown,
        }
    }

    pub async fn run(mut self) -> Result<RunFinish> {
        let result = self.run_inner().await;
        if let Err(error) = &result {
            if !self.superseded.is_cancelled() {
                self.abort_execs().await;
                let (category, summary) = match error {
                    Error::Provider(_) | Error::Http(_) => ("provider", error.to_string()),
                    Error::Store(_) | Error::Database(_) | Error::Migration(_) => {
                        ("store", error.to_string())
                    }
                    _ => (
                        "protocol",
                        match error {
                            Error::Protocol(message) => message.clone(),
                            _ => error.to_string(),
                        },
                    ),
                };
                let _ = self
                    .store
                    .finish_run(
                        self.run.run_id(),
                        crate::store::RunStatus::Failed,
                        None,
                        Some((category, summary.as_str())),
                    )
                    .await;
                self.run.cancel();
            }
        }
        result
    }

    /// 当前 turn 已提交的 step 数(工具轮算 `message_index` 的基础值)。
    ///
    /// 历史形状变过(新 turn / 压缩 / 插话,见 `TurnSteps::invalidate`)时装载一次历史数出来
    /// 并记住,其余时候工具轮只往前累加 —— 原来每个工具轮都整份装载(`06/P3-5`)。
    /// 拿不准就重新数,绝不猜:装载也只是每 turn 一次。
    async fn current_turn_steps(&mut self) -> Result<usize> {
        if let Some(steps) = self.turn_steps.known() {
            return Ok(steps);
        }
        let messages = self
            .store
            .load_current_messages(&ConversationId::new(&self.context.exec.conversation_id))
            .await?;
        let steps = turn_steps::turn_step_count(&messages);
        self.turn_steps.remember(steps);
        Ok(steps)
    }

    async fn run_inner(&mut self) -> Result<RunFinish> {
        if self.context.compacting {
            self.handle.emit(&events::summary_started())?;
        }
        let mut worker = CheckpointWorker::spawn(
            self.store.clone(),
            self.checkpoint.clone(),
            self.handle.clone(),
            self.context.mode,
        );
        let mut checkpoint_worker_open = true;
        let mut calls = BTreeMap::<usize, ToolCall>::new();
        let mut streams = BTreeMap::<usize, ToolCallStream>::new();
        let mut completions = HashMap::<String, ToolCompletion>::new();
        let mut completed = HashSet::<String>::new();
        let mut completed_round = None::<ToolRoundId>;
        let mut response_text = String::new();
        let mut response_thinking = String::new();
        let mut active_round = None::<ToolRoundId>;
        let mut active_tool_calls = HashSet::<String>::new();
        let mut interrupted_rounds = HashSet::<ToolRoundId>::new();
        let mut interrupted_tool_calls = HashSet::<String>::new();
        let mut final_checkpoint = None::<FinalCheckpoints>;
        let mut compaction_checkpoint = None::<pb::ConversationStateStructure>;
        let mut turn_usage = None::<Usage>;
        let mut context_tokens = None::<u64>;
        let mut ready = VecDeque::new();
        let mut presentation = StepBuffer::default();

        loop {
            if self.superseded.is_cancelled() {
                worker.abort();
                self.abort_execs().await;
                return Ok(RunFinish::Transport(TransportFinish::Cancelled));
            }
            let input = if let Ok(action) = self.runtime_actions.try_recv() {
                Input::RuntimeAction(Some(Box::new(action)))
            } else if let Some(completion) = ready.pop_front() {
                Input::Completion(completion)
            } else {
                tokio::select! {
                    biased;
                    _ = self.superseded.cancelled() => {
                        worker.abort();
                        self.abort_execs().await;
                        return Ok(RunFinish::Transport(TransportFinish::Cancelled));
                    }
                    action = self.runtime_actions.recv() => Input::RuntimeAction(action.map(Box::new)),
                    event = self.core.events.recv() => Input::Event(event),
                    completion = self.results.recv() => Input::CompletionResult(completion),
                    failure = worker.failures.recv(), if checkpoint_worker_open => Input::CheckpointFailure(failure),
                }
            };
            match input {
                Input::CheckpointFailure(Some(error)) => return Err(error),
                Input::CheckpointFailure(None) => {
                    checkpoint_worker_open = false;
                }
                Input::Completion(completion) => {
                    if let Some(completion) = self
                        .forward_completion(completion, &mut completions, &interrupted_tool_calls)
                        .await?
                    {
                        ready.push_back(completion);
                    }
                }
                Input::CompletionResult(Some(result)) => {
                    if let Some(completion) = self
                        .forward_completion(result?, &mut completions, &interrupted_tool_calls)
                        .await?
                    {
                        ready.push_back(completion);
                    }
                }
                Input::CompletionResult(None) => {
                    return Err(Error::Protocol("tool result channel closed".into()));
                }
                Input::RuntimeAction(Some(action)) => match *action {
                    RuntimeAction::Inject(action) => {
                        self.forward_injection(
                            action,
                            active_round.as_ref(),
                            &active_tool_calls,
                            &completions,
                            &mut interrupted_rounds,
                            &mut interrupted_tool_calls,
                        )
                        .await?;
                    }
                    RuntimeAction::UserMessage(action) => {
                        self.forward_user_message(
                            action,
                            active_round.as_ref(),
                            &active_tool_calls,
                            &completions,
                            &mut interrupted_rounds,
                            &mut interrupted_tool_calls,
                        )
                        .await?;
                    }
                },
                Input::RuntimeAction(None) => {
                    return Err(Error::Protocol("runtime action channel closed".into()));
                }
                Input::Event(None) => {
                    worker.abort();
                    return Err(Error::Protocol("core event channel closed".into()));
                }
                Input::Event(Some(event)) => match event {
                    RunEvent::AutoCompactionStarted => {
                        self.handle.emit(&events::summary_started())?;
                    }
                    RunEvent::AutoCompactionCompleted => {
                        self.handle.emit(&events::summary_completed())?;
                    }
                    RunEvent::CycleInterrupted => {
                        response_text.clear();
                        response_thinking.clear();
                        calls.clear();
                        streams.clear();
                        presentation.discard_model_output();
                    }
                    RunEvent::Heartbeat => {
                        self.handle.emit(&events::heartbeat())?;
                    }
                    RunEvent::ModelAttemptFailed { attempt, message } => {
                        tracing::warn!(
                            run_id = %self.run.run_id(),
                            attempt,
                            %message,
                            "retrying model call from current checkpoint"
                        );
                        presentation.finish_model_attempt();
                        for call in calls.values_mut() {
                            if call.arguments.is_null() {
                                call.arguments = serde_json::from_str(&call.arguments_text)
                                    .unwrap_or_else(|_| serde_json::json!({}));
                            }
                            let completion = compat::failure_with_message(
                                call,
                                format!("Model attempt failed before tool completion: {message}"),
                            );
                            self.handle
                                .emit(&codec::tool_completed(call, &completion))?;
                            presentation.tool_completed(&completion);
                        }
                        response_text.clear();
                        response_thinking.clear();
                        calls.clear();
                        streams.clear();
                    }
                    RunEvent::TextStart => {}
                    RunEvent::TextEnd => {
                        if !self.context.compacting {
                            presentation.finish_text();
                        }
                    }
                    RunEvent::TextDelta(delta) => {
                        response_text.push_str(&delta);
                        if self.context.compacting {
                            self.handle.emit(&events::summary_delta(delta))?;
                        } else {
                            presentation.text_delta(&delta);
                            self.emit_model_event(
                                crate::provider::ModelEvent::TextDelta(delta),
                                "",
                            )?;
                        }
                    }
                    RunEvent::ThinkingStart => {}
                    RunEvent::ThinkingDelta(delta) => {
                        response_thinking.push_str(&delta);
                        if !self.context.compacting {
                            presentation.thinking_delta(&delta);
                            self.emit_model_event(
                                crate::provider::ModelEvent::ThinkingDelta(delta),
                                "",
                            )?;
                        }
                    }
                    RunEvent::ThinkingEnd { duration } => {
                        if !self.context.compacting {
                            presentation.finish_thinking(duration);
                            self.handle.emit(&events::thinking_completed(duration))?;
                        }
                    }
                    RunEvent::ToolCallStart {
                        index,
                        call_id,
                        name,
                        model_call_id,
                    } => {
                        let call = ToolCall {
                            index,
                            call_id: call_id.clone(),
                            model_call_id: model_call_id.clone(),
                            name: name.clone(),
                            arguments_text: String::new(),
                            arguments: serde_json::Value::Null,
                            argument_error: None,
                        };
                        self.emit_model_event(
                            crate::provider::ModelEvent::ToolCallStart {
                                index,
                                call_id,
                                name: name.clone(),
                            },
                            &model_call_id,
                        )?;
                        streams.insert(
                            index,
                            ToolCallStream::new(&name, self.context.dynamic_tools.get(&name)),
                        );
                        calls.insert(index, call);
                    }
                    RunEvent::ToolCallArgumentsDelta { index, delta } => {
                        let call = calls.get_mut(&index).ok_or_else(|| {
                            Error::Protocol(format!("unknown streaming tool index: {index}"))
                        })?;
                        call.arguments_text.push_str(&delta);
                        let stream = streams.get_mut(&index).ok_or_else(|| {
                            Error::Protocol(format!("missing Cursor tool stream: {index}"))
                        })?;
                        match stream.arguments_delta(call, &delta) {
                            Ok(messages) => {
                                for message in messages {
                                    self.handle.emit(&message)?;
                                }
                            }
                            Err(Error::Protocol(message)) => {
                                tracing::warn!(
                                    call_id = %call.call_id,
                                    %message,
                                    "ignoring invalid streaming tool arguments until completion"
                                );
                            }
                            Err(Error::Json(error)) => {
                                tracing::warn!(
                                    call_id = %call.call_id,
                                    %error,
                                    "ignoring invalid streaming tool arguments until completion"
                                );
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    RunEvent::ToolCallEnd { index } => {
                        let call = calls.get_mut(&index).ok_or_else(|| {
                            Error::Protocol(format!("unknown completed tool index: {index}"))
                        })?;
                        // A tool call with no arguments streams no argument text.
                        // Treat empty text as an empty object, matching the model
                        // cycle, instead of failing the run on `from_str("")`.
                        call.arguments = if call.arguments_text.trim().is_empty() {
                            serde_json::json!({})
                        } else {
                            serde_json::from_str(&call.arguments_text)
                                .unwrap_or_else(|_| serde_json::json!({}))
                        };
                    }
                    RunEvent::UsageSnapshot(usage) => {
                        if !self.context.compacting {
                            if let Some(output_tokens) = usage.output_tokens {
                                self.handle.emit(&events::token_delta(output_tokens))?;
                            }
                            context_tokens = usage.context_input_tokens;
                        }
                    }
                    RunEvent::Usage(usage) => {
                        if !self.context.compacting {
                            if let Some(output_tokens) = usage.output_tokens {
                                self.handle.emit(&events::token_delta(output_tokens))?;
                            }
                        }
                        if !self.context.compacting {
                            context_tokens = usage.context_input_tokens;
                        }
                        match &mut turn_usage {
                            Some(total) => *total += usage,
                            None => turn_usage = Some(usage),
                        }
                    }
                    RunEvent::ExecuteToolRound {
                        round_id,
                        calls: round_calls,
                    } => {
                        // 本轮的起点 = 当前 turn 已提交的 step 数。这一轮的 assistant 消息会在
                        // 工具结果提交时进入历史(一个调用一条),所以派出之后基础值就往前推进一步,
                        // 下一轮直接用它 —— 不必再装载整段历史(`06/P3-5`)。
                        //
                        // 贡献取自库里这一轮(一条 `tool_rounds` 记录,不是历史):恢复出来的轮没有
                        // 本地 delta,只有库里有它的 text/thinking,用本地变量会少算。
                        let turn_steps = self.current_turn_steps().await?;
                        match self.store.tool_round(&round_id).await? {
                            Some(round) => self.turn_steps.advance(
                                &round.assistant.thinking,
                                &round.assistant.text,
                                round.calls.len(),
                            ),
                            // 查不到这一轮(理论上不会发生):不猜,下一次工具轮重新按历史数。
                            None => self.turn_steps.invalidate(),
                        }
                        // `completed` exists so that replaying ExecuteToolRound for a round
                        // does not dispatch a call this round already committed. Tool call ids
                        // are only unique *within* a round -- the schema says as much with
                        // `UNIQUE (round_id, call_id)` -- so an id retained from an earlier
                        // round would make start_batch skip a fresh call, and tool_round wait
                        // forever for a result nothing will ever produce.
                        if completed_round.as_ref() != Some(&round_id) {
                            completed.clear();
                            completed_round = Some(round_id.clone());
                            // 同样的理由适用于"被打断的调用":call_id 只在轮内唯一,上一轮
                            // 被插话打断后留下的 id 会让这一轮同名调用的结果被丢弃,而
                            // tool_round 会一直等它(那条等待没有超时)。迟到的结果由
                            // runtime 按 exec id 丢弃(tools/mod.rs、codec/response.rs)。
                            interrupted_tool_calls.clear();
                        }
                        active_round = Some(round_id.clone());
                        active_tool_calls = round_calls
                            .iter()
                            .map(|call| call.call_id.clone())
                            .collect();
                        // Runtime actions are deliberately prioritized over core events. An
                        // injection can therefore be observed before the already-queued
                        // ToolRoundStarted event reaches this session. In that case the
                        // accepted injection is still pending delivery and this round must be
                        // detached without starting any root tools.
                        if interrupted_rounds.contains(&round_id)
                            || !self.pending_injections.is_empty()
                        {
                            interrupted_rounds.insert(round_id.clone());
                            interrupted_tool_calls.extend(active_tool_calls.iter().cloned());
                            continue;
                        }
                        for dispatched in self
                            .tools
                            .start_batch(
                                &round_calls,
                                ToolBatchState {
                                    completed: &completed,
                                    response_text: &response_text,
                                    response_thinking: &response_thinking,
                                },
                                turn_steps,
                                &self.context.dynamic_tools,
                                &self.context.exec,
                            )
                            .await?
                        {
                            for message in dispatched.messages {
                                self.handle.emit(&message)?;
                            }
                            if let Some(completion) = dispatched.completion {
                                ready.push_back(completion);
                            }
                        }
                        response_text.clear();
                        response_thinking.clear();
                        calls.clear();
                        streams.clear();
                    }
                    RunEvent::MessagesCommitted(state) => {
                        // 历史形状变了:新 turn 的初始消息、压缩(历史被摘要重写)、以及插话
                        // (插话消息是 user 消息,会把 `turn_step_count` 的起点推到它之后)。
                        // 这三种情况把 step 计数打回未知,下一次工具轮按新历史重新数。
                        if matches!(
                            &state.cause,
                            CommitCause::InitialMessages
                                | CommitCause::Compaction { .. }
                                | CommitCause::RuntimeEvent { .. }
                        ) {
                            self.turn_steps.invalidate();
                        }
                        if matches!(&state.cause, CommitCause::RuntimeEvent { .. }) {
                            response_text.clear();
                            response_thinking.clear();
                            calls.clear();
                            streams.clear();
                        }
                        if let CommitCause::RuntimeEvent { event_id } = &state.cause {
                            // Injections key `pending_injections` by their raw
                            // injection id and commit under `inject-context:{id}`,
                            // while runtime user messages key it by (and commit
                            // under) the full `user-message:{id}` event id. Strip
                            // the injection prefix when present and otherwise use
                            // the event id verbatim so both are cleared and emit
                            // their delivered/appended events.
                            let injection_id = event_id
                                .strip_prefix("inject-context:")
                                .unwrap_or(event_id.as_str());
                            if let Some(pending) = self.pending_injections.remove(injection_id) {
                                let delivered_at_ms = crate::cursor::tools::runtime::now_ms()
                                    .min(i64::MAX as u64)
                                    as i64;
                                self.handle.emit(&events::context_injection_delivered(
                                    injection_id.to_owned(),
                                    pending.delivery_batch_id.clone(),
                                    delivered_at_ms,
                                ))?;
                                if let Some(user_message) = pending.user_message {
                                    self.handle
                                        .emit(&events::user_message_appended(user_message))?;
                                }
                            }
                        }
                        if let CommitCause::ToolRoundStarted(round_id) = &state.cause {
                            active_round = Some(round_id.clone());
                        }
                        let mut tool_round_settled = false;
                        if let CommitCause::ToolResult {
                            call_id,
                            interrupted,
                        } = &state.cause
                        {
                            let snapshot = self
                                .store
                                .tool_round(active_round.as_ref().ok_or_else(|| {
                                    Error::Protocol("tool commit has no active round".into())
                                })?)
                                .await?
                                .ok_or_else(|| {
                                    Error::Store("active tool round disappeared".into())
                                })?;
                            let call = snapshot
                                .calls
                                .iter()
                                .find(|call| call.call_id == *call_id)
                                .ok_or_else(|| {
                                    Error::Protocol(format!(
                                        "committed call is absent from tool round: {call_id}"
                                    ))
                                })?;
                            if !interrupted {
                                let completion = completions.remove(call_id).ok_or_else(|| {
                                    Error::Protocol(format!(
                                        "core committed a tool result without typed Cursor state: {call_id}"
                                    ))
                                })?;
                                self.handle
                                    .emit(&codec::tool_completed(call, &completion))?;
                                presentation.tool_completed(&completion);
                            }
                            completed.insert(call_id.clone());
                            tool_round_settled = snapshot.status == ToolRoundStatus::Settled;
                        }
                        let final_turn = state.cause == CommitCause::FinalTurn;
                        if let CommitCause::Compaction {
                            summary,
                            summary_message_id,
                        } = &state.cause
                        {
                            if !state.barrier.is_required() {
                                return Err(Error::Protocol(
                                    "compaction state has no completion barrier".into(),
                                ));
                            }
                            let (sender, receiver) = oneshot::channel();
                            worker
                                .jobs
                                .send(CheckpointJob {
                                    kind: CheckpointKind::Compaction {
                                        checkpoint_id: state.checkpoint_id,
                                        summary: summary.clone(),
                                        summary_message_id: summary_message_id.clone(),
                                        result: sender,
                                    },
                                    presentation: presentation.take(),
                                    context_tokens: None,
                                    ready: None,
                                })
                                .await
                                .map_err(|_| Error::Protocol("checkpoint worker closed".into()))?;
                            match receiver
                                .await
                                .map_err(|_| Error::Protocol("checkpoint worker stopped".into()))?
                            {
                                Ok(checkpoint) => {
                                    context_tokens = checkpoint_context_tokens(&checkpoint);
                                    compaction_checkpoint = Some(checkpoint);
                                    state.barrier.complete(Ok(()));
                                }
                                Err(error) => {
                                    state.barrier.complete(Err(error.to_string()));
                                    return Err(error);
                                }
                            }
                            continue;
                        }
                        if final_turn {
                            if !state.barrier.is_required() {
                                return Err(Error::Protocol(
                                    "final state has no completion barrier".into(),
                                ));
                            }
                            let (sender, receiver) = oneshot::channel();
                            worker
                                .jobs
                                .send(CheckpointJob {
                                    kind: CheckpointKind::Final {
                                        checkpoint_id: state.checkpoint_id,
                                        result: sender,
                                    },
                                    presentation: presentation.take(),
                                    context_tokens,
                                    ready: None,
                                })
                                .await
                                .map_err(|_| Error::Protocol("checkpoint worker closed".into()))?;
                            match receiver
                                .await
                                .map_err(|_| Error::Protocol("checkpoint worker stopped".into()))?
                            {
                                Ok(checkpoints) => {
                                    final_checkpoint = Some(checkpoints);
                                    state.barrier.complete(Ok(()));
                                }
                                Err(error) => {
                                    state.barrier.complete(Err(error.to_string()));
                                    return Err(error);
                                }
                            }
                        } else if let CommitCause::ToolRoundStarted(round_id) = &state.cause {
                            worker
                                .jobs
                                .send(CheckpointJob {
                                    kind: CheckpointKind::ToolStarted {
                                        round_id: round_id.clone(),
                                        stable_checkpoint_id: state.checkpoint_id,
                                    },
                                    presentation: presentation.take(),
                                    context_tokens,
                                    ready: None,
                                })
                                .await
                                .map_err(|_| Error::Protocol("checkpoint worker closed".into()))?;
                        } else if tool_round_settled {
                            if !state.barrier.is_required() {
                                return Err(Error::Protocol(
                                    "settled tool round has no completion barrier".into(),
                                ));
                            }
                            let (ready, published) = oneshot::channel();
                            worker
                                .jobs
                                .send(CheckpointJob {
                                    kind: CheckpointKind::ToolSettled(state.checkpoint_id),
                                    presentation: presentation.take(),
                                    context_tokens,
                                    ready: Some(ready),
                                })
                                .await
                                .map_err(|_| Error::Protocol("checkpoint worker closed".into()))?;
                            let result = published
                                .await
                                .map_err(|_| Error::Protocol("checkpoint worker stopped".into()))?
                                .map_err(Error::Protocol);
                            match result {
                                Ok(()) => state.barrier.complete(Ok(())),
                                Err(error) => {
                                    state.barrier.complete(Err(error.to_string()));
                                    return Err(error);
                                }
                            }
                            if let Some(round_id) = active_round.take() {
                                interrupted_rounds.remove(&round_id);
                            }
                            active_tool_calls.clear();
                            self.tool_runtime.clear_completed().await;
                        } else if !matches!(&state.cause, CommitCause::ToolResult { .. })
                            && active_round.is_some()
                        {
                            let round_id = active_round.clone().ok_or_else(|| {
                                Error::Protocol("active tool round disappeared".into())
                            })?;
                            worker
                                .jobs
                                .send(CheckpointJob {
                                    kind: CheckpointKind::ToolStarted {
                                        round_id,
                                        stable_checkpoint_id: state.checkpoint_id,
                                    },
                                    presentation: presentation.take(),
                                    context_tokens,
                                    ready: None,
                                })
                                .await
                                .map_err(|_| Error::Protocol("checkpoint worker closed".into()))?;
                        } else if !matches!(&state.cause, CommitCause::ToolResult { .. }) {
                            let requires_ready = state.barrier.is_required();
                            let (ready, published) = oneshot::channel();
                            worker
                                .jobs
                                .send(CheckpointJob {
                                    kind: CheckpointKind::Settled(state.checkpoint_id),
                                    presentation: presentation.take(),
                                    context_tokens,
                                    ready: requires_ready.then_some(ready),
                                })
                                .await
                                .map_err(|_| Error::Protocol("checkpoint worker closed".into()))?;
                            if requires_ready {
                                let result = published
                                    .await
                                    .map_err(|_| {
                                        Error::Protocol("checkpoint worker stopped".into())
                                    })?
                                    .map_err(Error::Protocol);
                                match result {
                                    Ok(()) => state.barrier.complete(Ok(())),
                                    Err(error) => {
                                        state.barrier.complete(Err(error.to_string()));
                                        return Err(error);
                                    }
                                }
                            }
                        }
                    }
                    RunEvent::Ended(outcome) => {
                        if self.superseded.is_cancelled() {
                            worker.abort();
                            self.abort_execs().await;
                            return Ok(RunFinish::Transport(TransportFinish::Cancelled));
                        }
                        return match outcome {
                            RunOutcome::Completed => {
                                if self.context.compacting {
                                    let checkpoint =
                                        compaction_checkpoint.take().ok_or_else(|| {
                                            Error::Protocol(
                                                "Completed compaction without checkpoint".into(),
                                            )
                                        })?;
                                    self.handle.emit(&events::summary_completed())?;
                                    self.handle.emit(&events::turn_ended(turn_usage))?;
                                    for _ in 0..3 {
                                        self.checkpoint.publish(&self.handle, &checkpoint).await?;
                                    }
                                    return Ok(RunFinish::TurnCompleted);
                                }
                                // 没有终态提交的 Completed 只有一种来源：这一轮没有新消息
                                // （重复投递的后台完成通知，引擎只推了一次 settled 提交就
                                // 收尾）。那时收尾状态已经推过——它就是客户端此刻手上的
                                // 状态——缺的只是终态帧。过去这里报 "Completed without
                                // final state"，于是库里记 completed、客户端收到错误
                                // EndStream：同一轮在两个地方说法相反。
                                let Some(checkpoints) = final_checkpoint.take() else {
                                    self.handle.emit(&events::turn_ended(turn_usage))?;
                                    return Ok(RunFinish::TurnCompleted);
                                };
                                self.handle.emit(&events::turn_ended(turn_usage))?;
                                self.checkpoint
                                    .publish(&self.handle, &checkpoints.staged)
                                    .await?;
                                self.checkpoint
                                    .publish(&self.handle, &checkpoints.settled)
                                    .await?;
                                self.handle.emit(&pb::AgentServerMessage {
                                    ttft_breakdown: None,
                                    message: Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(checkpoints.settled)),
                                })?;
                                Ok(RunFinish::TurnCompleted)
                            }
                            RunOutcome::Cancelled => {
                                worker.abort();
                                self.abort_execs().await;
                                Ok(RunFinish::Transport(TransportFinish::Cancelled))
                            }
                            RunOutcome::Failed(failure) => {
                                worker.abort();
                                self.abort_execs().await;
                                Ok(RunFinish::Transport(TransportFinish::Failed(cursor_error(
                                    failure,
                                ))))
                            }
                        };
                    }
                },
            }
        }
    }

    async fn abort_execs(&self) {
        for id in self.tool_runtime.drain_running().await {
            let _ = self.handle.emit(&codec::abort(id));
        }
    }

    async fn forward_completion(
        &self,
        mut completion: ToolCompletion,
        completions: &mut HashMap<String, ToolCompletion>,
        interrupted_tool_calls: &HashSet<String>,
    ) -> Result<Option<ToolCompletion>> {
        if interrupted_tool_calls.contains(&completion.result().call_id) {
            return Ok(None);
        }
        if let Some(image) = completion.take_read_image() {
            let blob_id = self.store.put_blob(&image.data, &[]).await?;
            completion.persist_read_image(&blob_id, &image)?;
        }
        let result = completion.result();
        if result.call_id.is_empty() {
            return Err(Error::Protocol("tool result call_id is empty".into()));
        }
        if completions.contains_key(&result.call_id) {
            return Err(Error::Protocol(format!(
                "duplicate tool result call_id: {}",
                result.call_id
            )));
        }
        if !accept_tool_completion(
            self.run.tool_result(result.clone()).await,
            &self.context.request_id,
            self.run.run_id().as_str(),
            &result.call_id,
        )? {
            return Ok(None);
        }
        completions.insert(result.call_id.clone(), completion.clone());
        let Some(dispatched) = self.tools.continue_after(&result.call_id).await? else {
            return Ok(None);
        };
        for message in dispatched.messages {
            self.handle.emit(&message)?;
        }
        Ok(dispatched.completion)
    }

    async fn forward_user_message(
        &mut self,
        action: pb::UserMessageAction,
        active_round: Option<&ToolRoundId>,
        active_tool_calls: &HashSet<String>,
        completions: &HashMap<String, ToolCompletion>,
        interrupted_rounds: &mut HashSet<ToolRoundId>,
        interrupted_tool_calls: &mut HashSet<String>,
    ) -> Result<()> {
        let user_message = action.user_message.clone().ok_or_else(|| {
            Error::Protocol("Cursor user message action has no UserMessage".into())
        })?;
        let injection_id = format!("user-message:{}", user_message.message_id);
        let message = compile_user_message_action(
            &action,
            self.context.mode,
            &self.compiler,
            &self.blob_sync,
        )
        .await?;
        self.queue_injection(
            injection_id,
            Some(user_message),
            message,
            InjectionState {
                active_round,
                active_tool_calls,
                completions,
                interrupted_rounds,
                interrupted_tool_calls,
            },
        )
        .await
    }

    async fn forward_injection(
        &mut self,
        action: pb::InjectContextAction,
        active_round: Option<&ToolRoundId>,
        active_tool_calls: &HashSet<String>,
        completions: &HashMap<String, ToolCompletion>,
        interrupted_rounds: &mut HashSet<ToolRoundId>,
        interrupted_tool_calls: &mut HashSet<String>,
    ) -> Result<()> {
        if action.injection_id.is_empty() {
            return Err(Error::Protocol(
                "InjectContextAction has no injection_id".into(),
            ));
        }
        if self.injection_ids.contains(&action.injection_id) {
            return Ok(());
        }
        if action.expected_run_id != self.context.request_id {
            let reason = format!(
                "InjectContextAction expected run {}, active run is {}",
                action.expected_run_id, self.context.request_id
            );
            self.handle.emit(&events::context_injection_rejected(
                action.injection_id.clone(),
                reason,
            ))?;
            self.injection_ids.insert(action.injection_id);
            return Ok(());
        }
        let user_message = match action.payload.as_ref() {
            Some(pb::inject_context_action::Payload::UserContext(context)) => {
                context.user_message.clone()
            }
            _ => None,
        };
        let message =
            compile_injection(&action, self.context.mode, &self.compiler, &self.blob_sync).await?;
        self.queue_injection(
            action.injection_id,
            user_message,
            message,
            InjectionState {
                active_round,
                active_tool_calls,
                completions,
                interrupted_rounds,
                interrupted_tool_calls,
            },
        )
        .await
    }

    async fn queue_injection(
        &mut self,
        injection_id: String,
        user_message: Option<pb::UserMessage>,
        message: crate::model::CanonicalMessage,
        state: InjectionState<'_>,
    ) -> Result<()> {
        let delivery_batch_id = injection_id.clone();
        self.injection_ids.insert(injection_id.clone());
        self.pending_injections.insert(
            injection_id.clone(),
            PendingInjection {
                user_message,
                delivery_batch_id,
            },
        );
        self.handle
            .emit(&events::context_injection_queued(injection_id.clone()))?;
        state.interrupted_tool_calls.extend(
            state
                .active_tool_calls
                .iter()
                .filter(|call_id| !state.completions.contains_key(*call_id))
                .cloned(),
        );
        if let Some(round_id) = state.active_round {
            state.interrupted_rounds.insert(round_id.clone());
        }
        self.interrupt_execs().await;
        let event_id = message
            .runtime_event_id
            .clone()
            .ok_or_else(|| Error::Protocol("runtime message has no event identity".into()))?;
        // 只入队：投递由 `spawn_injection_deliveries` 那一个消费任务按入队顺序发出，
        // 在这里等它会把 output 主循环和核心的提交互锁住（见该函数的注释）。
        let _ = self.deliveries.send(InjectionDelivery {
            conversation_id: ConversationId::new(&self.context.exec.conversation_id),
            compiled: CompiledMessages {
                event_id,
                target_run_id: None,
                messages: vec![message],
                delivery: MessageDelivery::BreakMessages,
            },
        });
        Ok(())
    }

    async fn interrupt_execs(&self) {
        for id in self.tools.interrupt_for_message().await {
            let _ = self.handle.emit(&codec::abort(id));
        }
    }

    fn emit_model_event(
        &self,
        event: crate::provider::ModelEvent,
        model_call_id: &str,
    ) -> Result<()> {
        if let Some(message) =
            events::response_event(&event, model_call_id, &self.context.dynamic_tools)?
        {
            self.handle.emit(&message)?;
        }
        Ok(())
    }
}

enum Input {
    Event(Option<RunEvent>),
    Completion(ToolCompletion),
    CompletionResult(Option<Result<ToolCompletion>>),
    RuntimeAction(Option<Box<RuntimeAction>>),
    CheckpointFailure(Option<Error>),
}

fn cursor_error(failure: RunFailure) -> Error {
    match failure {
        RunFailure::Protocol(message) => Error::Protocol(message),
        RunFailure::Provider(message) | RunFailure::Truncated(message) => Error::Provider(message),
        RunFailure::Store(message) => Error::Store(message),
        RunFailure::Client(message) => Error::Protocol(message),
    }
}

/// 回给客户端的短摘要：只取服务端自己拼的那一段，上游正文不进客户端弹窗。
///
/// `Error::Provider` 的形状是 `<label> <status>: <body>`（见 `provider::provider_event_error`
/// 与各家 provider 的 HTTP 错误拼装），冒号前是本地拼的标签与状态码；冒号后是上游
/// SSE/JSON 里的原文，属于外部可控文本。`Error::Http` 整条文本来自 reqwest（含请求
/// URL），没有可信前缀可用，直接给一句固定的。上游原文仍然留在本地：`tracing`、失败现场
/// 诊断，以及 run 落库的 failure summary（桌面端"查看详情"读的就是它）。
fn client_error_summary(error: &Error) -> String {
    match error {
        Error::Provider(message) => match message.split_once(": ") {
            Some((head, _)) if !head.is_empty() => head.to_string(),
            _ => "the provider rejected the request".into(),
        },
        _ => "the provider request could not be completed".into(),
    }
}

fn accept_tool_completion(
    delivery: CommandResult,
    request_id: &str,
    run_id: &str,
    call_id: &str,
) -> Result<bool> {
    match delivery {
        CommandResult::Applied | CommandResult::Duplicate => Ok(true),
        CommandResult::RunClosing | CommandResult::RunEnded | CommandResult::Suppressed => {
            tracing::warn!(
                request_id,
                run_id,
                call_id,
                ?delivery,
                "ignoring ToolCompletion delivered after Run stopped accepting results"
            );
            Ok(false)
        }
        CommandResult::StaleTarget => Err(Error::RunNotFound(request_id.into())),
    }
}

pub(crate) fn finish_success(handle: &TransportHandle) {
    handle.emit_frame(crate::cursor::protocol::connect::encode_end_stream());
    handle.close_output();
}

pub(crate) fn finish_failed(handle: &TransportHandle, error: &Error) -> Result<()> {
    use crate::cursor::protocol::connect::{
        encode_end_stream, encode_error_end_stream, ConnectCode, ConnectErrorDetail,
        ConnectStreamError,
    };
    use crate::cursor::protocol::proto::aiserver::v1 as ai;

    let plain = |code, message| ConnectStreamError {
        code,
        message,
        details: Vec::new(),
    };
    let stream_error = match error {
        Error::Provider(_) | Error::Http(_) => {
            // 弹窗里只放服务端自己拼的短摘要：正文是上游可控文本，而 Cursor 会把这个
            // 字段按 markdown 渲染——字段名本身就是约束（"只给我手写的可信 markdown"），
            // 过去却对最不可信的一类文本置了 true，等于授权上游在错误提示里塞命令链接。
            let summary = client_error_summary(error);
            tracing::error!(
                %error,
                "provider request failed; the client receives the server-side summary only"
            );
            let detail = ai::ErrorDetails {
                error: ai::error_details::Error::ProviderError as i32,
                details: Some(ai::CustomErrorDetails {
                    title: "Provider Error".into(),
                    detail: summary.clone(),
                    allow_command_links_potentially_unsafe_please_only_use_for_handwritten_trusted_markdown: Some(false),
                    is_retryable: Some(true),
                    show_request_id: Some(true),
                    should_show_immediate_error: Some(false),
                }),
                is_expected: Some(true),
            };
            ConnectStreamError {
                code: ConnectCode::Unavailable,
                message: summary,
                details: vec![ConnectErrorDetail {
                    type_name: "aiserver.v1.ErrorDetails".into(),
                    value: STANDARD_NO_PAD.encode(detail.encode_to_vec()),
                }],
            }
        }
        Error::Protocol(message) => plain(ConnectCode::InvalidArgument, message.clone()),
        Error::Decode(_) | Error::Json(_) => plain(ConnectCode::InvalidArgument, error.to_string()),
        Error::RunNotFound(_) => plain(ConnectCode::NotFound, error.to_string()),
        Error::Cancelled => plain(ConnectCode::Canceled, error.to_string()),
        _ => plain(ConnectCode::Internal, error.to_string()),
    };
    handle
        .emit_frame(encode_error_end_stream(&stream_error).unwrap_or_else(|_| encode_end_stream()));
    handle.close_output();
    Ok(())
}

pub(crate) fn finish_cancelled(handle: &TransportHandle) -> Result<()> {
    use crate::cursor::protocol::connect::{
        encode_end_stream, encode_error_end_stream, ConnectCode, ConnectStreamError,
    };
    let error = ConnectStreamError {
        code: ConnectCode::Canceled,
        message: "run was cancelled".into(),
        details: Vec::new(),
    };
    handle.emit_frame(encode_error_end_stream(&error).unwrap_or_else(|_| encode_end_stream()));
    handle.close_output();
    Ok(())
}

fn checkpoint_context_tokens(checkpoint: &pb::ConversationStateStructure) -> Option<u64> {
    checkpoint
        .token_details
        .as_ref()
        .map(|details| u64::from(details.used_tokens))
}

#[cfg(test)]
mod tests {
    use super::{accept_tool_completion, checkpoint_context_tokens, client_error_summary};
    use crate::{cursor::protocol::proto::agent::v1 as pb, run::CommandResult, Error};

    /// 客户端可见的错误文本只能是服务端自己拼的那一段；上游正文（可能带 markdown 与
    /// 命令链接）不进弹窗。
    #[test]
    fn provider_errors_reach_the_client_as_a_server_side_summary() {
        let upstream = Error::Provider(
            "Anthropic 400 Bad Request: {\"error\":{\"message\":\"[run this](command:rm -rf /)\"}}"
                .into(),
        );
        assert_eq!(
            client_error_summary(&upstream),
            "Anthropic 400 Bad Request",
            "冒号前是本地拼的标签与状态码，冒号后是上游原文"
        );
        assert_eq!(
            client_error_summary(&Error::Provider("nothing local to keep".into())),
            "the provider rejected the request",
            "没有本地前缀可留时用固定文案，而不是把上游原文当摘要"
        );
    }

    /// 注入投递必须按入队顺序发出。
    ///
    /// 观察点用"没有活跃 run 的会话"：`deliver` 把消息按到达顺序推进 `pending`，
    /// `take_pending` 按顺序取回。每条注入各 spawn 一个任务的写法在多项时顺序由调度
    /// 决定，队列写法是同一个消费任务逐条发。
    #[tokio::test]
    async fn injections_are_delivered_in_the_order_they_were_accepted() {
        use crate::{
            cursor::prompting::{PromptAssets, PromptCompiler},
            model::{CanonicalMessage, ConversationId, Origin, Role},
            provider::{Provider, ProviderStream},
            search::WebCache,
            store::Store,
        };
        use std::{path::Path, sync::Arc, time::Duration};
        use tokio_util::sync::CancellationToken;

        struct NoProvider;

        impl Provider for NoProvider {
            fn stream(
                &self,
                _invocation: crate::model::ModelInvocation,
                _cancellation: CancellationToken,
            ) -> ProviderStream {
                Box::pin(tokio_stream::empty())
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let assets = PromptAssets::load(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("prompt/cursor")
                .as_path(),
        )
        .unwrap();
        let registry = super::ConversationRegistry::new(
            store,
            Arc::new(NoProvider),
            PromptCompiler::new(assets),
            WebCache::at(directory.path().join("web")).unwrap(),
            None,
        );
        let deliveries = super::spawn_injection_deliveries(registry.clone());
        let conversation_id = ConversationId::new("ordered-conversation");
        for index in 0..4 {
            deliveries
                .send(super::InjectionDelivery {
                    conversation_id: conversation_id.clone(),
                    compiled: super::CompiledMessages {
                        event_id: format!("inject-context:{index}"),
                        target_run_id: None,
                        messages: vec![CanonicalMessage::text(
                            format!("message-{index}"),
                            Role::User,
                            Origin::Runtime,
                            "hello",
                        )],
                        delivery: super::MessageDelivery::BreakMessages,
                    },
                })
                .unwrap();
        }

        let mut delivered = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while delivered.len() < 4 {
            delivered.extend(
                registry
                    .take_pending(&conversation_id)
                    .await
                    .into_iter()
                    .map(|pending| pending.event_id),
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "注入没有在期限内投递完：{delivered:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert_eq!(
            delivered,
            [
                "inject-context:0",
                "inject-context:1",
                "inject-context:2",
                "inject-context:3"
            ]
        );
    }

    #[test]
    fn compacted_checkpoint_replaces_the_in_memory_context_usage() {
        let compacted = pb::ConversationStateStructure {
            token_details: Some(pb::ConversationTokenDetails {
                used_tokens: 20_000,
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(checkpoint_context_tokens(&compacted), Some(20_000));
        assert_eq!(
            checkpoint_context_tokens(&pb::ConversationStateStructure::default()),
            None
        );
    }

    #[test]
    fn closing_and_ended_runs_ignore_known_tool_completions() {
        for delivery in [CommandResult::RunClosing, CommandResult::RunEnded] {
            assert!(!accept_tool_completion(delivery, "request", "run", "call").unwrap());
        }
    }

    #[test]
    fn stale_target_remains_an_error() {
        assert!(matches!(
            accept_tool_completion(CommandResult::StaleTarget, "request", "run", "call"),
            Err(Error::RunNotFound(request_id)) if request_id == "request"
        ));
    }
}
