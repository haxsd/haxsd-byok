//! Owns the current Run and coordinates the Conversation lifecycle.
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    cursor::{
        checkpoint::CheckpointBuilder,
        compile,
        protocol::proto::agent::v1 as pb,
        services::{blob_sync::BlobSynchronizer, context_sync::RequestContextSynchronizer},
        tools::{
            codec, compat,
            runtime::CursorToolRuntime,
            tool_call_result::{tool_result_channel, ToolResultReceiver, ToolResultSender},
            ClientToolEvent, ToolDispatcher,
        },
        transport::{OrderedInbox, TransportHandle},
    },
    model::ConversationId,
    run::{CommandResult, RunEngine, RunHandle, RunPhase},
};

use super::{
    CompiledMessages, ConversationDependencies, ConversationOutput, ConversationOutputDependencies,
    ConversationRegistry, MessageDelivery, RunFinish, TransportCommand, TransportFinish,
};

pub struct ConversationRuntime;

const CONTINUATION_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone)]
struct RunGeneration {
    id: u64,
    request: pb::AgentRunRequest,
    superseded: CancellationToken,
    finished: CancellationToken,
    run: Arc<parking_lot::Mutex<Option<RunHandle>>>,
    results: ToolResultSender,
    runtime_actions: mpsc::UnboundedSender<compile::RuntimeAction>,
    tool_runtime: CursorToolRuntime,
    tools: ToolDispatcher,
}

struct FinishGeneration(CancellationToken);

struct TransportActorGuard(TransportHandle);

impl Drop for TransportActorGuard {
    fn drop(&mut self) {
        self.0.close_transport();
    }
}

impl Drop for FinishGeneration {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// 命令通道在 actor 活着的时候关不掉，所以"通道关闭 / 客户端消失"不是一条真实的收尾路径。
///
/// actor 自己持有 `TransportActorGuard(handle.clone())`（内含命令发送端），另有 blob_sync /
/// context_sync 两份 handle 克隆，注册表那侧即使摘掉条目也仍有发送端在。收尾因此只有两个
/// 入口：`TransportCommand::Disconnect`（`api/cursor/run_sse.rs` 的 Drop → `handle.disconnect()`）
/// 加随后的 draining，以及 `waiting_for_action` 的 idle 超时。这里不写"通道关闭就收尾"的
/// 恢复分支：那条路永远不走，留着只会让人以为"客户端消失"有两条收尾路径，而且它会在
/// 有人改掉了上面的前提时悄悄收掉一条可能还有订阅者的 run。真发生就是响亮的 panic。
fn command_channel_closed() -> ! {
    unreachable!(
        "the actor keeps its own command sender, so the command channel cannot close while it runs; \
         the only teardown entries are TransportCommand::Disconnect and the continuation idle timeout"
    )
}

impl ConversationRuntime {
    pub(crate) fn spawn(
        registry: ConversationRegistry,
        handle: TransportHandle,
        mut receiver: mpsc::Receiver<TransportCommand>,
    ) {
        tokio::spawn(async move {
            let _actor_guard = TransportActorGuard(handle.clone());
            let dependencies = registry.dependencies().clone();
            let blob_sync = BlobSynchronizer::new(
                handle.request_id().into(),
                dependencies.store.clone(),
                handle.clone(),
            );
            let mut inbox = OrderedInbox::starting_at(0);
            let tool_runtime_factory = CursorToolRuntime::default();
            let context_sync =
                RequestContextSynchronizer::new(handle.clone(), dependencies.store.clone());
            let mut current = None::<RunGeneration>;
            let mut next_generation = 1_u64;
            let mut pending_finish = None::<(u64, TransportFinish)>;
            let mut draining = false;
            let mut waiting_for_action = false;
            loop {
                let command = if draining {
                    if !handle.admissions_drained() {
                        tokio::select! {
                            command = receiver.recv() => match command {
                                Some(command) => command,
                                None => command_channel_closed(),
                            },
                            _ = handle.wait_admissions_drained() => continue,
                        }
                    } else {
                        handle.mark_draining();
                        match receiver.try_recv() {
                            Ok(command) => command,
                            Err(mpsc::error::TryRecvError::Empty) => {
                                finish_pending(&registry, &handle, &current, pending_finish.take())
                                    .await;
                                break;
                            }
                            Err(mpsc::error::TryRecvError::Disconnected) => {
                                command_channel_closed()
                            }
                        }
                    }
                } else if waiting_for_action {
                    tokio::select! {
                        command = receiver.recv() => match command {
                            Some(command) => command,
                            None => command_channel_closed(),
                        },
                        _ = tokio::time::sleep(CONTINUATION_IDLE_TIMEOUT) => {
                            let Some(generation) = current.as_ref() else {
                                super::finish_success(&handle);
                                break;
                            };
                            handle.begin_close();
                            pending_finish = Some((generation.id, TransportFinish::Success));
                            draining = true;
                            waiting_for_action = false;
                            continue;
                        }
                    }
                } else {
                    match receiver.recv().await {
                        Some(command) => command,
                        None => command_channel_closed(),
                    }
                };
                match command {
                    TransportCommand::Disconnect => {
                        handle.mark_disconnected();
                        let running_tools = match current.as_ref() {
                            Some(generation) => {
                                generation.tool_runtime.running_exec_ids().await.len()
                            }
                            None => 0,
                        };
                        tracing::info!(
                            request_id = handle.request_id(),
                            conversation_id = handle.conversation_id().unwrap_or("none"),
                            run_id = current
                                .as_ref()
                                .and_then(|generation| generation
                                    .run
                                    .lock()
                                    .as_ref()
                                    .map(|run| run.run_id().to_string()))
                                .as_deref()
                                .unwrap_or("none"),
                            running_tools,
                            "Cursor transport disconnected; cancelling the in-flight run"
                        );
                        if let Some(generation) = current.as_ref() {
                            generation.superseded.cancel();
                            if let Some(run) = generation.run.lock().clone() {
                                run.cancel();
                            }
                            for id in generation.tool_runtime.drain_running().await {
                                let _ = handle.emit(&codec::abort(id));
                            }
                        }
                        let turn_completed = waiting_for_action
                            || current.as_ref().is_some_and(|generation| {
                                generation
                                    .run
                                    .lock()
                                    .as_ref()
                                    .is_none_or(|run| run.phase() != RunPhase::Running)
                            });
                        if current.is_some() && !turn_completed {
                            // 断开发生在这一轮还在跑的时候：这才是"用户停止"。轮次已收尾
                            // （或服务端自己收尾）的断开只是流被关掉，不计入停止，否则
                            // 后台子代理的完成通知会被当成"停了就别打扰"而丢掉。
                            if let Some(conversation_id) = handle.conversation_id() {
                                registry
                                    .mark_stopped(&ConversationId::new(conversation_id))
                                    .await;
                            }
                        }
                        if current.is_some() {
                            // 子代理那条流被客户端断开（关掉子代理/点停止）：父会话也要收到
                            // 一条 aborted，否则它会一直等到超时。官方同样是 aborted 也通知。
                            super::subagents::subagent_finished(
                                &registry,
                                &handle,
                                &TransportFinish::Cancelled,
                            )
                            .await;
                        }
                        if turn_completed {
                            super::finish_success(&handle);
                        } else {
                            super::finish_cancelled(&handle).ok();
                        }
                        break;
                    }
                    TransportCommand::RunFinished { generation, finish } => {
                        if current
                            .as_ref()
                            .is_none_or(|current| current.id != generation)
                        {
                            continue;
                        }
                        match finish {
                            RunFinish::TurnCompleted => {
                                pending_finish = None;
                                draining = false;
                                waiting_for_action = true;
                                // 这一轮跑完就该通知父会话了：transport 收尾还要等一个 idle
                                // 超时，那条路会把唤醒拖到父轮等待的宽限之外（真机上表现为
                                // "有时不唤醒"）。重复通知无害——同一条完成消息按身份去重。
                                super::subagents::subagent_finished(
                                    &registry,
                                    &handle,
                                    &TransportFinish::Success,
                                )
                                .await;
                            }
                            RunFinish::Transport(finish) => {
                                waiting_for_action = false;
                                handle.begin_close();
                                pending_finish = Some((generation, finish));
                                draining = true;
                            }
                        }
                    }
                    TransportCommand::Append { seqno, message } => {
                        for (_seqno, message) in inbox.push(seqno, *message) {
                            {
                                match message.message {
                                    Some(pb::agent_client_message::Message::RunRequest(
                                        request,
                                    )) => {
                                        if let Some(conversation_id) =
                                            request.conversation_id.as_deref()
                                        {
                                            if let Err(error) =
                                                handle.set_conversation_id(conversation_id)
                                            {
                                                tracing::error!(
                                                    request_id = handle.request_id(),
                                                    %error,
                                                    "invalid Cursor conversation id"
                                                );
                                                let _ = super::finish_failed(&handle, &error);
                                                return;
                                            }
                                        }
                                        start_generation(
                                            &registry,
                                            &handle,
                                            &dependencies,
                                            &blob_sync,
                                            &context_sync,
                                            &tool_runtime_factory,
                                            &mut current,
                                            &mut next_generation,
                                            &mut draining,
                                            &mut pending_finish,
                                            &mut waiting_for_action,
                                            request,
                                        )
                                        .await;
                                    }
                                    Some(pb::agent_client_message::Message::ExecClientMessage(
                                        message,
                                    )) => {
                                        if context_sync.handle_client(&message).await {
                                            continue;
                                        }
                                        let Some(generation) = current.as_ref() else {
                                            continue;
                                        };
                                        match codec::client_event(
                                            &message,
                                            &generation.tool_runtime,
                                        )
                                        .await
                                        {
                                            Ok(codec::ClientExecEvent::Delta(message)) => {
                                                let _ = handle.emit(&message);
                                            }
                                            Ok(codec::ClientExecEvent::Message(message)) => {
                                                let _ = handle.emit(&message);
                                            }
                                            Ok(codec::ClientExecEvent::Completed(result)) => {
                                                generation.results.send(*result)
                                            }
                                            Ok(codec::ClientExecEvent::Pending) => {}
                                            Err(error) => generation.results.send_error(error),
                                        }
                                    }
                                    Some(
                                        pb::agent_client_message::Message::ExecClientControlMessage(
                                            message,
                                        ),
                                    ) => {
                                        use pb::exec_client_control_message::Message;
                                        match message.message {
                                            Some(Message::StreamClose(close)) => {
                                                if context_sync.handle_stream_close(close.id).await
                                                {
                                                    continue;
                                                }
                                                let Some(generation) = current.as_ref() else {
                                                    continue;
                                                };
                                                match codec::stream_closed(
                                                    close.id,
                                                    &generation.tool_runtime,
                                                )
                                                .await
                                                {
                                                    Ok(Some(completion)) => {
                                                        generation.results.send(completion)
                                                    }
                                                    Ok(None) => {}
                                                    Err(error) => {
                                                        generation.results.send_error(error)
                                                    }
                                                }
                                            }
                                            Some(Message::Throw(throw)) => {
                                                if context_sync
                                                    .handle_throw(
                                                        throw.id,
                                                        format!(
                                                            "Cursor request context failed: {}",
                                                            throw.error
                                                        ),
                                                    )
                                                    .await
                                                {
                                                    continue;
                                                }
                                                let Some(generation) = current.as_ref() else {
                                                    continue;
                                                };
                                                if generation
                                                    .tool_runtime
                                                    .is_interrupted(throw.id)
                                                    .await
                                                {
                                                    generation
                                                        .tool_runtime
                                                        .discard_exec(throw.id)
                                                        .await;
                                                    continue;
                                                }
                                                match generation
                                                    .tool_runtime
                                                    .take_exec(throw.id)
                                                    .await
                                                {
                                                    Some(pending) => generation.results.send(
                                                        compat::failure_with_message(
                                                            &pending.call,
                                                            format!(
                                                                "Exec {} failed: {}",
                                                                pending.call.call_id, throw.error
                                                            ),
                                                        ),
                                                    ),
                                                    None => tracing::warn!(
                                                        id = throw.id,
                                                        "ignoring failure for unknown tool execution"
                                                    ),
                                                }
                                            }
                                            Some(Message::Heartbeat(_)) | None => {}
                                        }
                                    }
                                    Some(
                                        pb::agent_client_message::Message::InteractionResponse(
                                            message,
                                        ),
                                    ) => {
                                        let Some(generation) = current.as_ref() else {
                                            continue;
                                        };
                                        match generation.tools.interaction_response(&message).await
                                        {
                                            Ok(ClientToolEvent::Completed(completion)) => {
                                                generation.results.send(*completion)
                                            }
                                            Ok(ClientToolEvent::Pending) => {}
                                            Err(error) => generation.results.send_error(error),
                                        }
                                    }
                                    Some(pb::agent_client_message::Message::KvClientMessage(
                                        message,
                                    )) => {
                                        let _ = blob_sync.handle_client(message).await;
                                    }
                                    // TODO: ConversationAction has two different delivery paths that
                                    // must not be conflated:
                                    //
                                    // 1. AgentRunRequest.action starts/resumes a Run. compile::prepare
                                    //    currently consumes UserMessageAction,
                                    //    BackgroundTaskCompletionAction, SummarizeAction and
                                    //    ExecutePlanAction. ResumeAction only works indirectly through
                                    //    the absence of a new runtime event and still needs an explicit
                                    //    implementation that consumes ResumeAction.request_context.
                                    // 2. AgentClientMessage::ConversationAction arrives while a Bidi Run
                                    //    is already active and needs a runtime dispatcher here. Supporting
                                    //    an Action in compile::prepare does not mean this path supports it.
                                    //
                                    // Cursor 3.16 sends a queued follow-up as InjectContextAction.
                                    // It targets expected_run_id and asks the active Run to yield to the
                                    // queued message. The session owns this path because interruption must
                                    // abort active execs and publish a recoverable checkpoint before the
                                    // old Run ends. It must not be reduced to handle.cancel() here.
                                    //
                                    // The remaining unimplemented Action variants are
                                    // ShellCommandAction, StartPlanAction,
                                    // AsyncAskQuestionCompletionAction, BackgroundShellAction,
                                    // BackgroundSubagentAction,
                                    // SubscriptionNotificationAction and GoalContinuationAction.
                                    // Variants whose wire behavior is not captured yet need evidence
                                    // before assigning semantics. Every unsupported runtime Action must
                                    // return an explicit Protocol Error rather than falling through silently.
                                    Some(
                                        pb::agent_client_message::Message::ConversationAction(
                                            conversation_action,
                                        ),
                                    ) => match conversation_action.action.clone() {
                                        Some(
                                            pb::conversation_action::Action::UserMessageAction(
                                                action,
                                            ),
                                        ) => {
                                            let mut action = action;
                                            merge_dynamic_context(
                                                &mut action.request_context,
                                                &conversation_action,
                                            );
                                            let delivered_to_active_run =
                                                current.as_ref().is_some_and(|generation| {
                                                    generation.run.lock().as_ref().is_some_and(
                                                        |run| run.phase() == RunPhase::Running,
                                                    ) && generation
                                                        .runtime_actions
                                                        .send(compile::RuntimeAction::UserMessage(
                                                            action.clone(),
                                                        ))
                                                        .is_ok()
                                                });
                                            if delivered_to_active_run {
                                                continue;
                                            }
                                            let Some(previous) = current.as_ref() else {
                                                continue;
                                            };
                                            let mut request = previous.request.clone();
                                            request.action = Some(conversation_action);
                                            request.conversation_state = None;
                                            request.pre_fetched_blobs.clear();
                                            start_generation(
                                                &registry,
                                                &handle,
                                                &dependencies,
                                                &blob_sync,
                                                &context_sync,
                                                &tool_runtime_factory,
                                                &mut current,
                                                &mut next_generation,
                                                &mut draining,
                                                &mut pending_finish,
                                                &mut waiting_for_action,
                                                request,
                                            )
                                            .await;
                                        }
                                        Some(pb::conversation_action::Action::CancelAction(_)) => {
                                            if let Some(generation) = current.as_ref() {
                                                if let Some(run) = generation.run.lock().clone() {
                                                    run.cancel();
                                                }
                                                for id in
                                                    generation.tool_runtime.drain_running().await
                                                {
                                                    let _ = handle.emit(&codec::abort(id));
                                                }
                                            }
                                        }
                                        Some(
                                            pb::conversation_action::Action::InjectContextAction(
                                                action,
                                            ),
                                        ) => {
                                            let mut action = action;
                                            if let Some(
                                                pb::inject_context_action::Payload::UserContext(
                                                    context,
                                                ),
                                            ) = action.payload.as_mut()
                                            {
                                                merge_dynamic_context(
                                                    &mut context.request_context,
                                                    &conversation_action,
                                                );
                                            }
                                            let Some(generation) = current.as_ref() else {
                                                continue;
                                            };
                                            if generation
                                                .runtime_actions
                                                .send(compile::RuntimeAction::Inject(action))
                                                .is_err()
                                            {
                                                generation.results.send_error(crate::Error::Protocol(
                                                    "InjectContextAction arrived without an active Run"
                                                        .into(),
                                                ));
                                            }
                                        }
                                        Some(
                                            pb::conversation_action::Action::CancelSubagentAction(
                                                action,
                                            ),
                                        ) => {
                                            if let Some(generation) = current.as_ref() {
                                                if let Some(id) = generation
                                                    .tool_runtime
                                                    .running_task_exec_id(&action.subagent_id)
                                                    .await
                                                {
                                                    let _ = handle.emit(&codec::abort(id));
                                                }
                                            }
                                        }
                                        Some(action) => {
                                            tracing::warn!(
                                                request_id = handle.request_id(),
                                                action = runtime_action_name(&action),
                                                "ignoring unsupported runtime ConversationAction"
                                            );
                                        }
                                        None => {
                                            if let Some(generation) = current.as_ref() {
                                                generation.results.send_error(
                                                    crate::Error::Protocol(
                                                        "runtime ConversationAction has no action"
                                                            .into(),
                                                    ),
                                                );
                                            }
                                        }
                                    },
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_generation(
    registry: &ConversationRegistry,
    handle: &TransportHandle,
    dependencies: &ConversationDependencies,
    blob_sync: &BlobSynchronizer,
    context_sync: &RequestContextSynchronizer,
    tool_runtime_factory: &CursorToolRuntime,
    current: &mut Option<RunGeneration>,
    next_generation: &mut u64,
    draining: &mut bool,
    pending_finish: &mut Option<(u64, TransportFinish)>,
    waiting_for_action: &mut bool,
    request: pb::AgentRunRequest,
) {
    // 起新代之前必须把上一代的收尾状态清干净。
    //
    // 上一代以 `RunFinish::Transport(_)` 收尾时，actor 已经把 transport 推进 Closing 并
    // 留下 `pending_finish`，等 admissions 排空后收尾退出。这里若不清，回到循环顶就会
    // 立刻走"已排空"分支：用旧代的 id 收尾（id 与 current 不匹配 → 什么都不做）然后
    // 退出 actor——新代照常跑，它的每一帧却打进已经没人订阅的输出，客户端看到一条空流。
    // 过去这三行只挂在 `RunRequest` 那条路上，运行时 `UserMessageAction` 起的新代就会
    // 变成孤儿；放在这里，任何起代入口都不会漏。
    handle.reopen();
    *draining = false;
    *pending_finish = None;
    *waiting_for_action = false;

    let previous_finished = if let Some(previous) = current.take() {
        previous.superseded.cancel();
        if let Some(run) = previous.run.lock().clone() {
            run.cancel();
        }
        for id in previous.tool_runtime.interrupt_for_run_replacement().await {
            let _ = handle.emit(&codec::abort(id));
        }
        Some(previous.finished.clone())
    } else {
        None
    };
    let (results, result_receiver) = tool_result_channel();
    let (runtime_actions, runtime_action_receiver) =
        mpsc::unbounded_channel::<compile::RuntimeAction>();
    let tool_runtime = tool_runtime_factory.next_run();
    let tools = ToolDispatcher::with_results(
        tool_runtime.clone(),
        results.clone(),
        dependencies.store.clone(),
        dependencies.web_cache.clone(),
    );
    let generation = RunGeneration {
        id: *next_generation,
        request: request.clone(),
        superseded: CancellationToken::new(),
        finished: CancellationToken::new(),
        run: Arc::new(parking_lot::Mutex::new(None)),
        results,
        runtime_actions,
        tool_runtime,
        tools,
    };
    *next_generation = next_generation.saturating_add(1);
    *current = Some(generation.clone());
    spawn_run_request(
        registry.clone(),
        handle.clone(),
        request,
        dependencies.clone(),
        blob_sync.clone(),
        context_sync.clone(),
        generation,
        previous_finished,
        result_receiver,
        runtime_action_receiver,
    );
}

async fn finish_pending(
    registry: &ConversationRegistry,
    handle: &TransportHandle,
    current: &Option<RunGeneration>,
    pending: Option<(u64, TransportFinish)>,
) {
    let Some((generation, finish)) = pending else {
        return;
    };
    if current
        .as_ref()
        .is_some_and(|current| current.id == generation)
    {
        // 先把子代理自己这条流收干净（用户看到它结束），再通知父会话——
        // 父会话那边可能在忙，通知慢一步不该拖着子代理不收尾。
        finish_transport(handle, &finish);
        super::subagents::subagent_finished(registry, handle, &finish).await;
    }
}

fn finish_transport(handle: &TransportHandle, finish: &TransportFinish) {
    match finish {
        TransportFinish::Success => super::finish_success(handle),
        TransportFinish::Failed(error) => {
            let _ = super::finish_failed(handle, error);
        }
        TransportFinish::Cancelled => {
            let _ = super::finish_cancelled(handle);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_run_request(
    registry: ConversationRegistry,
    handle: TransportHandle,
    request: pb::AgentRunRequest,
    dependencies: ConversationDependencies,
    blob_sync: BlobSynchronizer,
    context_sync: RequestContextSynchronizer,
    generation: RunGeneration,
    previous_finished: Option<CancellationToken>,
    results: ToolResultReceiver,
    runtime_actions: mpsc::UnboundedReceiver<compile::RuntimeAction>,
) {
    tokio::spawn(async move {
        let _finished = FinishGeneration(generation.finished.clone());
        if let Some(previous_finished) = previous_finished {
            tokio::select! {
                biased;
                _ = generation.superseded.cancelled() => return,
                _ = previous_finished.cancelled() => {}
            }
        }
        if generation.superseded.is_cancelled() {
            return;
        }

        // 客户端又在这个会话里开口了："用户停止过"的标记到此失效。放在编译之前是有意的——
        // 等 run 建立起来再清，会把这段窗口里完成的子代理通知白白丢掉。
        if let Some(conversation_id) = request.conversation_id.as_deref() {
            registry
                .clear_stopped(&ConversationId::new(conversation_id))
                .await;
        }

        let mut checkpoint = CheckpointBuilder::new(
            dependencies.store.clone(),
            blob_sync.clone(),
            handle
                .parent()
                .and_then(|parent| parent.tool_call_id.clone()),
            // 状态结构不在构造时克隆：编译阶段只按 `request.conversation_state` 水合消息与
            // prefetched blob，编译完再把整份状态**移动**进 builder（`adopt`）。过去这里
            // `clone()` 一整份会话状态，长会话每轮白拷一次（审计 05/P2-1）。
            None,
        );
        let prepared = tokio::select! {
            biased;
            _ = generation.superseded.cancelled() => return,
            prepared = compile::prepare(
                handle.request_id(),
                &request,
                compile::PrepareDependencies {
                    compiler: &dependencies.compiler,
                    store: &dependencies.store,
                    checkpoint: &checkpoint,
                    blob_sync: &blob_sync,
                    context_sync: &context_sync,
                    local_rules_dir: dependencies.local_rules_dir.as_deref(),
                    parent: handle.parent(),
                },
            ) => prepared,
        };
        let mut request = request;
        checkpoint.adopt(request.conversation_state.take());
        let (mut prepared, context) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if generation.superseded.is_cancelled() {
                    return;
                }
                tracing::error!(
                    request_id = handle.request_id(),
                    %error,
                    "failed to prepare Cursor Run"
                );
                let _ = handle
                    .command(TransportCommand::RunFinished {
                        generation: generation.id,
                        finish: RunFinish::Transport(TransportFinish::Failed(error)),
                    })
                    .await;
                return;
            }
        };
        checkpoint.configure(
            prepared.model.model_id.clone(),
            prepared.model.context_window_tokens,
            context.checkpoint_prompt.instructions.clone(),
            context.checkpoint_prompt.tools.clone(),
            context.dynamic_tools.keys().cloned().collect(),
            context.turn_user.clone(),
        );
        if context.background_completion {
            let event_id = prepared
                .initial_messages
                .first()
                .and_then(|message| message.runtime_event_id.clone())
                .unwrap_or_else(|| format!("background:{}", prepared.run_id));
            match registry
                .deliver(
                    &prepared.conversation_id,
                    CompiledMessages {
                        event_id,
                        target_run_id: None,
                        messages: prepared.initial_messages.clone(),
                        delivery: MessageDelivery::InsertMessages,
                    },
                )
                .await
            {
                CommandResult::Applied | CommandResult::Duplicate => {
                    tracing::info!(
                        conversation_id = %prepared.conversation_id,
                        run_id = %prepared.run_id,
                        "background completion inserted into the active run"
                    );
                    if !generation.superseded.is_cancelled() {
                        let _ = handle
                            .command(TransportCommand::RunFinished {
                                generation: generation.id,
                                finish: RunFinish::Transport(TransportFinish::Success),
                            })
                            .await;
                    }
                    return;
                }
                CommandResult::Suppressed => {
                    // 只有唤醒类投递（`deliver_wake`）会返回这个结果，而这里走的是
                    // 客户端自己发来的完成通知：真走到这里说明投递口径被改过，别静默吞掉。
                    tracing::warn!(
                        conversation_id = %prepared.conversation_id,
                        run_id = %prepared.run_id,
                        "background completion suppressed on a path that should not suppress"
                    );
                    return;
                }
                CommandResult::RunClosing => {
                    // 会话正在收尾：这条完成消息只能排进 pending，等下一次该会话的
                    // 运行把它当成运行时的前置消息。客户端下次开口才会看到它。
                    tracing::info!(
                        conversation_id = %prepared.conversation_id,
                        "background completion queued: the conversation run was closing"
                    );
                    prepared.initial_messages.clear();
                    tokio::select! {
                        biased;
                        _ = generation.superseded.cancelled() => return,
                        _ = registry.wait_until_idle(&prepared.conversation_id) => {}
                    }
                    if let Ok(checkpoint) = dependencies
                        .store
                        .ensure_conversation(&prepared.conversation_id)
                        .await
                    {
                        prepared.base_checkpoint_id = checkpoint;
                    }
                }
                CommandResult::RunEnded => {
                    tracing::info!(
                        conversation_id = %prepared.conversation_id,
                        "background completion queued: no run is active for this conversation"
                    );
                    prepared.initial_messages.clear();
                }
                CommandResult::StaleTarget => {
                    tracing::warn!(
                        conversation_id = %prepared.conversation_id,
                        "background completion dropped: its target run was replaced"
                    );
                    if !generation.superseded.is_cancelled() {
                        let _ = handle
                            .command(TransportCommand::RunFinished {
                                generation: generation.id,
                                finish: RunFinish::Transport(TransportFinish::Success),
                            })
                            .await;
                    }
                    return;
                }
            }
        }
        let pending = registry.take_pending(&prepared.conversation_id).await;
        if !pending.is_empty() {
            let mut messages = pending
                .into_iter()
                .flat_map(|pending| pending.messages)
                .collect::<Vec<_>>();
            messages.extend(prepared.initial_messages);
            prepared.initial_messages = messages;
            if let Ok(checkpoint) = dependencies
                .store
                .ensure_conversation(&prepared.conversation_id)
                .await
            {
                prepared.base_checkpoint_id = checkpoint;
            }
        }
        if generation.superseded.is_cancelled() {
            return;
        }

        let run_id = prepared.run_id.clone();
        let conversation_id = prepared.conversation_id.clone();
        let (port, core, run_handle) = crate::run::channel(run_id.clone(), 256);
        *generation.run.lock() = Some(run_handle.clone());
        if generation.superseded.is_cancelled() {
            run_handle.cancel();
            *generation.run.lock() = None;
            return;
        }
        registry
            .activate(conversation_id.clone(), run_id.clone(), run_handle.clone())
            .await;
        let cancellation = run_handle.cancellation();
        let engine = RunEngine::new(dependencies.store.clone(), dependencies.provider.clone());
        let core_run = tokio::spawn(async move { engine.run(prepared, port, cancellation).await });
        let output = ConversationOutput::new(
            handle.clone(),
            dependencies.store.clone(),
            context,
            core,
            run_handle,
            registry.clone(),
            ConversationOutputDependencies {
                superseded: generation.superseded.clone(),
                tools: generation.tools.clone(),
                results,
                runtime_actions,
                compiler: dependencies.compiler.clone(),
                blob_sync,
                checkpoint,
                tool_runtime: generation.tool_runtime.clone(),
            },
        );
        let finish = match output.run().await {
            Ok(finish) => finish,
            Err(error) => {
                if generation.superseded.is_cancelled() {
                    RunFinish::Transport(TransportFinish::Cancelled)
                } else {
                    tracing::error!(
                        request_id = handle.request_id(),
                        %error,
                        "Cursor session failed"
                    );
                    RunFinish::Transport(TransportFinish::Failed(error))
                }
            }
        };
        let _ = core_run.await;
        registry.release(&conversation_id, &run_id).await;
        if generation
            .run
            .lock()
            .as_ref()
            .is_some_and(|run| run.run_id() == &run_id)
        {
            *generation.run.lock() = None;
        }
        if !generation.superseded.is_cancelled() {
            let _ = handle
                .command(TransportCommand::RunFinished {
                    generation: generation.id,
                    finish,
                })
                .await;
        }
    });
}

/// 把父 `ConversationAction` 上报的请求上下文并进具体 action 的 `request_context`。
///
/// `context::request_context`（整条 RunRequest 那条路）的取值顺序是"`request_context_parts.
/// dynamic_context` 优先，其次才是 action 自己的 `request_context`"；运行时插话/注入走的
/// 是另一条路——只把具体 action 发进 output actor，父级的 `dynamic_context` 在这里就丢了，
/// 编译出来的运行时消息因此是空上下文。在入口补齐，两条路取到同一份上下文。
fn merge_dynamic_context(target: &mut Option<pb::RequestContext>, parent: &pb::ConversationAction) {
    let Some(dynamic) = parent
        .request_context_parts
        .as_ref()
        .and_then(|parts| parts.dynamic_context.as_ref())
    else {
        return;
    };
    *target = Some(dynamic.clone());
}

fn runtime_action_name(action: &pb::conversation_action::Action) -> &'static str {
    use pb::conversation_action::Action;

    match action {
        Action::UserMessageAction(_) => "UserMessageAction",
        Action::ResumeAction(_) => "ResumeAction",
        Action::CancelAction(_) => "CancelAction",
        Action::SummarizeAction(_) => "SummarizeAction",
        Action::ShellCommandAction(_) => "ShellCommandAction",
        Action::StartPlanAction(_) => "StartPlanAction",
        Action::ExecutePlanAction(_) => "ExecutePlanAction",
        Action::AsyncAskQuestionCompletionAction(_) => "AsyncAskQuestionCompletionAction",
        Action::CancelSubagentAction(_) => "CancelSubagentAction",
        Action::BackgroundTaskCompletionAction(_) => "BackgroundTaskCompletionAction",
        Action::BackgroundShellAction(_) => "BackgroundShellAction",
        Action::BackgroundSubagentAction(_) => "BackgroundSubagentAction",
        Action::SubscriptionNotificationAction(_) => "SubscriptionNotificationAction",
        Action::GoalContinuationAction(_) => "GoalContinuationAction",
        Action::InjectContextAction(_) => "InjectContextAction",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use crate::{
        cursor::{
            prompting::{PromptAssets, PromptCompiler},
            protocol::connect,
            transport::TransportRegistry,
        },
        model::{ModelConfigInput, ModelInvocation, ModelType},
        provider::{FinishReason, ModelEvent, Provider, ProviderStream},
        store::Store,
    };
    use prost::Message;

    use super::*;

    struct ScriptedProvider {
        responses: parking_lot::Mutex<VecDeque<Vec<crate::Result<ModelEvent>>>>,
        calls: AtomicUsize,
    }

    impl ScriptedProvider {
        fn push(&self, events: Vec<crate::Result<ModelEvent>>) {
            self.responses.lock().push_back(events);
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Provider for ScriptedProvider {
        fn stream(
            &self,
            _invocation: ModelInvocation,
            _cancellation: CancellationToken,
        ) -> ProviderStream {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let events = self.responses.lock().pop_front().unwrap_or_default();
            Box::pin(tokio_stream::iter(events))
        }
    }

    fn model_input(model_id: &str) -> ModelConfigInput {
        ModelConfigInput {
            sort_order: 0,
            display_name: model_id.into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "test-key".into(),
            tooltip_data: model_id.into(),
            model_id: model_id.into(),
            reasoning_effort: None,
            openai_endpoint: "/v1/chat/completions".into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: Some(100_000),
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        }
    }

    fn client_run(
        request_id: &str,
        conversation_id: &str,
        model_id: &str,
    ) -> pb::AgentClientMessage {
        pb::AgentClientMessage {
            message: Some(pb::agent_client_message::Message::RunRequest(
                pb::AgentRunRequest {
                    action: Some(pb::ConversationAction {
                        action: Some(pb::conversation_action::Action::UserMessageAction(
                            pb::UserMessageAction {
                                user_message: Some(user_message("draining-user-1", "first turn")),
                                ..Default::default()
                            },
                        )),
                        ..Default::default()
                    }),
                    conversation_id: Some(conversation_id.into()),
                    run_id: Some(request_id.into()),
                    requested_model: Some(pb::RequestedModel {
                        model_id: model_id.into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        }
    }

    fn runtime_user_message() -> pb::AgentClientMessage {
        pb::AgentClientMessage {
            message: Some(pb::agent_client_message::Message::ConversationAction(
                pb::ConversationAction {
                    action: Some(pb::conversation_action::Action::UserMessageAction(
                        pb::UserMessageAction {
                            user_message: Some(user_message("draining-user-2", "second turn")),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                },
            )),
        }
    }

    fn user_message(message_id: &str, text: &str) -> pb::UserMessage {
        pb::UserMessage {
            text: text.into(),
            message_id: message_id.into(),
            mode: pb::AgentMode::Agent as i32,
            ..Default::default()
        }
    }

    fn text_response(text: &str) -> Vec<crate::Result<ModelEvent>> {
        vec![
            Ok(ModelEvent::Start {
                model_call_id: format!("call-{text}"),
            }),
            Ok(ModelEvent::TextStart),
            Ok(ModelEvent::TextDelta(text.into())),
            Ok(ModelEvent::TextEnd),
            Ok(ModelEvent::Done(FinishReason::Stop)),
        ]
    }

    fn kv_ack(id: u32) -> pb::AgentClientMessage {
        pb::AgentClientMessage {
            message: Some(pb::agent_client_message::Message::KvClientMessage(
                pb::KvClientMessage {
                    id,
                    message: Some(pb::kv_client_message::Message::SetBlobResult(
                        pb::SetBlobResult { error: None },
                    )),
                },
            )),
        }
    }

    async fn acknowledge_kv(handle: &TransportHandle, seqno: &mut i64, frame: &[u8]) {
        let (flags, payload) = connect::decode_frames(frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            return;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = server.message {
            handle
                .command(TransportCommand::Append {
                    seqno: *seqno,
                    message: Box::new(kv_ack(kv.id)),
                })
                .await
                .unwrap();
            *seqno += 1;
        }
    }

    /// draining 期间收到运行时 `UserMessageAction`：新代必须有人收尾。
    ///
    /// 复现方式贴近真机：上一代以 `RunFinish::Transport(Failed)` 收尾（上一轮报错），
    /// 同时还有一次 append 准入在处理（客户端的心跳/KV 应答就是这个准入），actor 于是
    /// 停在 draining 分支上等命令——这时客户端把重发的消息投进来。旧的写法只在
    /// `RunRequest` 那条路上清 draining，新代照跑而 actor 用旧代的 id 收尾后退出：
    /// 一个帧都收不到。修好后新代正常收尾（turnEnded + 成功 EndStream）。
    #[tokio::test]
    async fn a_user_message_while_draining_still_finishes_the_new_generation() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let model = store
            .create_model(&model_input("draining-model"))
            .await
            .unwrap();
        let provider = Arc::new(ScriptedProvider {
            responses: parking_lot::Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
        });
        // 上一代：不可重试的上游错误，run 立刻以 Transport(Failed) 收尾。
        provider.push(vec![Err(crate::Error::Provider(
            "Anthropic 401 Unauthorized: bad key".into(),
        ))]);
        // 新代：正常回答。
        provider.push(text_response("second generation"));
        let registry = TransportRegistry::new(
            store,
            provider.clone(),
            PromptCompiler::new(PromptAssets::embedded().unwrap()),
        );
        let handle = registry.get_or_create("draining-request").await.unwrap();
        let mut output = handle.subscribe();
        let admission = handle.admit().unwrap();
        handle
            .command(TransportCommand::Append {
                seqno: 0,
                message: Box::new(client_run(
                    "draining-request",
                    "draining-conversation",
                    &model.model_hash,
                )),
            })
            .await
            .unwrap();

        // 一边读帧一边等上一代收尾：准备阶段会向客户端要 blob 应答（KV），不读帧就会
        // 把这一代卡在准备阶段。actor 处理 `RunFinished::Transport` 时会 `begin_close`，
        // 那就是"上一代已经收尾、actor 正停在 draining 里"的标志。
        let mut append_seqno = 1;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while handle.accepting_appends() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "上一代没有以 Transport 收尾"
            );
            if let Ok(Some(frame)) =
                tokio::time::timeout(Duration::from_millis(20), output.recv()).await
            {
                let (flags, _) = connect::decode_frames(&frame).unwrap().pop().unwrap();
                assert_eq!(
                    flags & connect::END_STREAM_FLAG,
                    0,
                    "上一代的收尾被准入占着，不该已经关流"
                );
                acknowledge_kv(&handle, &mut append_seqno, &frame).await;
            }
        }
        // 这条运行时消息正好落在 draining 窗口里。
        handle
            .command(TransportCommand::Append {
                seqno: append_seqno,
                message: Box::new(runtime_user_message()),
            })
            .await
            .unwrap();
        append_seqno += 1;
        drop(admission);

        let mut saw_turn_ended = false;
        let ended = loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), output.recv())
                .await
                .expect("新代没有收尾：draining 没被清掉")
                .expect("输出被关闭");
            let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
            if flags & connect::END_STREAM_FLAG != 0 {
                break payload;
            }
            let server = pb::AgentServerMessage::decode(payload).unwrap();
            if let Some(pb::agent_server_message::Message::InteractionUpdate(update)) =
                &server.message
            {
                if matches!(
                    update.message,
                    Some(pb::interaction_update::Message::TurnEnded(_))
                ) {
                    saw_turn_ended = true;
                }
            }
            acknowledge_kv(&handle, &mut append_seqno, &frame).await;
        };

        assert_eq!(ended.as_ref(), b"{}", "新代应当正常收尾");
        assert!(saw_turn_ended, "新代要发出 turnEnded");
        assert_eq!(provider.calls(), 2, "两代各跑一次模型");
    }

    /// 运行时插话/注入的请求上下文：父 `ConversationAction` 的 `dynamic_context` 优先，
    /// 与 `compile::context::request_context` 同一套取值顺序。
    #[test]
    fn the_parent_dynamic_context_wins_for_runtime_actions() {
        let dynamic = pb::RequestContext {
            env: Some(pb::RequestContextEnv {
                workspace_paths: vec!["/dynamic".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let own = pb::RequestContext {
            env: Some(pb::RequestContextEnv {
                workspace_paths: vec!["/own".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let parent = pb::ConversationAction {
            action: Some(pb::conversation_action::Action::UserMessageAction(
                pb::UserMessageAction {
                    user_message: Some(user_message("context-user", "hi")),
                    request_context: Some(own.clone()),
                    ..Default::default()
                },
            )),
            request_context_parts: Some(pb::RequestContextPartReferences {
                dynamic_context: Some(dynamic.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut target = Some(own);
        merge_dynamic_context(&mut target, &parent);
        assert_eq!(target, Some(dynamic));

        // 父级没有 dynamic_context 时不动具体 action 自己的那份。
        let parent_without_dynamic = pb::ConversationAction {
            request_context_parts: Some(pb::RequestContextPartReferences::default()),
            ..parent.clone()
        };
        let mut target = parent_without_dynamic
            .action
            .as_ref()
            .and_then(|action| match action {
                pb::conversation_action::Action::UserMessageAction(action) => {
                    action.request_context.clone()
                }
                _ => None,
            });
        merge_dynamic_context(&mut target, &parent_without_dynamic);
        assert_eq!(
            target
                .as_ref()
                .and_then(|context| context.env.as_ref())
                .and_then(|env| env.workspace_paths.first())
                .map(String::as_str),
            Some("/own")
        );
    }

    /// 收尾只有一个入口：`Disconnect`（`api/cursor/run_sse.rs` 的 Drop 走它）。
    ///
    /// 删掉"通道关闭 / 客户端消失"那几条不可达的恢复分支之后，这条路的结局必须仍然是
    /// "给客户端一条终止帧 + 关掉输出 + transport 关闭"。断开落在一条真跑的代上（客户端
    /// 关掉流、用户点停止都长这样）：actor 收到命令后取消这一代，按"轮次是否已收尾"选
    /// `finish_cancelled` 或 `finish_success`，两者都发终止帧。
    ///
    /// 驱动是确定性的、**不含任何墙钟上界**（并行跑全量套件、CPU 被抢时不会假红）：
    /// `wait_transport_closed()` 等到的是 actor 退出时 `TransportActorGuard` 的 Drop，
    /// 而 `finish_*` 一定在它之前跑完——所以它返回时终止帧已经躺在输出队列里，之后
    /// `try_recv` 只是把队列读空：`Empty` 表示"收尾没跑（输出还开着、也没有终止帧）"，
    /// `Disconnected` 才是收尾完成。注册表条目退休由集成用例
    /// `cursor_transport_lifecycle::actor_exit_removes_the_matching_transport_and_allows_a_new_generation`
    /// 负责（那一步在另一个任务里做，unit 用例里等它只能靠墙钟）。
    #[tokio::test]
    async fn disconnecting_finishes_the_run_with_a_terminal_frame() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let model = store
            .create_model(&model_input("disconnect-model"))
            .await
            .unwrap();
        let provider = Arc::new(ScriptedProvider {
            responses: parking_lot::Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
        });
        let registry = TransportRegistry::new(
            store,
            provider,
            PromptCompiler::new(PromptAssets::embedded().unwrap()),
        );
        let handle = registry.get_or_create("disconnect-request").await.unwrap();
        let mut output = handle.subscribe();
        handle
            .command(TransportCommand::Append {
                seqno: 0,
                message: Box::new(client_run(
                    "disconnect-request",
                    "disconnect-conversation",
                    &model.model_hash,
                )),
            })
            .await
            .unwrap();

        handle.command(TransportCommand::Disconnect).await.unwrap();
        handle.wait_transport_closed().await;

        let mut flags_seen = Vec::new();
        loop {
            match output.try_recv() {
                Ok(frame) => {
                    let (flags, _) = connect::decode_frames(&frame).unwrap().pop().unwrap();
                    flags_seen.push(flags);
                }
                Err(mpsc::error::TryRecvError::Empty) => panic!(
                    "actor 已经退出，收尾却没跑：输出还开着、也没有终止帧（已收到的 flags: {flags_seen:?}）"
                ),
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        assert!(
            flags_seen
                .iter()
                .any(|flags| flags & connect::END_STREAM_FLAG != 0),
            "收尾必须给客户端一条终止帧：{flags_seen:?}"
        );
        assert!(!handle.accepting_appends(), "收尾之后不该再接受 append");
    }
}
