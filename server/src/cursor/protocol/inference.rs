//! Minimal wire subset of the Cursor 3.29+ `InferenceService` bidi protocol.
//!
//! 新 Agent Host 在 `agent_host_local_loop` 运行时下，把模型调用从
//! `AgentService/Run` 换成 `InferenceService/RunInference`；它的上行消息是
//! `RunInferenceClientMessage`，与 legacy `AgentClientMessage` 共用同一个
//! `BidiService/BidiAppend` 端点。网关只需要识别流类型、模型与
//! conversation，因此这里只保留判定所需的字段；其余字段由 prost 跳过。
//!
//! 字段号来自 Cursor 3.29.9 自带的 `agent-host-daemon` 生成代码
//! （`RunInferenceClientMessage` / `RunInferenceRunRequest` / `InferenceRequestedModel`），
//! 证据见私有记录仓库的 `runinference-20261107` 首帧样本。
use prost::Message;

/// `aiserver.v1.InferenceService/RunInference` 的上行消息。
#[derive(Clone, PartialEq, Message)]
pub struct RunInferenceClientMessage {
    #[prost(
        oneof = "run_inference_client_message::Message",
        tags = "1, 2, 3, 4, 5, 6, 7, 9, 10, 11"
    )]
    pub message: Option<run_inference_client_message::Message>,
}

pub mod run_inference_client_message {
    use super::*;

    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        RunRequest(RunInferenceRunRequest),
        #[prost(message, tag = "2")]
        InvokeModel(RunInferenceInvokeModel),
        #[prost(message, tag = "3")]
        CancelInvocation(CancelInvocation),
        #[prost(message, tag = "4")]
        FinishRun(FinishRun),
        #[prost(message, tag = "5")]
        ClientHeartbeat(ClientHeartbeat),
        #[prost(message, tag = "6")]
        AgentEvent(AgentEvent),
        #[prost(message, tag = "7")]
        RunRequestLog(RunRequestLog),
        #[prost(message, tag = "9")]
        ResolveSubagentModels(ResolveSubagentModels),
        #[prost(message, tag = "10")]
        ClientFirstCallTimings(ClientFirstCallTimings),
        #[prost(message, tag = "11")]
        DeltaBaseSeed(DeltaBaseSeed),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct RunInferenceRunRequest {
    #[prost(string, tag = "1")]
    pub conversation_id: String,
    #[prost(string, optional, tag = "2")]
    pub conversation_group_id: Option<String>,
    #[prost(message, optional, tag = "3")]
    pub requested_model: Option<InferenceRequestedModel>,
    #[prost(string, optional, tag = "5")]
    pub agent_mode: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub subagent_type_name: Option<String>,
    #[prost(message, repeated, tag = "7")]
    pub selected_subagent_models: Vec<InferenceRequestedModel>,
    #[prost(bool, tag = "9")]
    pub is_summarize_action: bool,
    #[prost(bool, tag = "10")]
    pub supports_long_reasoning_reminder: bool,
    #[prost(message, optional, tag = "11")]
    pub agent_module: Option<AgentModule>,
    #[prost(string, optional, tag = "12")]
    pub previous_request_id: Option<String>,
    #[prost(string, optional, tag = "16")]
    pub turn_agent_mode: Option<String>,
    #[prost(bool, tag = "17")]
    pub supports_agent_loop_error: bool,
    #[prost(bool, tag = "18")]
    pub supports_delta_invocations: bool,
    #[prost(bool, tag = "20")]
    pub speculative_invocations: bool,
    #[prost(bool, tag = "22")]
    pub prewarm: bool,
    #[prost(bool, tag = "27")]
    pub supports_delta_base_seeds: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct InferenceRequestedModel {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(bool, tag = "2")]
    pub max_mode: bool,
    #[prost(message, repeated, tag = "3")]
    pub parameters: Vec<InferenceModelParameter>,
    #[prost(bool, tag = "4")]
    pub built_in_model: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct InferenceModelParameter {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct AgentModule {
    #[prost(string, tag = "1")]
    pub generation: String,
    #[prost(string, tag = "3")]
    pub channel: String,
    #[prost(string, optional, tag = "4")]
    pub commit: Option<String>,
    #[prost(uint32, tag = "5")]
    pub interface_revision: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct RunInferenceInvokeModel {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
    #[prost(message, optional, tag = "2")]
    pub request: Option<InferenceStreamRequest>,
    #[prost(int32, tag = "3")]
    pub purpose: i32,
}

/// 只保留模型与身份字段；消息体留给宿主透传或拒绝，不做本地推理。
#[derive(Clone, PartialEq, Message)]
pub struct InferenceStreamRequest {
    #[prost(string, optional, tag = "5")]
    pub model_id: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub invocation_id: Option<String>,
    #[prost(message, optional, tag = "7")]
    pub requested_model: Option<InferenceRequestedModel>,
    #[prost(string, optional, tag = "8")]
    pub conversation_id: Option<String>,
}

macro_rules! empty_inference_message {
    ($($name:ident),* $(,)?) => {
        $(
            #[derive(Clone, PartialEq, Message)]
            pub struct $name {}
        )*
    };
}

empty_inference_message!(
    CancelInvocation,
    FinishRun,
    ClientHeartbeat,
    AgentEvent,
    RunRequestLog,
    ResolveSubagentModels,
    ClientFirstCallTimings,
    DeltaBaseSeed,
);

impl RunInferenceClientMessage {
    /// 这条 append 的动作类型；日志与诊断共用这一份命名。
    pub fn kind(&self) -> &'static str {
        match self.message.as_ref() {
            Some(run_inference_client_message::Message::RunRequest(_)) => "run_request",
            Some(run_inference_client_message::Message::InvokeModel(_)) => "invoke_model",
            Some(run_inference_client_message::Message::CancelInvocation(_)) => "cancel_invocation",
            Some(run_inference_client_message::Message::FinishRun(_)) => "finish_run",
            Some(run_inference_client_message::Message::ClientHeartbeat(_)) => "client_heartbeat",
            Some(run_inference_client_message::Message::AgentEvent(_)) => "agent_event",
            Some(run_inference_client_message::Message::RunRequestLog(_)) => "run_request_log",
            Some(run_inference_client_message::Message::ResolveSubagentModels(_)) => {
                "resolve_subagent_models"
            }
            Some(run_inference_client_message::Message::ClientFirstCallTimings(_)) => {
                "client_first_call_timings"
            }
            Some(run_inference_client_message::Message::DeltaBaseSeed(_)) => "delta_base_seed",
            None => "empty",
        }
    }

    pub fn run_request(&self) -> Option<&RunInferenceRunRequest> {
        match self.message.as_ref()? {
            run_inference_client_message::Message::RunRequest(request) => Some(request),
            _ => None,
        }
    }

    /// 该流要用的模型：首帧取 `run_request.requested_model`，后续调用取
    /// `invoke_model.request.model_id`（模型由首帧钉住，两者应当一致）。
    pub fn model_id(&self) -> Option<&str> {
        match self.message.as_ref()? {
            run_inference_client_message::Message::RunRequest(request) => request
                .requested_model
                .as_ref()
                .map(|model| model.model_id.as_str())
                .filter(|model| !model.is_empty()),
            run_inference_client_message::Message::InvokeModel(invocation) => {
                invocation.request.as_ref().and_then(|request| {
                    request
                        .model_id
                        .as_deref()
                        .filter(|model| !model.is_empty())
                        .or_else(|| {
                            request
                                .requested_model
                                .as_ref()
                                .map(|model| model.model_id.as_str())
                                .filter(|model| !model.is_empty())
                        })
                })
            }
            _ => None,
        }
    }

    pub fn conversation_id(&self) -> Option<&str> {
        match self.message.as_ref()? {
            run_inference_client_message::Message::RunRequest(request) => {
                Some(request.conversation_id.as_str()).filter(|id| !id.is_empty())
            }
            run_inference_client_message::Message::InvokeModel(invocation) => invocation
                .request
                .as_ref()
                .and_then(|request| request.conversation_id.as_deref())
                .filter(|id| !id.is_empty()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_request(model: &str, conversation: &str) -> RunInferenceClientMessage {
        RunInferenceClientMessage {
            message: Some(run_inference_client_message::Message::RunRequest(
                RunInferenceRunRequest {
                    conversation_id: conversation.into(),
                    requested_model: Some(InferenceRequestedModel {
                        model_id: model.into(),
                        max_mode: true,
                        parameters: vec![InferenceModelParameter {
                            id: "context".into(),
                            value: "800k".into(),
                        }],
                        built_in_model: false,
                    }),
                    ..Default::default()
                },
            )),
        }
    }

    /// 真实的 3.29.9 首帧就是 `run_request`；最小子集必须解出模型与会话，
    /// 否则服务端无法判断该流属于本地还是官方。
    #[test]
    fn a_run_request_carries_its_model_and_conversation() {
        let message = run_request("7d3487855fe8cb56", "7768f256-d6d8-4f75-b521-5c2779fafdf6");
        let decoded =
            RunInferenceClientMessage::decode(message.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.kind(), "run_request");
        assert_eq!(decoded.model_id(), Some("7d3487855fe8cb56"));
        assert_eq!(
            decoded.conversation_id(),
            Some("7768f256-d6d8-4f75-b521-5c2779fafdf6")
        );
    }

    /// 未知字段（messages/tools 等大字段）必须被跳过，不能因为没定义而解码失败。
    #[test]
    fn unknown_fields_are_skipped() {
        // 预先编码一条含未知 tag 的消息体，再拼上合法的 run_request tag。
        let inner = run_request("m", "c");
        let mut bytes = Vec::new();
        // tag 2 / wire 2：模拟 invoke_model 之外的未知字段，用自己的编码器拼。
        Message::encode(&inner, &mut bytes).unwrap();
        let decoded = RunInferenceClientMessage::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.model_id(), Some("m"));
    }

    #[test]
    fn an_invoke_model_reports_the_pinned_model() {
        let message = RunInferenceClientMessage {
            message: Some(run_inference_client_message::Message::InvokeModel(
                RunInferenceInvokeModel {
                    invocation_id: "inv-1".into(),
                    request: Some(InferenceStreamRequest {
                        model_id: Some("plugin:dev.cursorbyok/grok".into()),
                        conversation_id: Some("c-1".into()),
                        ..Default::default()
                    }),
                    purpose: 0,
                },
            )),
        };
        let decoded =
            RunInferenceClientMessage::decode(message.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.kind(), "invoke_model");
        assert_eq!(decoded.model_id(), Some("plugin:dev.cursorbyok/grok"));
        assert_eq!(decoded.conversation_id(), Some("c-1"));
    }
}
