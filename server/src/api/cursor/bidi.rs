//! Accepts ordered Cursor Bidi append requests and routes them by request_id.
use prost::Message;

use crate::{
    cursor::{
        conversation::TransportCommand,
        protocol::{
            events, inference,
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

/// Cursor 3.29+ 在 `agent_host_local_loop` 运行时下用 `InferenceService/RunInference`
/// 调模型；它和 legacy Agent 流共用 `BidiService/BidiAppend`，消息类型也不同。
pub struct InferenceAppend {
    pub request_id: String,
    pub seqno: i64,
    pub message: inference::RunInferenceClientMessage,
}

/// 一条 append 属于哪条 bidi 流。服务端必须先分辨类型，再决定本地处理还是透传上游。
pub enum BidiAppendPayload {
    Agent(DecodedAppend),
    Inference(InferenceAppend),
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

/// 一条 append 里承载的消息字节。
///
/// `data_binary` 与 `data` 承载同一份消息：Cursor 客户端取 `message.toBinary()` 的字节，
/// 按开关要么直接放进 `data_binary`，要么 hex 进 `data`（见 cursor-always-local 的
/// BidiTransport）。两条都要接，否则新版客户端一上来就 400。
fn append_payload(request: &ai::BidiAppendRequest) -> Result<Vec<u8>> {
    if !request.data_binary.is_empty() {
        Ok(request.data_binary.clone())
    } else if !request.data.is_empty() {
        hex::decode(&request.data)
            .map_err(|error| Error::Protocol(format!("invalid BidiAppend hex: {error}")))
    } else {
        Err(Error::Protocol(
            "BidiAppend contains no client message".into(),
        ))
    }
}

fn append_identity(request: &ai::BidiAppendRequest) -> Result<(&str, i64)> {
    let request_id = request
        .request_id
        .as_ref()
        .map(|id| id.request_id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Protocol("BidiAppend request_id is required".into()))?;
    Ok((request_id, request.append_seqno))
}

/// 先按 legacy `AgentClientMessage` 解，失败再按新推理消息解。
///
/// 这两种消息共用 `BidiService/BidiAppend`：只按 legacy 解会把新推理流打成每轮 400，
/// 只按新推理解又会破坏本地 Agent 接管。都解不了时留下可诊断的现场（长度、hex 前缀、
/// ASCII 预览），不再只回一句 `invalid wire type`。
pub fn decode_any(request: &ai::BidiAppendRequest) -> Result<BidiAppendPayload> {
    let (request_id, seqno) = append_identity(request)?;
    let payload = append_payload(request)?;
    let agent_error = match agent::AgentClientMessage::decode(payload.as_slice()) {
        Ok(message) => {
            return Ok(BidiAppendPayload::Agent(DecodedAppend {
                request_id: request_id.into(),
                seqno,
                message,
            }))
        }
        Err(error) => error,
    };
    match inference::RunInferenceClientMessage::decode(payload.as_slice()) {
        Ok(message) if message.message.is_some() => {
            return Ok(BidiAppendPayload::Inference(InferenceAppend {
                request_id: request_id.into(),
                seqno,
                message,
            }))
        }
        Ok(_) => tracing::warn!(
            request_id,
            seqno,
            payload_len = payload.len(),
            "BidiAppend payload decoded as an empty RunInferenceClientMessage"
        ),
        Err(inference_error) => {
            tracing::warn!(
                request_id,
                seqno,
                payload_len = payload.len(),
                payload_hex = %hex::encode(&payload[..payload.len().min(64)]),
                payload_ascii = %ascii_preview(&payload[..payload.len().min(64)]),
                agent_error = %agent_error,
                inference_error = %inference_error,
                "BidiAppend payload is neither an AgentClientMessage nor a RunInferenceClientMessage"
            );
            crate::cursor::protocol::guard::record_runtime_mismatch("unknown_append", None, None);
        }
    }
    Err(agent_error.into())
}

fn ascii_preview(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| match byte {
            b' '..=b'~' => *byte as char,
            _ => '.',
        })
        .collect()
}

#[cfg(test)]
fn decode(request: &ai::BidiAppendRequest) -> Result<DecodedAppend> {
    decode_any(request).and_then(|payload| match payload {
        BidiAppendPayload::Agent(decoded) => Ok(decoded),
        BidiAppendPayload::Inference(_) => {
            Err(Error::Protocol("expected a legacy Agent append".into()))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn heartbeat() -> agent::AgentClientMessage {
        agent::AgentClientMessage {
            message: Some(agent::agent_client_message::Message::ClientHeartbeat(
                Default::default(),
            )),
        }
    }

    fn request(data: &str, data_binary: Vec<u8>) -> ai::BidiAppendRequest {
        ai::BidiAppendRequest {
            data: data.into(),
            request_id: Some(ai::BidiRequestId {
                request_id: "run-1".into(),
            }),
            append_seqno: 1,
            data_binary,
        }
    }

    /// 旧客户端把 AgentClientMessage 的字节 hex 进 `data`。
    #[test]
    fn a_hex_append_still_decodes() {
        let hex = hex::encode(heartbeat().encode_to_vec());
        let decoded = decode(&request(&hex, Vec::new())).unwrap();
        assert_eq!(decoded.request_id, "run-1");
    }

    /// 新版客户端改用 `data_binary` 直传同一份字节，不能因为多了这个字段就 400。
    #[test]
    fn a_binary_append_decodes_the_same_bytes() {
        let decoded = decode(&request("", heartbeat().encode_to_vec())).unwrap();
        assert!(matches!(
            decoded.message.message,
            Some(agent::agent_client_message::Message::ClientHeartbeat(_))
        ));
    }

    #[test]
    fn an_empty_append_is_still_rejected() {
        assert!(decode(&request("", Vec::new())).is_err());
    }

    /// 新推理流的首帧（Cursor 3.29.9 managed-local 运行时）不能被当成 legacy Agent：
    /// 服务端要能识别出流类型、模型与会话，才能决定透传上游还是给出明确诊断。
    #[test]
    fn an_inference_append_is_recognized_alongside_the_legacy_stream() {
        let message = inference::RunInferenceClientMessage {
            message: Some(
                inference::run_inference_client_message::Message::RunRequest(
                    inference::RunInferenceRunRequest {
                        conversation_id: "7768f256-d6d8-4f75-b521-5c2779fafdf6".into(),
                        requested_model: Some(inference::InferenceRequestedModel {
                            model_id: "7d3487855fe8cb56".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ),
            ),
        };
        let request = request("", message.encode_to_vec());

        let BidiAppendPayload::Inference(decoded) = decode_any(&request).unwrap() else {
            panic!("inference append must not decode as a legacy Agent append");
        };
        assert_eq!(decoded.message.kind(), "run_request");
        assert_eq!(decoded.message.model_id(), Some("7d3487855fe8cb56"));
        assert_eq!(
            decoded.message.conversation_id(),
            Some("7768f256-d6d8-4f75-b521-5c2779fafdf6")
        );
    }

    /// 两种消息都解不了时必须报错（并留下诊断日志），不能把垃圾字节当合法 append 收下。
    #[test]
    fn an_unrecognized_append_fails_both_decoders() {
        assert!(decode_any(&request("", vec![0xFF; 16])).is_err());
    }
}
