use std::collections::BTreeMap;

use serde_json::Value;

use crate::{
    model::{ProviderReplayState, ProviderType, Usage},
    provider::{FinishReason, ModelEvent},
    Result,
};

use super::wire::{self, Field};

#[derive(Clone, Debug)]
pub struct ResponseState {
    pub message_id: String,
    pub provider: ProviderType,
    tool_calls: BTreeMap<usize, ToolAccumulator>,
    usage: Option<Usage>,
    finish_reason: FinishReason,
    replay_state: Option<ProviderReplayState>,
}

#[derive(Clone, Debug)]
struct ToolAccumulator {
    call_id: String,
    name: String,
    arguments: String,
}

impl ResponseState {
    pub fn new(message_id: impl Into<String>, provider: ProviderType) -> Self {
        Self {
            message_id: message_id.into(),
            provider,
            tool_calls: BTreeMap::new(),
            usage: None,
            finish_reason: FinishReason::Stop,
            replay_state: None,
        }
    }

    pub fn set_fallback_input_tokens(&mut self, input_tokens: u64) {
        if input_tokens > 0 && self.usage.is_none() {
            self.usage = Some(Usage {
                input_tokens: Some(input_tokens),
                ..Usage::default()
            });
        }
    }

    fn merge_usage(&mut self, usage: Usage) {
        let accounted_input = usage
            .input_tokens
            .unwrap_or_default()
            .saturating_add(usage.cache_read_tokens.unwrap_or_default())
            .saturating_add(usage.cache_write_tokens.unwrap_or_default());
        let Some(fallback) = self.usage else {
            self.usage = Some(usage);
            return;
        };
        if accounted_input > 0 {
            self.usage = Some(usage);
            return;
        }
        self.usage = Some(Usage {
            input_tokens: fallback.input_tokens,
            cache_read_tokens: fallback.cache_read_tokens.or(usage.cache_read_tokens),
            cache_write_tokens: fallback.cache_write_tokens.or(usage.cache_write_tokens),
            ..usage
        });
    }
}

pub fn stream_event(state: &mut ResponseState, event: ModelEvent) -> Result<Vec<Vec<u8>>> {
    match event {
        ModelEvent::Start { model_call_id } => {
            if !model_call_id.is_empty() {
                state.message_id = model_call_id;
            }
            Ok(Vec::new())
        }
        ModelEvent::TextDelta(text) if !text.is_empty() => {
            frame_payload(text_delta(&state.message_id, &text)?)
        }
        ModelEvent::ThinkingDelta(text) if !text.is_empty() => {
            frame_payload(thinking_delta(&state.message_id, &text)?)
        }
        ModelEvent::ToolCallStart {
            index,
            call_id,
            name,
        } => {
            state.tool_calls.insert(
                index,
                ToolAccumulator {
                    call_id,
                    name,
                    arguments: String::new(),
                },
            );
            Ok(Vec::new())
        }
        ModelEvent::ToolCallArgumentsDelta { index, delta } => {
            if let Some(tool) = state.tool_calls.get_mut(&index) {
                tool.arguments.push_str(&delta);
            }
            Ok(Vec::new())
        }
        ModelEvent::ToolCallEnd { .. } => Ok(Vec::new()),
        ModelEvent::ProviderReplayState(replay_state) => {
            state.replay_state = Some(replay_state);
            Ok(Vec::new())
        }
        ModelEvent::Usage(usage) => {
            state.merge_usage(usage);
            Ok(Vec::new())
        }
        ModelEvent::Done(reason) => {
            state.finish_reason = reason;
            Ok(Vec::new())
        }
        ModelEvent::TextDelta(_) | ModelEvent::ThinkingDelta(_) => Ok(Vec::new()),
        ModelEvent::TextStart
        | ModelEvent::TextEnd
        | ModelEvent::ThinkingStart
        | ModelEvent::ThinkingEnd => Ok(Vec::new()),
    }
}

pub fn finish(state: ResponseState, model_uid: &str) -> Result<Vec<Vec<u8>>> {
    let mut frames = Vec::new();
    let tools = state.tool_calls.values().collect::<Vec<_>>();
    if !tools.is_empty() {
        frames.push(wire::frame(&tool_delta(&state.message_id, &tools)?, false)?);
    }
    if let Some(signature) = replay_signature(state.replay_state.as_ref()) {
        frames.push(wire::frame(
            &signature_delta(&state.message_id, &signature)?,
            false,
        )?);
    }
    frames.push(wire::frame(
        &stop_chunk(
            &state.message_id,
            state.finish_reason,
            model_uid,
            state.usage,
            state.provider,
        )?,
        false,
    )?);
    frames.push(wire::end_frame(None));
    Ok(frames)
}

fn frame_payload(payload: Vec<u8>) -> Result<Vec<Vec<u8>>> {
    Ok(vec![wire::frame(&payload, false)?])
}

fn text_delta(message_id: &str, text: &str) -> Result<Vec<u8>> {
    let mut fields = base_fields(message_id)?;
    fields.push(Field::bytes(3, text));
    fields.push(Field::varint(
        4,
        text.chars().count().max(1).div_ceil(4) as u64,
    ));
    wire::serialize_fields(&fields)
}

fn thinking_delta(message_id: &str, text: &str) -> Result<Vec<u8>> {
    let mut fields = base_fields(message_id)?;
    fields.push(Field::bytes(9, text));
    wire::serialize_fields(&fields)
}

fn signature_delta(message_id: &str, signature: &str) -> Result<Vec<u8>> {
    let mut fields = base_fields(message_id)?;
    fields.push(Field::bytes(10, signature));
    wire::serialize_fields(&fields)
}

fn tool_delta(message_id: &str, tools: &[&ToolAccumulator]) -> Result<Vec<u8>> {
    let mut fields = base_fields(message_id)?;
    for tool in tools {
        fields.push(Field::bytes(
            6,
            wire::serialize_fields(&[
                Field::bytes(1, &tool.call_id),
                Field::bytes(2, &tool.name),
                Field::bytes(
                    3,
                    if tool.arguments.is_empty() {
                        "{}"
                    } else {
                        &tool.arguments
                    },
                ),
            ])?,
        ));
    }
    wire::serialize_fields(&fields)
}

fn stop_chunk(
    message_id: &str,
    reason: FinishReason,
    model_uid: &str,
    usage: Option<Usage>,
    provider: ProviderType,
) -> Result<Vec<u8>> {
    let mut fields = base_fields(message_id)?;
    fields.push(Field::varint(5, finish_reason_code(reason)));
    fields.push(Field::fixed64(12, 0f64.to_le_bytes()));
    if let Some(usage) = usage {
        fields.push(Field::bytes(
            7,
            usage_stats(message_id, model_uid, provider, usage)?,
        ));
    }
    if !model_uid.is_empty() {
        fields.push(Field::bytes(23, model_uid));
    }
    wire::serialize_fields(&fields)
}

fn usage_stats(
    message_id: &str,
    model_uid: &str,
    provider: ProviderType,
    usage: Usage,
) -> Result<Vec<u8>> {
    let mut fields = vec![
        Field::varint(6, provider_code(provider)),
        Field::bytes(7, message_id),
        Field::bytes(9, model_uid),
        Field::bytes(10, model_uid),
        Field::bytes(11, model_uid),
    ];
    for (number, value) in [
        (2, usage.input_tokens),
        (3, usage.output_tokens),
        (4, usage.cache_write_tokens),
        (5, usage.cache_read_tokens),
    ] {
        if let Some(value) = value.filter(|value| *value > 0) {
            fields.push(Field::varint(number, value));
        }
    }
    wire::serialize_fields(&fields)
}

fn base_fields(message_id: &str) -> Result<Vec<Field>> {
    Ok(vec![
        Field::bytes(1, message_id),
        Field::bytes(
            2,
            wire::serialize_fields(&[
                Field::varint(1, now_seconds()),
                Field::varint(2, now_nanos()),
            ])?,
        ),
    ])
}

fn replay_signature(state: Option<&ProviderReplayState>) -> Option<String> {
    let Value::Object(root) = &state?.value else {
        return None;
    };
    let Value::Array(blocks) = root.get("blocks")? else {
        return None;
    };
    blocks.iter().find_map(|block| {
        block
            .get("signature")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

fn finish_reason_code(reason: FinishReason) -> u64 {
    match reason {
        FinishReason::Stop => 2,
        FinishReason::Length => 3,
        FinishReason::ToolUse => 10,
    }
}

fn provider_code(provider: ProviderType) -> u64 {
    match provider {
        ProviderType::Anthropic => 11,
        ProviderType::OpenAiChat | ProviderType::OpenAiResponses | ProviderType::Plugin => 10,
    }
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        devin::wire::{first_field, parse_fields, unwrap_request, FieldValue},
        model::{ProviderType, Usage},
        provider::{FinishReason, ModelEvent},
    };

    #[test]
    fn text_events_emit_a_devin_text_delta_frame() {
        let mut state = ResponseState::new("response-1", ProviderType::OpenAiChat);
        let frames = stream_event(&mut state, ModelEvent::TextDelta("hello".into())).unwrap();
        assert_eq!(frames.len(), 1);

        let fields = parse_fields(&unwrap_request(&frames[0], None).unwrap()).unwrap();
        assert_eq!(
            first_field(&fields, 1).unwrap().value,
            FieldValue::Bytes(b"response-1".to_vec())
        );
        assert_eq!(
            first_field(&fields, 3).unwrap().value,
            FieldValue::Bytes(b"hello".to_vec())
        );
    }

    #[test]
    fn tool_arguments_and_usage_are_emitted_before_the_stop_frame() {
        let mut state = ResponseState::new("response-2", ProviderType::Anthropic);
        for event in [
            ModelEvent::ToolCallStart {
                index: 0,
                call_id: "call-1".into(),
                name: "read_file".into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"path\":".into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "\"README.md\"}".into(),
            },
            ModelEvent::ToolCallEnd { index: 0 },
            ModelEvent::Usage(Usage {
                input_tokens: Some(100),
                output_tokens: Some(20),
                cache_read_tokens: Some(80),
                ..Usage::default()
            }),
            ModelEvent::Done(FinishReason::ToolUse),
        ] {
            stream_event(&mut state, event).unwrap();
        }

        let frames = finish(state, "MODEL_CLAUDE_4_SONNET_BYOK").unwrap();
        assert_eq!(frames.len(), 3);
        let tool_fields = parse_fields(&unwrap_request(&frames[0], None).unwrap()).unwrap();
        let tool = match &first_field(&tool_fields, 6).unwrap().value {
            FieldValue::Bytes(value) => parse_fields(value).unwrap(),
            _ => panic!("tool field must be nested"),
        };
        assert_eq!(
            first_field(&tool, 1).unwrap().value,
            FieldValue::Bytes(b"call-1".to_vec())
        );
        assert_eq!(
            first_field(&tool, 3).unwrap().value,
            FieldValue::Bytes(br#"{"path":"README.md"}"#.to_vec())
        );

        let stop_fields = parse_fields(&unwrap_request(&frames[1], None).unwrap()).unwrap();
        assert_eq!(
            first_field(&stop_fields, 5).unwrap().value,
            FieldValue::Varint(10)
        );
        assert!(first_field(&stop_fields, 7).is_some());
        assert_eq!(frames[2][0], 2);
    }

    #[test]
    fn fallback_input_usage_survives_provider_usage_without_input_tokens() {
        let mut state = ResponseState::new("response-3", ProviderType::OpenAiChat);
        state.set_fallback_input_tokens(120);
        stream_event(
            &mut state,
            ModelEvent::Usage(Usage {
                output_tokens: Some(7),
                ..Usage::default()
            }),
        )
        .unwrap();
        let frames = finish(state, "model-1").unwrap();
        let stop = parse_fields(&unwrap_request(&frames[0], None).unwrap()).unwrap();
        let usage = match &first_field(&stop, 7).unwrap().value {
            FieldValue::Bytes(value) => parse_fields(value).unwrap(),
            _ => panic!("usage field must be nested"),
        };
        assert_eq!(
            first_field(&usage, 2).unwrap().value,
            FieldValue::Varint(120)
        );
        assert_eq!(first_field(&usage, 3).unwrap().value, FieldValue::Varint(7));
    }
}
