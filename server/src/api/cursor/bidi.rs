//! Accepts ordered Cursor Bidi append requests and routes them by request_id.
use prost::Message;

use crate::{
    cursor::{
        conversation::TransportCommand,
        protocol::{
            events,
            proto::{agent::v1 as agent, aiserver::v1 as ai},
        },
        transport::{TransportParent, TransportRegistry},
    },
    Error, Result,
};

pub struct DecodedAppend {
    pub request_id: String,
    pub seqno: i64,
    pub message: agent::AgentClientMessage,
}

impl DecodedAppend {
    pub fn model_id(&self) -> Option<&str> {
        let agent::agent_client_message::Message::RunRequest(request) =
            self.message.message.as_ref()?
        else {
            return None;
        };
        request
            .requested_model
            .as_ref()
            .map(|model| model.model_id.as_str())
            .filter(|model| !model.is_empty())
            .or_else(|| {
                request
                    .model_details
                    .as_ref()
                    .map(|model| model.model_id.as_str())
                    .filter(|model| !model.is_empty())
            })
    }

    pub fn conversation_id(&self) -> Option<&str> {
        let agent::agent_client_message::Message::RunRequest(request) =
            self.message.message.as_ref()?
        else {
            return None;
        };
        request.conversation_id.as_deref()
    }

    /// 这条 append 的动作类型：RunRequest 的动作按协议名给出（snake_case），其它消息
    /// 给出消息类型。
    ///
    /// 日志与追踪元数据共用这一份命名，免得两处各写一遍再慢慢分叉；`match` 是穷尽的，
    /// 协议新增动作时编译器会提醒我们补上。
    pub fn action_kind(&self) -> &'static str {
        let Some(message) = self.message.message.as_ref() else {
            return "empty";
        };
        let agent::agent_client_message::Message::RunRequest(request) = message else {
            return client_message_type(message);
        };
        let Some(action) = request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref())
        else {
            return "none";
        };
        match action {
            agent::conversation_action::Action::UserMessageAction(_) => "user_message",
            agent::conversation_action::Action::ResumeAction(_) => "resume",
            agent::conversation_action::Action::CancelAction(_) => "cancel",
            agent::conversation_action::Action::SummarizeAction(_) => "summarize",
            agent::conversation_action::Action::ShellCommandAction(_) => "shell_command",
            agent::conversation_action::Action::StartPlanAction(_) => "start_plan",
            agent::conversation_action::Action::ExecutePlanAction(_) => "execute_plan",
            agent::conversation_action::Action::AsyncAskQuestionCompletionAction(_) => {
                "async_ask_question_completion"
            }
            agent::conversation_action::Action::CancelSubagentAction(_) => "cancel_subagent",
            agent::conversation_action::Action::BackgroundTaskCompletionAction(_) => {
                "background_task_completion"
            }
            agent::conversation_action::Action::BackgroundShellAction(_) => "background_shell",
            agent::conversation_action::Action::BackgroundSubagentAction(_) => {
                "background_subagent"
            }
            agent::conversation_action::Action::SubscriptionNotificationAction(_) => {
                "subscription_notification"
            }
            agent::conversation_action::Action::GoalContinuationAction(_) => "goal_continuation",
            agent::conversation_action::Action::InjectContextAction(_) => "inject_context",
        }
    }

    /// 高频、且与子代理唤醒链路无关的 append：工具结果与其控制消息、KV 同步、
    /// 心跳、预热、以及用户轮（用户轮已经有「routing Cursor Run to ...」那条日志）。
    /// 这些不值得再单独记一条 INFO；其余动作（后台任务、取消、计划、总结……）都要。
    pub fn is_routine(&self) -> bool {
        matches!(
            self.action_kind(),
            "empty"
                | "user_message"
                | "exec_client_message"
                | "exec_client_control_message"
                | "kv_client_message"
                | "client_heartbeat"
                | "prewarm_request"
        )
    }

    pub fn trace_metadata(&self) -> serde_json::Value {
        let Some(message) = self.message.message.as_ref() else {
            return serde_json::json!({
                "append_seqno": self.seqno,
                "message_type": "empty",
            });
        };
        let agent::agent_client_message::Message::RunRequest(request) = message else {
            return serde_json::json!({
                "append_seqno": self.seqno,
                "message_type": client_message_type(message),
            });
        };
        let (action_type, history_messages, history_images) = request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref())
            .map(|action| match action {
                agent::conversation_action::Action::UserMessageAction(action) => {
                    let history = action.conversation_history.as_ref();
                    (
                        "user_message",
                        history.map_or(0, |history| history.messages.len()),
                        history.map_or(0, history_image_count),
                    )
                }
                _ => (self.action_kind(), 0, 0),
            })
            .unwrap_or(("none", 0, 0));
        let state = request.conversation_state.as_ref();
        serde_json::json!({
            "append_seqno": self.seqno,
            "message_type": "run_request",
            "conversation_id": request.conversation_id,
            "model_id": self.model_id(),
            "action_type": action_type,
            "conversation_history_messages": history_messages,
            "conversation_history_images": history_images,
            "root_message_count": state.map_or(0, |state| state.root_prompt_messages_json.len()),
            "turn_count": state.map_or(0, |state| state.turns.len()),
            "prefetched_blob_count": request.pre_fetched_blobs.len(),
        })
    }
}

fn client_message_type(message: &agent::agent_client_message::Message) -> &'static str {
    use agent::agent_client_message::Message;
    match message {
        Message::RunRequest(_) => "run_request",
        Message::ExecClientMessage(_) => "exec_client_message",
        Message::ExecClientControlMessage(_) => "exec_client_control_message",
        Message::KvClientMessage(_) => "kv_client_message",
        Message::ConversationAction(_) => "conversation_action",
        Message::InteractionResponse(_) => "interaction_response",
        Message::ClientHeartbeat(_) => "client_heartbeat",
        Message::PrewarmRequest(_) => "prewarm_request",
    }
}

fn history_image_count(history: &agent::ConversationHistory) -> usize {
    use agent::{
        conversation_history_message::Message,
        conversation_history_tool_result_content::Content as ToolContent,
        conversation_history_user_content::Content as UserContent,
    };
    history
        .messages
        .iter()
        .map(|message| match message.message.as_ref() {
            Some(Message::User(user)) => user
                .content
                .iter()
                .filter(|content| matches!(content.content, Some(UserContent::Image(_))))
                .count(),
            Some(Message::Tool(tool)) => tool
                .content
                .iter()
                .filter(|content| matches!(content.content, Some(ToolContent::Image(_))))
                .count(),
            _ => 0,
        })
        .sum()
}

pub fn decode(request: &ai::BidiAppendRequest) -> Result<DecodedAppend> {
    let request_id = request
        .request_id
        .as_ref()
        .map(|id| id.request_id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Protocol("BidiAppend request_id is required".into()))?;
    if !request.data_binary.is_empty() {
        return Err(Error::Protocol(
            "BidiAppend data_binary is not part of the captured protocol".into(),
        ));
    }
    if request.data.is_empty() {
        return Err(Error::Protocol(
            "BidiAppend contains no AgentClientMessage".into(),
        ));
    }
    let payload = hex::decode(&request.data)
        .map_err(|error| Error::Protocol(format!("invalid BidiAppend hex: {error}")))?;
    Ok(DecodedAppend {
        request_id: request_id.into(),
        seqno: request.append_seqno,
        message: agent::AgentClientMessage::decode(payload.as_slice())?,
    })
}

pub async fn append(
    registry: &TransportRegistry,
    request: DecodedAppend,
    parent: Option<TransportParent>,
) -> Result<ai::BidiAppendResponse> {
    let replace_closing = request.model_id().is_some();
    let handle = registry
        .get_or_create_for_append(&request.request_id, replace_closing)
        .await?;
    let _admission = handle.admit()?;
    if let Some(conversation_id) = request.conversation_id() {
        handle.set_conversation_id(conversation_id)?;
    }
    if let Some(parent) = parent {
        handle.set_parent(parent)?;
    }
    if matches!(
        request.message.message.as_ref(),
        Some(agent::agent_client_message::Message::ClientHeartbeat(_))
    ) {
        handle.emit(&events::heartbeat())?;
    }
    handle
        .command(TransportCommand::Append {
            seqno: request.seqno,
            message: Box::new(request.message),
        })
        .await?;
    Ok(ai::BidiAppendResponse {})
}
