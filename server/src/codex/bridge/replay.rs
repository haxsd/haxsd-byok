//! 跨轮推理状态在 Responses `reasoning.encrypted_content` 里的归属封装。
//!
//! base64 只是编码，不是加密声明：它记录签发协议与模型，防止把别家模型的密文或
//! 别家协议的推理状态回放给当前上游。未知密文一律明确拒绝，由调用方决定如何提示用户。
use super::UpstreamFormat;
use crate::{Error, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

const PREFIX: &str = "haxsd-codex-replay-v1:";
const MAX_ENCODED: usize = 4 * 1024 * 1024;

fn protocol(format: UpstreamFormat) -> &'static str {
    match format {
        UpstreamFormat::Chat => "chat",
        UpstreamFormat::Anthropic => "anthropic",
    }
}

pub(super) fn encode(format: UpstreamFormat, model: &str, state: &Value) -> Result<String> {
    let payload = json!({"protocol": protocol(format), "model": model, "state": state});
    Ok(format!(
        "{PREFIX}{}",
        STANDARD.encode(serde_json::to_vec(&payload)?)
    ))
}

/// 解码 Codex 回放的推理状态；`Ok(None)` 表示没有可回放的状态（只有可见摘要）。
pub(super) fn decode(item: &Value, format: UpstreamFormat, model: &str) -> Result<Option<Value>> {
    let Some(raw) = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    else {
        return Ok(None);
    };
    if raw.len() > MAX_ENCODED {
        return Err(Error::Protocol(
            "Codex reasoning state is too large to replay".into(),
        ));
    }
    let encoded = raw.strip_prefix(PREFIX).ok_or_else(|| {
        Error::Protocol(
            "Codex reasoning state comes from another provider and cannot be replayed here; start a new Codex conversation with this model"
                .into(),
        )
    })?;
    let bytes = STANDARD.decode(encoded).map_err(|_| {
        Error::Protocol("Codex reasoning state is not decodable; start a new Codex conversation with this model".into())
    })?;
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| {
        Error::Protocol("Codex reasoning state is not valid JSON; start a new Codex conversation with this model".into())
    })?;
    if payload.get("protocol").and_then(Value::as_str) != Some(protocol(format))
        || payload.get("model").and_then(Value::as_str) != Some(model)
    {
        return Err(Error::Protocol(
            "Codex reasoning state was issued for another model or protocol; start a new Codex conversation with this model"
                .into(),
        ));
    }
    payload
        .get("state")
        .cloned()
        .map(Some)
        .ok_or_else(|| Error::Protocol("Codex reasoning state carries no payload".into()))
}
