//! Compiles an AgentRunRequest into a PreparedRun.
use std::collections::BTreeMap;

use prost::Message as _;
use uuid::Uuid;

use crate::{
    cursor::prompting::{Mode, PromptCompiler},
    cursor::{
        checkpoint::messages,
        checkpoint::CheckpointBuilder,
        protocol::proto::agent::v1 as pb,
        services::blob_sync::BlobSynchronizer,
        services::context_sync::RequestContextSynchronizer,
        tools::runtime::{ExecContext, ExecStage, PendingExec, SubagentModel},
        transport::TransportParent,
    },
    model::{
        CanonicalMessage, ContentPart, ConversationId, MessageContent, Origin, PreparedRun,
        PromptSpec, Role, RunAction, RunId, RunKind, ToolCallContent, ToolResult,
        ToolResultContent, ToolRoundId,
    },
    store::{BlobId, Store},
    Error, Result,
};

use super::{break_messages, context, insert_messages, model};

struct ActionProjection {
    mode: i32,
    turn_user: Option<pb::UserMessage>,
    action_context: String,
    event_id: Option<String>,
    input_id: Option<String>,
    starts_turn: bool,
    compacting: bool,
    background_completion: bool,
}

pub struct CursorRunContext {
    pub request_id: String,
    pub mode: i32,
    pub turn_user: Option<pb::UserMessage>,
    pub exec: ExecContext,
    pub dynamic_tools: BTreeMap<String, pb::McpToolDefinition>,
    pub checkpoint_prompt: PromptSpec,
    pub compacting: bool,
    pub background_completion: bool,
    /// 新动作里已解析的 pending Task 结果：checkpoint builder 用它们补子代理关联。
    pub resolved_tool_calls: Vec<pb::ToolCall>,
}

pub(crate) struct PrepareDependencies<'a> {
    pub compiler: &'a PromptCompiler,
    pub store: &'a Store,
    pub checkpoint: &'a CheckpointBuilder,
    pub blob_sync: &'a BlobSynchronizer,
    pub context_sync: &'a RequestContextSynchronizer,
    pub local_rules_dir: Option<&'a std::path::Path>,
    /// 客户端随这次 append 带来的父身份（子代理请求才有，且两个头可以只出现一个）。
    pub parent: Option<&'a TransportParent>,
}

pub(crate) async fn prepare(
    request_id: &str,
    request: &pb::AgentRunRequest,
    dependencies: PrepareDependencies<'_>,
) -> Result<(PreparedRun, CursorRunContext)> {
    let PrepareDependencies {
        compiler,
        store,
        checkpoint,
        blob_sync,
        context_sync,
        local_rules_dir,
        parent,
    } = dependencies;
    checkpoint
        .import_prefetched(&request.pre_fetched_blobs)
        .await?;
    let conversation_id = ConversationId::new(
        request
            .conversation_id
            .clone()
            .unwrap_or_else(|| request_id.into()),
    );
    let run_id = execution_run_id(request_id);
    let mut base_messages = if request.conversation_state.is_some() {
        Some(
            checkpoint
                .hydrate_messages(request.conversation_state.as_ref())
                .await?,
        )
    } else {
        None
    };
    if let Some(trace) = blob_sync.trace() {
        let hydrated_messages = base_messages.as_deref().unwrap_or_default();
        let hydrated_images = hydrated_messages
            .iter()
            .map(|message| match &message.content {
                MessageContent::Parts { parts } => parts
                    .iter()
                    .filter(|part| matches!(part, ContentPart::Image { .. }))
                    .count(),
                _ => 0,
            })
            .sum::<usize>();
        let history = request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref())
            .and_then(|action| match action {
                pb::conversation_action::Action::UserMessageAction(action) => {
                    action.conversation_history.as_ref()
                }
                _ => None,
            });
        let summary = serde_json::json!({
            "checkpoint_root_count": request.conversation_state.as_ref().map_or(0, |state| state.root_prompt_messages_json.len()),
            "checkpoint_turn_count": request.conversation_state.as_ref().map_or(0, |state| state.turns.len()),
            "conversation_history_message_count": history.map_or(0, |history| history.messages.len()),
            "hydrated_message_count": hydrated_messages.len(),
            "hydrated_image_count": hydrated_images,
            "selected_source": "root_prompt_messages_json",
        });
        let encoded = serde_json::to_vec(&summary)?;
        trace.artifact("history_projection", "byok_server", &encoded, summary);
    }
    let mut request_context = context::hydrate(request, context_sync).await?;
    if let Some(rules_dir) = local_rules_dir {
        context::merge_local_rules(&mut request_context, rules_dir).await;
    }
    let request_context = request_context;
    let ActionProjection {
        mode: mode_number,
        mut turn_user,
        action_context,
        mut event_id,
        input_id,
        starts_turn,
        compacting,
        background_completion,
    } = action(request)?;
    // C02：新用户动作携带的 pending 调用真实结果。只有客户端把 checkpoint 里那一个
    // pending 批次的每个调用都显式收尾时才接管；缺一条就保留原有行为，不凭空补结果。
    let pending_resolution = if !compacting && starts_turn {
        let resolutions = match request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref())
        {
            Some(pb::conversation_action::Action::UserMessageAction(action)) => {
                action.interrupted_pending_tool_call_resolutions.as_ref()
            }
            _ => None,
        };
        let pending = request.conversation_state.as_ref().and_then(|state| {
            match state.pending_tool_calls.as_slice() {
                [pending] => Some(pending.as_str()),
                _ => None,
            }
        });
        match (pending, resolutions) {
            (Some(pending), Some(resolutions)) => resolve_pending_round(pending, resolutions)?,
            _ => None,
        }
    } else {
        None
    };
    let (pending_messages, resolved_tool_calls) = match pending_resolution {
        Some(resolved) => {
            let completed = store
                .complete_pending_tool_round(
                    &conversation_id,
                    resolved.started_at_ms,
                    &resolved.call_ids,
                    &resolved.results,
                )
                .await?;
            if !completed {
                tracing::warn!(
                    conversation_id = %conversation_id,
                    started_at_ms = resolved.started_at_ms,
                    "no local pending tool round matched the resolutions; keeping the canonical projection only"
                );
            }
            (resolved.messages, resolved.resolved_calls)
        }
        None => (Vec::new(), Vec::new()),
    };
    let checkpoint_mode = if request.subagent_type_name.is_some() {
        Mode::Subagent
    } else {
        mode_from_proto(mode_number)?
    };
    let mut model = model::requested_model(request)?;
    if let Some(configured_model) = store.model(&model.model_id).await? {
        configured_model.configure(&mut model);
    }
    let dynamic = context::dynamic_mcp(request, &request_context)?;
    let subagent_model_overrides = model::overrides(request)?;
    let subagents_disabled = subagent_model_overrides
        .first()
        .is_some_and(|(_, selection)| {
            matches!(selection, crate::model::SubagentModelOverride::Disabled)
        });
    let mut checkpoint_prompt = compiler.prompt_spec(
        checkpoint_mode,
        &model,
        &dynamic
            .values()
            .map(|(_, definition)| definition.clone())
            .collect::<Vec<_>>(),
        request.suppress_subagent_progress_update_tool == Some(true),
    )?;
    if subagents_disabled {
        checkpoint_prompt.tools.retain(|tool| tool.name != "Task");
    }
    let prompt = if compacting {
        compiler.prompt_spec(Mode::Compaction, &model, &[], false)?
    } else {
        checkpoint_prompt.clone()
    };
    let proposed_base_checkpoint_id = match base_messages.as_mut() {
        Some(messages) if !messages.is_empty() => {
            validate_prompt_root(messages)?;
            messages.retain(|message| {
                !(message.role == Role::System && message.origin == Origin::Prompt)
            });
            store.import_checkpoint(&conversation_id, messages).await?
        }
        Some(_) | None => store.ensure_conversation(&conversation_id).await?,
    };
    let base_checkpoint_id = match input_id.as_deref() {
        Some(input_id) => {
            store
                .anchor_input(&conversation_id, input_id, proposed_base_checkpoint_id)
                .await?
        }
        None => proposed_base_checkpoint_id,
    };
    // C01：ResumeAction 自带的请求上下文只有 parts/dynamic_context 之外的这一条来源时，
    // 也必须作为新事件追加，而不是只让 `context::hydrate` 读到它、历史里却没有。
    //
    // no-state/空-state Resume 的 base 是当前 checkpoint，但 `base_messages` 为空；
    // 内容去重必须看权威的当前链，否则同一份 context 每次重试都会再追加一条。
    let resume_action = matches!(
        request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref()),
        Some(pb::conversation_action::Action::ResumeAction(_))
    );
    let resume_history = if !compacting
        && resume_action
        && base_messages
            .as_deref()
            .is_none_or(|messages| messages.is_empty())
    {
        store.load_checkpoint_messages(base_checkpoint_id).await?
    } else {
        Vec::new()
    };
    let resume_context_message = if !compacting && !background_completion && resume_action {
        let history = base_messages
            .as_deref()
            .filter(|messages| !messages.is_empty())
            .unwrap_or(&resume_history);
        break_messages::compile_resume_context(base_checkpoint_id, &request_context, history)?
    } else {
        None
    };
    let mut projected_user_context = if input_id.is_some() && !compacting && !background_completion
    {
        break_messages::compile_request_context(
            "identity",
            &request_context,
            base_messages.as_deref().unwrap_or_default(),
        )?
    } else {
        None
    };
    if event_id.is_none() {
        if let (Some(input_id), Some(user)) = (input_id.as_deref(), turn_user.as_ref()) {
            event_id = Some(
                break_messages::user_event_id(
                    input_id,
                    checkpoint_mode,
                    user,
                    &request_context,
                    &action_context,
                    projected_user_context
                        .as_ref()
                        .map(|message| &message.content),
                    compiler,
                    blob_sync,
                )
                .await?,
            );
        }
    }
    let existing_runtime = match event_id.as_deref() {
        Some(event_id) => {
            store
                .message(&conversation_id, &format!("runtime:{event_id}"))
                .await?
        }
        _ => None,
    };
    let request_context_message = match event_id.as_deref() {
        Some(event_id) if !compacting && !background_completion => {
            let message_id = format!("request-context:{event_id}");
            match store.message(&conversation_id, &message_id).await? {
                Some(message) => Some(message),
                None if input_id.is_some() => projected_user_context.take().map(|mut message| {
                    message.message_id = message_id;
                    message
                }),
                None => break_messages::compile_request_context(
                    event_id,
                    &request_context,
                    base_messages.as_deref().unwrap_or_default(),
                )?,
            }
        }
        _ => None,
    };
    let mut initial_messages = if compacting {
        Vec::new()
    } else {
        match (turn_user.clone(), event_id) {
            (Some(mut user), Some(event_id)) if background_completion => {
                let (message, text) = match existing_runtime {
                    Some(message) => {
                        let text = runtime_message_text(&message)?;
                        (message, text)
                    }
                    None => {
                        break_messages::compile_background(
                            event_id,
                            &user,
                            &request_context,
                            &action_context,
                            blob_sync,
                        )
                        .await?
                    }
                };
                user.text = text;
                turn_user = Some(user);
                vec![message]
            }
            (Some(user), Some(event_id)) => {
                let runtime = match existing_runtime {
                    Some(message) => message,
                    None => {
                        break_messages::compile(
                            event_id,
                            checkpoint_mode,
                            &user,
                            &request_context,
                            &action_context,
                            compiler,
                            blob_sync,
                        )
                        .await?
                    }
                };
                request_context_message
                    .into_iter()
                    .chain(std::iter::once(runtime))
                    .collect()
            }
            (None, None) => Vec::new(),
            _ => {
                return Err(Error::Protocol(
                    "Cursor action has an incomplete runtime event".into(),
                ))
            }
        }
    };
    if let Some(message) = resume_context_message {
        initial_messages.push(message);
    }
    if !pending_messages.is_empty() {
        let mut combined = pending_messages;
        combined.append(&mut initial_messages);
        initial_messages = combined;
    }
    let (base_checkpoint_id, reused) = store
        .match_checkpoint_prefix(&conversation_id, base_checkpoint_id, &initial_messages)
        .await?;
    initial_messages.drain(..reused);
    let action = if background_completion {
        // 完成通知是幂等投递：身份相同就说明有人已经送过，别因为正文差别重跑一轮。
        RunAction::BackgroundCompletion
    } else if compacting {
        RunAction::Compact
    } else if starts_turn {
        RunAction::Start
    } else {
        let pending_tool_round = match request
            .conversation_state
            .as_ref()
            .map(|state| state.pending_tool_calls.as_slice())
            .unwrap_or_default()
        {
            [] => None,
            [pending] => Some(messages::decode_pending(pending)?),
            pending => {
                return Err(Error::Protocol(format!(
                    "Cursor resume contains {} pending assistant messages",
                    pending.len()
                )))
            }
        };
        RunAction::Resume { pending_tool_round }
    };
    let exec = exec_context(
        request,
        &request_context,
        &conversation_id,
        &model.model_id,
        subagents_disabled,
        &subagent_model_overrides,
    );
    let kind = run_kind(request, parent, store).await?;
    Ok((
        PreparedRun {
            run_id,
            cursor_request_id: Some(request_id.into()),
            conversation_id,
            kind,
            model,
            prompt,
            initial_messages,
            action,
            base_checkpoint_id,
        },
        CursorRunContext {
            request_id: request_id.into(),
            mode: mode_number,
            turn_user,
            exec,
            dynamic_tools: dynamic
                .into_iter()
                .map(|(name, (wire, _))| (name, wire))
                .collect(),
            checkpoint_prompt,
            compacting,
            background_completion,
            resolved_tool_calls,
        },
    ))
}

struct ResolvedPendingRound {
    messages: Vec<CanonicalMessage>,
    resolved_calls: Vec<pb::ToolCall>,
    results: Vec<ToolResult>,
    call_ids: Vec<String>,
    started_at_ms: u64,
}

/// 把新动作里携带的 pending 调用真实结果变成可提交的历史消息。
///
/// 只接管"checkpoint 里只有一个 pending 批次，且每个调用都有中断结果"的情况：
/// 缺一条就返回 `None`，保留原有 Start 行为（待决调用不进入 canonical 历史），不凭空
/// 编造未提供的 shell/task 结果。返回的 `results`/`call_ids`/`started_at_ms` 同时交给
/// `Store::complete_pending_tool_round`，让原 `tool_round_calls` 也收敛为 completed；
/// Task 的原始 `pb::ToolCall` 继续供 checkpoint builder 写父调用 → 子代理关联。
fn resolve_pending_round(
    pending: &str,
    resolutions: &pb::InterruptedPendingToolCallResolutions,
) -> Result<Option<ResolvedPendingRound>> {
    let round = messages::decode_pending(pending)?;
    let by_call = resolutions
        .resolutions
        .iter()
        .map(|resolution| (resolution.tool_call_id.as_str(), resolution))
        .collect::<std::collections::HashMap<_, _>>();
    if round
        .calls
        .iter()
        .any(|call| !by_call.contains_key(call.call_id.as_str()))
    {
        return Ok(None);
    }
    let started_at_ms = round.started_at_ms;
    let call_ids = round
        .calls
        .iter()
        .map(|call| call.call_id.clone())
        .collect::<Vec<_>>();
    let digest = BlobId::digest(pending.as_bytes()).to_base64();
    let round_id = format!("cursor-pending:{digest}");
    let assistant = CanonicalMessage {
        message_id: format!("{round_id}:assistant"),
        role: Role::Assistant,
        origin: Origin::Assistant,
        content: MessageContent::Assistant {
            text: round.assistant.text,
            thinking: round.assistant.thinking,
            tool_round_id: Some(ToolRoundId::new(round_id.clone())),
            replay_state: round.assistant.replay_state,
            tool_calls: round
                .calls
                .iter()
                .map(|call| ToolCallContent {
                    index: call.index,
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
        },
        runtime_event_id: None,
    };
    let mut messages = vec![assistant];
    let mut resolved_calls = Vec::new();
    let mut results = Vec::new();
    for call in &round.calls {
        let Some(wire) =
            crate::cursor::tools::runtime::resolution_exec_message(by_call[call.call_id.as_str()])
        else {
            return Ok(None);
        };
        let completion = match crate::cursor::tools::tool_call_result::from_exec(
            PendingExec {
                call: call.clone(),
                context: ExecContext::default(),
                started_at_ms: round.started_at_ms,
                stdout: String::new(),
                stderr: String::new(),
                stage: ExecStage::Direct,
            },
            &wire,
        ) {
            Ok(completion) => completion,
            Err(error) => {
                tracing::warn!(
                    call_id = %call.call_id,
                    %error,
                    "ignoring an invalid pending tool resolution"
                );
                return Ok(None);
            }
        };
        resolved_calls.push(completion.tool_call().clone());
        let result = completion.result().clone();
        messages.push(CanonicalMessage {
            message_id: format!("{round_id}:{}:result", call.call_id),
            role: Role::Tool,
            origin: Origin::Tool,
            content: MessageContent::ToolResult(ToolResultContent {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                content: result.content.clone(),
                is_error: result.is_error,
                image: None,
                provider_parts: Vec::new(),
            }),
            runtime_event_id: None,
        });
        results.push(result);
    }
    Ok(Some(ResolvedPendingRound {
        messages,
        resolved_calls,
        results,
        call_ids,
        started_at_ms,
    }))
}

/// 这次运行的身份：子代理运行（请求带了 `subagent_type_name`）记下父身份。
///
/// 父身份两个来源都是可选的：父请求头只有客户端发才有，而父运行可能已经结束
/// （子代理比父轮活得久是常态）——查不到就留空，不猜、也不因此拒绝运行。
async fn run_kind(
    request: &pb::AgentRunRequest,
    parent: Option<&TransportParent>,
    store: &Store,
) -> Result<RunKind> {
    let Some(name) = request.subagent_type_name.as_deref() else {
        return Ok(RunKind::Root);
    };
    let parent_run_id = match parent.and_then(|parent| parent.request_id.as_deref()) {
        Some(request_id) => store.active_run_for_cursor_request(request_id).await?,
        None => None,
    };
    Ok(RunKind::Subagent {
        parent_run_id,
        parent_tool_call_id: parent.and_then(|parent| parent.tool_call_id.clone()),
        kind: model::subagent_kind(name),
    })
}

fn runtime_message_text(message: &CanonicalMessage) -> Result<String> {
    let MessageContent::Parts { parts } = &message.content else {
        return Err(Error::Protocol(
            "stored runtime message does not contain parts".into(),
        ));
    };
    let Some(ContentPart::Text { text }) = parts.first() else {
        return Err(Error::Protocol(
            "stored runtime message does not start with text".into(),
        ));
    };
    Ok(text.clone())
}

fn validate_prompt_root(messages: &[CanonicalMessage]) -> Result<()> {
    let prompts = messages
        .iter()
        .filter(|message| message.role == Role::System && message.origin == Origin::Prompt)
        .collect::<Vec<_>>();
    let [prompt] = prompts.as_slice() else {
        return Err(Error::Protocol(format!(
            "Cursor history contains {} system prompt roots",
            prompts.len()
        )));
    };
    let MessageContent::Parts { parts } = &prompt.content else {
        return Err(Error::Protocol(
            "Cursor system prompt root is not textual content".into(),
        ));
    };
    let [ContentPart::Text { .. }] = parts.as_slice() else {
        return Err(Error::Protocol(
            "Cursor system prompt root is not one text part".into(),
        ));
    };
    Ok(())
}

fn execution_run_id(request_id: &str) -> RunId {
    let execution_id = Uuid::new_v4().simple().to_string();
    RunId::new(format!("{request_id}:{}", &execution_id[..8]))
}

/// Windows Cursor 会发**没有** `message_id` 的子代理启动消息。
///
/// 从消息内容里推出一个稳定的 identity（而不是拒绝请求）：同一条 kickoff 换一个
/// request id 重试时，落到的仍是同一个 `input_id` 与同一个运行时事件（幂等，不会
/// 产生重复 turn）；同一会话里内容不同的消息各自保有独立身份。
fn synthetic_message_id(user: &pb::UserMessage) -> String {
    let mut identity = user.clone();
    identity.message_id = String::new();
    format!(
        "synthetic:{}",
        BlobId::digest(&identity.encode_to_vec()).to_base64()
    )
}

fn action(request: &pb::AgentRunRequest) -> Result<ActionProjection> {
    let conversation_mode = request
        .conversation_state
        .as_ref()
        .and_then(|state| state.mode);
    let mode = conversation_mode.unwrap_or(pb::AgentMode::Agent as i32);
    let Some(action) = request
        .action
        .as_ref()
        .and_then(|action| action.action.as_ref())
    else {
        return Ok(ActionProjection {
            mode,
            turn_user: None,
            action_context: String::new(),
            event_id: None,
            input_id: None,
            starts_turn: false,
            compacting: false,
            background_completion: false,
        });
    };
    match action {
        pb::conversation_action::Action::UserMessageAction(action) => {
            let user = action.user_message.as_ref().ok_or_else(|| {
                Error::Protocol("Cursor user message action has no UserMessage".into())
            })?;
            let mode = if user.mode == pb::AgentMode::Unspecified as i32 {
                conversation_mode.unwrap_or(user.mode)
            } else {
                user.mode
            };
            let mut turn_user = user.clone();
            if turn_user.message_id.is_empty() {
                // Windows 客户端的子代理 kickoff 不带 message_id；从内容推出稳定身份，
                // 让重试保持幂等（见 synthetic_message_id）。
                turn_user.message_id = synthetic_message_id(&turn_user);
            }
            if turn_user.text.trim() == "/summarize" {
                return Ok(ActionProjection {
                    mode,
                    turn_user: Some(turn_user),
                    action_context: String::new(),
                    event_id: None,
                    input_id: None,
                    starts_turn: false,
                    compacting: true,
                    background_completion: false,
                });
            }
            let mut context = action
                .prepend_user_messages
                .iter()
                .map(|message| message.text.trim())
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            context.extend(
                turn_user
                    .subagent_system_reminder
                    .iter()
                    .filter(|text| !text.is_empty())
                    .cloned(),
            );
            let input_id = format!("cursor:user:{}", turn_user.message_id);
            Ok(ActionProjection {
                mode,
                turn_user: Some(turn_user),
                action_context: context.join("\n\n"),
                event_id: None,
                input_id: Some(input_id),
                starts_turn: true,
                compacting: false,
                background_completion: false,
            })
        }
        pb::conversation_action::Action::BackgroundTaskCompletionAction(action) => {
            let Some(projection) = insert_messages::project(action, mode)? else {
                // 只有进度/改派通知的批次：没有可投影的完成事件。按"这条请求没有 action"
                // 处理（不写 runtime 消息、不换 turn user），而不是把整轮 run 判成协议错误。
                return Ok(ActionProjection {
                    mode,
                    turn_user: None,
                    action_context: String::new(),
                    event_id: None,
                    input_id: None,
                    starts_turn: false,
                    compacting: false,
                    background_completion: false,
                });
            };
            let event_id = projection.turn_user.message_id.clone();
            Ok(ActionProjection {
                mode,
                action_context: projection.context,
                event_id: Some(event_id),
                input_id: None,
                turn_user: Some(projection.turn_user),
                starts_turn: true,
                compacting: false,
                background_completion: true,
            })
        }
        pb::conversation_action::Action::ExecutePlanAction(action) => execute_plan(action),
        pb::conversation_action::Action::SummarizeAction(_) => Ok(ActionProjection {
            mode,
            turn_user: None,
            action_context: String::new(),
            event_id: None,
            input_id: None,
            starts_turn: false,
            compacting: true,
            background_completion: false,
        }),
        _ => Ok(ActionProjection {
            mode,
            turn_user: None,
            action_context: String::new(),
            event_id: None,
            input_id: None,
            starts_turn: false,
            compacting: false,
            background_completion: false,
        }),
    }
}

fn execute_plan(action: &pb::ExecutePlanAction) -> Result<ActionProjection> {
    let plan = action
        .plan_file_content
        .as_deref()
        .or_else(|| action.plan.as_ref().map(|plan| plan.plan.as_str()))
        .filter(|plan| !plan.trim().is_empty())
        .ok_or_else(|| Error::Protocol("ExecutePlan is missing plan content".into()))?;
    let source = action
        .plan_file_uri
        .as_deref()
        .or(action.plan_file_path.as_deref())
        .filter(|source| !source.is_empty());
    let action_context = match source {
        Some(source) => {
            format!("<approved_plan>\n<plan_file>{source}</plan_file>\n{plan}\n</approved_plan>")
        }
        None => format!("<approved_plan>\n{plan}\n</approved_plan>"),
    };
    let identity = BlobId::digest(
        format!(
            "{}\0{}\0{}\0{}\0{}",
            action.execution_mode,
            action.plan_id.as_deref().unwrap_or_default(),
            action.kickoff_message_id.as_deref().unwrap_or_default(),
            source.unwrap_or_default(),
            plan,
        )
        .as_bytes(),
    )
    .to_base64();
    let event_id = format!("execute-plan:{identity}");
    Ok(ActionProjection {
        mode: action.execution_mode,
        turn_user: Some(pb::UserMessage {
            text: "Execute the approved plan.".into(),
            message_id: event_id.clone(),
            mode: action.execution_mode,
            ..Default::default()
        }),
        action_context,
        event_id: Some(event_id),
        input_id: None,
        starts_turn: true,
        compacting: false,
        background_completion: false,
    })
}

pub(super) fn mode_from_proto(mode: i32) -> Result<Mode> {
    let mode = pb::AgentMode::try_from(mode)
        .map_err(|_| Error::Protocol(format!("unknown Cursor agent mode: {mode}")))?;
    match mode {
        pb::AgentMode::Agent => Ok(Mode::Agent),
        pb::AgentMode::Ask => Ok(Mode::Ask),
        pb::AgentMode::Plan => Ok(Mode::Plan),
        pb::AgentMode::Debug => Ok(Mode::Debug),
        pb::AgentMode::Multitask => Ok(Mode::Multitask),
        // 客户端会发这几种模式（未指定、triage、project、custom），而提示词资产里
        // 没有对应的一套。以前它们让 run 在编译阶段直接失败——用户那条消息连库都进不去，
        // 看起来像被吞了；按 Agent 处理至少让会话继续，原始枚举值留给日志。
        other => {
            tracing::warn!(
                mode = other.as_str_name(),
                "no prompt asset for this Cursor agent mode, using Agent"
            );
            Ok(Mode::Agent)
        }
    }
}

fn exec_context(
    request: &pb::AgentRunRequest,
    request_context: &pb::RequestContext,
    conversation_id: &ConversationId,
    model_id: &str,
    subagents_disabled: bool,
    overrides: &[(
        crate::model::SubagentKind,
        crate::model::SubagentModelOverride,
    )],
) -> ExecContext {
    let subagent_model = overrides.first().map(|(_, value)| match value {
        crate::model::SubagentModelOverride::Explicit(model) => {
            SubagentModel::Model(model.model_id.clone())
        }
        crate::model::SubagentModelOverride::Inherit => SubagentModel::Model(model_id.into()),
        crate::model::SubagentModelOverride::Disabled => SubagentModel::Disabled,
    });
    ExecContext {
        conversation_id: conversation_id.to_string(),
        root_conversation_id: request
            .conversation_group_id
            .clone()
            .unwrap_or_else(|| conversation_id.to_string()),
        default_subagent_model: model_id.into(),
        subagent_model,
        allow_subagents: request.subagent_type_name.is_none() && !subagents_disabled,
        subagents_disabled,
        terminals_folder: request_context
            .env
            .as_ref()
            .map(|env| env.terminals_folder.clone())
            .unwrap_or_default(),
        workspace_paths: request_context
            .env
            .as_ref()
            .map(|env| env.workspace_paths.clone())
            .unwrap_or_default(),
        admin_command_denylist: request_context.admin_command_denylist.clone(),
        mcp_routes: context::meta_mcp_routes(request_context),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 客户端会发未指定/triage/project/custom 这几种模式,提示词资产里没有对应的一套。
    /// 它们不能让 run 在编译阶段失败:那样用户的消息连库都进不去,看起来像被吞了。
    #[test]
    fn cursor_modes_without_a_prompt_asset_fall_back_to_agent() {
        for mode in [
            pb::AgentMode::Unspecified,
            pb::AgentMode::Triage,
            pb::AgentMode::Project,
            pb::AgentMode::Custom,
        ] {
            assert_eq!(
                mode_from_proto(mode as i32).unwrap(),
                Mode::Agent,
                "{mode:?} 应回退到 Agent"
            );
        }
        for (mode, expected) in [
            (pb::AgentMode::Agent, Mode::Agent),
            (pb::AgentMode::Ask, Mode::Ask),
            (pb::AgentMode::Plan, Mode::Plan),
            (pb::AgentMode::Debug, Mode::Debug),
            (pb::AgentMode::Multitask, Mode::Multitask),
        ] {
            assert_eq!(mode_from_proto(mode as i32).unwrap(), expected);
        }
        assert!(mode_from_proto(99).is_err(), "未知数值仍应报协议错误");
    }
}
