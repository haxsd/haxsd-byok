use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::json;
use uuid::Uuid;

use crate::{
    devin::{wire::FieldValue, DevinModelBinding},
    model::{
        ContentPart, ModelConfig, ModelRequest, ModelSpec, ProjectedContent, ProjectedMessage,
        PromptSpec, ProviderReplayState, Role, ToolCallContent, ToolDefinition, ToolResultContent,
    },
    Error, Result,
};

const SOURCE_USER: u64 = 1;
const SOURCE_ASSISTANT: u64 = 2;
const SOURCE_ASSISTANT_ALT: u64 = 3;
const SOURCE_TOOL: u64 = 4;
const SOURCE_SYSTEM: u64 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextTokenSource {
    Devin,
    Estimate,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DevinChatRequest {
    pub system: String,
    pub history: Vec<ProjectedMessage>,
    pub tools: Vec<ToolDefinition>,
    pub requested_model: String,
    pub cascade_id: String,
    pub prompt_id: String,
    pub execution_id: String,
    pub context_tokens: u64,
    pub context_token_source: ContextTokenSource,
}

pub fn parse_chat_request(payload: &[u8]) -> Result<DevinChatRequest> {
    let fields = super::wire::parse_fields(payload)?;
    let mut system = string_field(&fields, 2)?;
    let mut history = Vec::new();
    let mut declared_context_tokens = 0u64;

    for field in super::wire::all_fields(&fields, 3) {
        let bytes = bytes_value(field)?;
        if let Some(message) = parse_message(bytes, &mut declared_context_tokens)? {
            if message.role == Role::System {
                if !system.is_empty() {
                    system.push('\n');
                }
                if let ProjectedContent::Parts(parts) = message.content {
                    append_text_parts(&mut system, &parts);
                }
            } else {
                history.push(message);
            }
        }
    }
    if history.is_empty() {
        history.push(ProjectedMessage {
            message_id: "devin:continue".into(),
            role: Role::User,
            content: ProjectedContent::Parts(vec![ContentPart::Text {
                text: "Continue.".into(),
            }]),
        });
    }

    let tools = super::wire::all_fields(&fields, 10)
        .map(|field| parse_tool(bytes_value(field)?))
        .collect::<Result<Vec<_>>>()?;
    if super::wire::first_field(&fields, 12).is_some() {
        return Err(Error::Protocol(
            "Devin tool choice is not supported by the shared model request yet".into(),
        ));
    }
    let requested_model = string_field(&fields, 21)?;
    let cascade_id = string_field(&fields, 16)?;
    let prompt_id = string_field(&fields, 17)?;
    let execution_id = string_field(&fields, 22)?;
    let context_token_source = if declared_context_tokens > 0 {
        ContextTokenSource::Devin
    } else {
        ContextTokenSource::Estimate
    };
    let context_tokens = if declared_context_tokens > 0 {
        declared_context_tokens
    } else {
        estimate_context_tokens(&system, &history, &tools)
    };

    Ok(DevinChatRequest {
        system,
        history,
        tools,
        requested_model,
        cascade_id,
        prompt_id,
        execution_id,
        context_tokens,
        context_token_source,
    })
}

pub fn to_invocation(
    request: DevinChatRequest,
    binding: &DevinModelBinding,
    model: &ModelConfig,
) -> Result<crate::model::ModelInvocation> {
    if request.requested_model != binding.model_uid {
        return Err(Error::Protocol(format!(
            "Devin model '{}' is not assigned to this router binding",
            request.requested_model
        )));
    }
    let model_hash = binding.effective_model_hash();
    if model_hash != model.model_hash {
        return Err(Error::Config(format!(
            "Devin binding for '{}' points to unknown cursor-byok model hash {}",
            binding.model_uid, model_hash
        )));
    }

    let call_id = if request.execution_id.is_empty() {
        format!("devin-call:{}", Uuid::new_v4())
    } else {
        format!("devin:{}", request.execution_id)
    };
    let conversation_id = if request.cascade_id.is_empty() {
        call_id.clone()
    } else {
        format!("devin:{}", request.cascade_id)
    };
    let mut model_spec = ModelSpec::new(model.model_hash.clone());
    model_spec.display_name = (!binding.display_name.is_empty())
        .then(|| binding.display_name.clone())
        .or_else(|| Some(model.display_name.clone()));
    model_spec.max_output_tokens = model.max_output_tokens();
    model_spec.context_window_tokens = binding
        .context_window_tokens
        .or(model.context_window_tokens);
    model_spec.reasoning.effort = model.reasoning_effort.clone();

    Ok(crate::model::ModelInvocation {
        call_id,
        run_id: if request.execution_id.is_empty() {
            "devin:anonymous".into()
        } else {
            format!("devin:{}", request.execution_id)
        },
        conversation_id,
        provider_call_index: 0,
        request: ModelRequest {
            prompt: PromptSpec {
                instructions: request.system,
                tools: request.tools,
            },
            model: model_spec,
            history: request.history,
        },
    })
}

fn parse_message(
    bytes: &[u8],
    declared_context_tokens: &mut u64,
) -> Result<Option<ProjectedMessage>> {
    let fields = super::wire::parse_fields(bytes)?;
    let message_id = string_field(&fields, 1)?;
    let source = varint_field(&fields, 2)?.unwrap_or(SOURCE_USER);
    if let Some(tokens) = varint_field(&fields, 4)? {
        *declared_context_tokens = declared_context_tokens.saturating_add(tokens);
    }
    let text = string_field(&fields, 3)?;
    let images = super::wire::all_fields(&fields, 10)
        .map(|field| parse_image(bytes_value(field)?))
        .collect::<Result<Vec<_>>>()?;

    match source {
        SOURCE_SYSTEM | SOURCE_USER => {
            let mut parts = Vec::new();
            parts.extend(images);
            if !text.is_empty() {
                parts.push(ContentPart::Text { text });
            }
            Ok(Some(ProjectedMessage {
                message_id,
                role: if source == SOURCE_SYSTEM {
                    Role::System
                } else {
                    Role::User
                },
                content: ProjectedContent::Parts(parts),
            }))
        }
        SOURCE_ASSISTANT | SOURCE_ASSISTANT_ALT => {
            let calls = super::wire::all_fields(&fields, 6)
                .enumerate()
                .map(|(index, field)| parse_tool_call(bytes_value(field)?, index))
                .collect::<Result<Vec<_>>>()?;
            let thinking = string_field(&fields, 11)?;
            let signature = string_field(&fields, 12)?;
            let replay_state = if thinking.is_empty() || signature.is_empty() {
                None
            } else {
                Some(ProviderReplayState {
                    provider_kind: "anthropic".into(),
                    value: json!({
                        "blocks": [{
                            "type": "thinking",
                            "thinking": thinking,
                            "signature": signature,
                        }]
                    }),
                })
            };
            Ok(Some(ProjectedMessage {
                message_id,
                role: Role::Assistant,
                content: ProjectedContent::Assistant {
                    text,
                    thinking,
                    replay_state,
                    calls,
                },
            }))
        }
        SOURCE_TOOL => Ok(Some(ProjectedMessage {
            message_id,
            role: Role::Tool,
            content: ProjectedContent::ToolResult(ToolResultContent {
                call_id: string_field(&fields, 7)?,
                name: "_".into(),
                content: text,
                is_error: varint_field(&fields, 9)?.unwrap_or_default() != 0,
                image: None,
                provider_parts: images,
            }),
        })),
        other => Err(Error::Protocol(format!(
            "unsupported Devin message source {other}"
        ))),
    }
}

fn parse_tool_call(bytes: &[u8], index: usize) -> Result<ToolCallContent> {
    let fields = super::wire::parse_fields(bytes)?;
    let arguments = string_field(&fields, 3)?;
    Ok(ToolCallContent {
        index,
        call_id: string_field(&fields, 1)?,
        name: string_field(&fields, 2)?,
        arguments: serde_json::from_str(&arguments).unwrap_or_else(|_| json!({})),
    })
}

fn parse_tool(bytes: &[u8]) -> Result<ToolDefinition> {
    let fields = super::wire::parse_fields(bytes)?;
    let parameters = string_field(&fields, 3)?;
    Ok(ToolDefinition {
        name: string_field(&fields, 1)?,
        description: string_field(&fields, 2)?,
        parameters: if parameters.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&parameters)
                .map_err(|error| Error::Protocol(format!("invalid Devin tool schema: {error}")))?
        },
    })
}

fn parse_image(bytes: &[u8]) -> Result<ContentPart> {
    let fields = super::wire::parse_fields(bytes)?;
    let encoded = string_field(&fields, 1)?;
    let mime_type = string_field(&fields, 2)?.if_empty_then("image/png");
    let data = STANDARD
        .decode(encoded)
        .map_err(|error| Error::Protocol(format!("invalid Devin image data: {error}")))?;
    Ok(ContentPart::Image { mime_type, data })
}

fn append_text_parts(output: &mut String, parts: &[ContentPart]) {
    for part in parts {
        if let ContentPart::Text { text } = part {
            output.push_str(text);
        }
    }
}

fn estimate_context_tokens(
    system: &str,
    history: &[ProjectedMessage],
    tools: &[ToolDefinition],
) -> u64 {
    let bytes = serde_json::to_vec(&(system, history, tools))
        .map(|value| value.len() as u64)
        .unwrap_or_default();
    bytes.saturating_add(3) / 4
}

fn bytes_value(field: &super::wire::Field) -> Result<&[u8]> {
    match &field.value {
        FieldValue::Bytes(value) => Ok(value),
        _ => Err(Error::Protocol(format!(
            "Devin field {} must be bytes",
            field.number
        ))),
    }
}

fn string_field(fields: &[super::wire::Field], number: u32) -> Result<String> {
    match super::wire::first_field(fields, number) {
        Some(field) => Ok(String::from_utf8_lossy(bytes_value(field)?).into_owned()),
        None => Ok(String::new()),
    }
}

fn varint_field(fields: &[super::wire::Field], number: u32) -> Result<Option<u64>> {
    super::wire::first_field(fields, number)
        .map(|field| match &field.value {
            FieldValue::Varint(value) => Ok(*value),
            _ => Err(Error::Protocol(format!(
                "Devin field {number} must be a varint"
            ))),
        })
        .transpose()
}

trait EmptyStringExt {
    fn if_empty_then(self, fallback: &str) -> String;
}

impl EmptyStringExt for String {
    fn if_empty_then(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.into()
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        devin::{
            wire::{serialize_fields, Field},
            DevinRoute,
        },
        model::{ModelConfig, ModelType, ProjectedContent, Role, OPENAI_CHAT_ENDPOINT},
    };

    fn model_config(model_hash: &str) -> ModelConfig {
        ModelConfig {
            model_hash: model_hash.into(),
            sort_order: 0,
            display_name: "Cursor Sonnet".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "http://127.0.0.1:9000".into(),
            use_full_url: false,
            api_key: "secret-must-stay-in-store".into(),
            tooltip_data: String::new(),
            model_id: "sonnet".into(),
            reasoning_effort: None,
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: Some(128_000),
            max_completion_tokens: Some(8_192),
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn message(fields: Vec<Field>) -> Field {
        Field::bytes(3, serialize_fields(&fields).unwrap())
    }

    fn fixture() -> Vec<u8> {
        let assistant_tool_call = serialize_fields(&[
            Field::bytes(1, b"call-1"),
            Field::bytes(2, b"read_file"),
            Field::bytes(3, br#"{"path":"README.md"}"#),
        ])
        .unwrap();
        serialize_fields(&[
            Field::bytes(2, b"system instructions"),
            message(vec![
                Field::bytes(1, b"system-1"),
                Field::varint(2, 5),
                Field::bytes(3, b"system message"),
            ]),
            message(vec![
                Field::bytes(1, b"user-1"),
                Field::varint(2, 1),
                Field::bytes(3, b"hello"),
                Field::varint(4, 4),
            ]),
            message(vec![
                Field::bytes(1, b"assistant-1"),
                Field::varint(2, 2),
                Field::bytes(3, b"I will inspect the file."),
                Field::bytes(6, assistant_tool_call),
                Field::bytes(11, b"thinking"),
                Field::bytes(12, b"signature"),
            ]),
            message(vec![
                Field::bytes(1, b"tool-1"),
                Field::varint(2, 4),
                Field::bytes(3, b"file contents"),
                Field::bytes(7, b"call-1"),
                Field::varint(9, 0),
            ]),
            Field::bytes(
                10,
                serialize_fields(&[
                    Field::bytes(1, b"read_file"),
                    Field::bytes(2, b"Read a file"),
                    Field::bytes(3, br#"{"type":"object","properties":{}}"#),
                ])
                .unwrap(),
            ),
            Field::bytes(16, b"cascade-1"),
            Field::bytes(21, b"MODEL_CLAUDE_4_SONNET_BYOK"),
            Field::bytes(22, b"exec-1"),
        ])
        .unwrap()
    }

    #[test]
    fn parses_messages_tools_and_request_identity() {
        let request = parse_chat_request(&fixture()).unwrap();

        assert_eq!(request.system, "system instructions\nsystem message");
        assert_eq!(request.requested_model, "MODEL_CLAUDE_4_SONNET_BYOK");
        assert_eq!(request.cascade_id, "cascade-1");
        assert_eq!(request.execution_id, "exec-1");
        assert_eq!(request.context_tokens, 4);
        assert_eq!(request.tools[0].name, "read_file");
        assert_eq!(request.history.len(), 3);
        assert_eq!(request.history[0].role, Role::User);
        assert!(matches!(
            request.history[1].content,
            ProjectedContent::Assistant { .. }
        ));
        assert!(matches!(
            request.history[2].content,
            ProjectedContent::ToolResult(_)
        ));
    }

    #[test]
    fn maps_request_to_the_existing_model_hash_without_copying_credentials() {
        let request = parse_chat_request(&fixture()).unwrap();
        let binding = DevinModelBinding {
            model_uid: "MODEL_CLAUDE_4_SONNET_BYOK".into(),
            model_hash: "0123abcd".into(),
            display_name: "Devin Sonnet".into(),
            context_window_tokens: Some(200_000),
            enabled: true,
            ..DevinModelBinding::new("", "")
        };
        let model = model_config("0123abcd");

        let invocation = to_invocation(request, &binding, &model).unwrap();

        assert_eq!(invocation.request.model.model_id, "0123abcd");
        assert_eq!(
            invocation.request.model.context_window_tokens,
            Some(200_000)
        );
        assert_eq!(
            invocation.request.prompt.instructions,
            "system instructions\nsystem message"
        );
        assert_eq!(invocation.run_id, "devin:exec-1");
        assert_eq!(invocation.conversation_id, "devin:cascade-1");
        assert!(!serde_json::to_string(&invocation)
            .unwrap()
            .contains("secret-must-stay"));

        let anonymous_request = parse_chat_request(
            &serialize_fields(&[Field::bytes(21, b"MODEL_CLAUDE_4_SONNET_BYOK")]).unwrap(),
        )
        .unwrap();
        let first = to_invocation(anonymous_request.clone(), &binding, &model).unwrap();
        let second = to_invocation(anonymous_request, &binding, &model).unwrap();
        assert_ne!(first.call_id, second.call_id);
    }

    #[test]
    fn routes_invocation_to_the_enabled_active_route_hash() {
        let request = parse_chat_request(&fixture()).unwrap();
        let binding = DevinModelBinding {
            model_uid: "MODEL_CLAUDE_4_SONNET_BYOK".into(),
            model_hash: "legacy-hash".into(),
            enabled: true,
            routes: vec![
                DevinRoute {
                    route_id: "disabled".into(),
                    model_hash: "disabled-hash".into(),
                    label: "Disabled".into(),
                    enabled: false,
                },
                DevinRoute {
                    route_id: "active".into(),
                    model_hash: "active-hash".into(),
                    label: "Active".into(),
                    enabled: true,
                },
            ],
            active_route_id: Some("active".into()),
            ..DevinModelBinding::new("", "")
        };

        let invocation =
            to_invocation(request.clone(), &binding, &model_config("active-hash")).unwrap();
        assert_eq!(invocation.request.model.model_id, "active-hash");
        assert_eq!(binding.model_uid, "MODEL_CLAUDE_4_SONNET_BYOK");
        assert_eq!(binding.model_hash, "legacy-hash");
        assert!(to_invocation(request.clone(), &binding, &model_config("legacy-hash")).is_err());

        let legacy = DevinModelBinding::new("MODEL_CLAUDE_4_SONNET_BYOK", "legacy-hash");
        let legacy_invocation =
            to_invocation(request, &legacy, &model_config("legacy-hash")).unwrap();
        assert_eq!(legacy_invocation.request.model.model_id, "legacy-hash");
    }
}
