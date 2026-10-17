//! 跨轮推理状态在 Responses `reasoning.encrypted_content` 里的归属封装。
//!
//! base64 只是编码，不是加密声明：它记录签发协议与模型，防止把别家模型的密文或
//! 别家协议的推理状态回放给当前上游。不认识、不是这里签发的密文一律降级为"没有
//! 可回放状态"（调用方退回可见摘要），只有我们自己写坏的结构才报错——一次配置
//! 改动不该让整轮对话 400。
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

pub(super) fn encode(format: UpstreamFormat, owner: &str, state: &Value) -> Result<String> {
    let payload = json!({"protocol": protocol(format), "owner": owner, "state": state});
    Ok(format!(
        "{PREFIX}{}",
        STANDARD.encode(serde_json::to_vec(&payload)?)
    ))
}

/// 解码 Codex 回放的推理状态；`Ok(None)` 表示没有可回放的状态。
///
/// 三种情况都算"没有"：没有密文、不是这里签发的密文（官方模型或别的工具的
/// `encrypted_content`）、签发者不是当前上游 + 模型 + 协议。它们都不是错误，回放只是
/// 可选项：退回可见摘要比整轮 400 好。结构坏掉（base64、JSON、缺 state）才是
/// 我们自己的数据被改坏，照旧报错。
pub(super) fn decode(item: &Value, format: UpstreamFormat, owner: &str) -> Result<Option<Value>> {
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
    let Some(encoded) = raw.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    let bytes = STANDARD.decode(encoded).map_err(|_| {
        Error::Protocol("Codex reasoning state is not decodable; start a new Codex conversation with this model".into())
    })?;
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| {
        Error::Protocol("Codex reasoning state is not valid JSON; start a new Codex conversation with this model".into())
    })?;
    if payload.get("protocol").and_then(Value::as_str) != Some(protocol(format))
        || payload.get("owner").and_then(Value::as_str) != Some(owner)
    {
        return Ok(None);
    }
    payload
        .get("state")
        .cloned()
        .map(Some)
        .ok_or_else(|| Error::Protocol("Codex reasoning state carries no payload".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(encrypted: &str) -> Value {
        json!({"type": "reasoning", "encrypted_content": encrypted})
    }

    #[test]
    fn round_trips_the_state_that_was_issued_for_this_model_and_protocol() {
        let owner = "owner-1";
        let encoded = encode(UpstreamFormat::Chat, owner, &json!("thought")).unwrap();
        assert_eq!(
            decode(&item(&encoded), UpstreamFormat::Chat, owner).unwrap(),
            Some(json!("thought"))
        );
    }

    /// 密文属于别的上游/模型/协议时不再是错误：这轮照常发出去，只是没有可回放的
    /// 推理状态（调用方退回可见摘要 / 不冒充 thinking 块）。
    #[test]
    fn state_issued_for_another_owner_or_protocol_degrades_instead_of_failing() {
        let encoded = encode(UpstreamFormat::Chat, "other-owner", &json!("thought")).unwrap();
        assert_eq!(
            decode(&item(&encoded), UpstreamFormat::Chat, "owner-1").unwrap(),
            None
        );
        assert_eq!(
            decode(&item(&encoded), UpstreamFormat::Anthropic, "other-owner").unwrap(),
            None
        );
    }

    /// 反例（复核 13-codex/P1-1）：同一份密文的 owner 里带的是签发它的上游 origin，
    /// 换一台同协议的主机（origin 不同、model_hash 相同）后不能回放给新上游。
    #[test]
    fn state_from_another_upstream_origin_is_not_replayed() {
        let encoded = encode(UpstreamFormat::Chat, "origin-a:model-x", &json!("thought")).unwrap();
        assert_eq!(
            decode(&item(&encoded), UpstreamFormat::Chat, "origin-a:model-x").unwrap(),
            Some(json!("thought")),
            "同一个上游签发的状态可以回放"
        );
        assert_eq!(
            decode(&item(&encoded), UpstreamFormat::Chat, "origin-b:model-x").unwrap(),
            None,
            "换上游 origin 后旧密文必须失效"
        );
    }

    /// 官方模型或别的工具写的 `encrypted_content` 不是我们签发的：一样降级。
    #[test]
    fn foreign_ciphertext_degrades_instead_of_failing() {
        assert_eq!(
            decode(
                &item("gAAAAABm-vendor-ciphertext"),
                UpstreamFormat::Chat,
                "o"
            )
            .unwrap(),
            None
        );
    }

    /// 结构坏掉的是我们自己的数据被改坏，必须报出来而不是静默丢弃。
    #[test]
    fn structurally_broken_state_is_still_an_error() {
        let bad_base64 = format!("{PREFIX}not base64!!");
        assert!(decode(&item(&bad_base64), UpstreamFormat::Chat, "o").is_err());
        let bad_json = format!("{PREFIX}{}", STANDARD.encode(b"{"));
        assert!(decode(&item(&bad_json), UpstreamFormat::Chat, "o").is_err());
        // 结构与归属都对，只是没有 state：编码方写坏了，不能当成"没有状态"。
        let no_state = format!(
            "{PREFIX}{}",
            STANDARD.encode(br#"{"protocol":"chat","owner":"o"}"#)
        );
        assert!(decode(&item(&no_state), UpstreamFormat::Chat, "o").is_err());
    }

    /// 没有密文与超长密文：前者降级，后者是结构性拒绝。
    #[test]
    fn missing_ciphertext_degrades_and_oversized_state_is_refused() {
        assert_eq!(
            decode(&json!({"type": "reasoning"}), UpstreamFormat::Chat, "o").unwrap(),
            None
        );
        let oversized = format!("{PREFIX}{}", "A".repeat(MAX_ENCODED + 1));
        assert!(decode(&item(&oversized), UpstreamFormat::Chat, "o").is_err());
    }
}
