//! Implements Cursor analytics endpoints and event handling.
use axum::{
    body::{Body, Bytes},
    extract::Extension,
    http::{header, HeaderValue, Request, Response, StatusCode},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::{BufMut, BytesMut};
use prost::Message;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::{api::cursor::proxy, Error, Result};

pub const BOOTSTRAP_STATSIG_PATH: &str = "/aiserver.v1.AnalyticsService/BootstrapStatsig";
const AGENT_RETRIES_GATE: &str = "nal_agent_retries";
/// Cursor 3.29+ 打开这个门禁后，Agent Run 改走 `wss://api2.cursor.sh/agent/v1/run`，
/// 不再走本地代理接管的 `RunSSE` + `BidiAppend`。本地 token 拿不到上游门禁，客户端按
/// 默认值（关）走 HTTP；登录官方账号后上游门禁可能为开，聊天就绕过本机网关（issue #8）。
const AGENT_WEBSOCKET_GATE: &str = "nal_websocket_client";
/// Agent Host 总开关。打开后 Cursor 不再用 `RunSSE` 的“服务端跑 Agent 循环”链路，而是
/// 换成客户端本地循环（`agent_host_local_loop`，模型调用走 `InferenceService/RunInference`）
/// 或新的 host 传输。这两条链路本机网关都没有实现：前者要另写一套推理协议，后者要接管
/// WebSocket。网关只实现了 legacy `RunSSE` + `BidiAppend`，所以必须让客户端回到 legacy——
/// 官方的 `AgentRunRequest` 与 `RunInferenceClientMessage` 是两种消息，混在一起只会整轮 400。
const CURSOR_AGENT_HOST_GATE: &str = "cursor_agent_host";
/// 即使 Agent Host 因其他途径（例如 private inference token）被打开，也强制它用
/// “host 传输 + 服务端循环”而不是本地循环；本地循环的 `RunInference` 同样没有实现。
const AGENT_HOST_LOCAL_LOOP_GATE: &str = "agent_host_local_loop";
const LOCAL_RULE: &str = "local_enabled";

#[derive(Clone, PartialEq, Message)]
struct BootstrapStatsigResponse {
    #[prost(string, tag = "1")]
    config: String,
    #[prost(uint64, tag = "2")]
    generated_at_ms: u64,
}

pub async fn bootstrap_statsig(
    Extension(upstream): Extension<proxy::CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    match proxy::forward_buffered(&upstream, request).await {
        Ok(response) if response.status.is_success() => match patch_upstream(response) {
            Ok(response) => Ok(response),
            Err(error) => {
                tracing::warn!(%error, "Cursor Statsig bootstrap was invalid; using local bootstrap");
                local_response()
            }
        },
        Ok(response) => {
            tracing::warn!(status = %response.status, "Cursor Statsig bootstrap was rejected; using local bootstrap");
            local_response()
        }
        Err(error) => {
            tracing::warn!(%error, "Cursor Statsig bootstrap was unavailable; using local bootstrap");
            local_response()
        }
    }
}

fn patch_upstream(response: proxy::BufferedResponse) -> Result<Response<Body>> {
    let (framed, payload) = unary_payload(&response.body)?;
    let mut message = BootstrapStatsigResponse::decode(payload)?;
    let mut config = serde_json::from_str::<Value>(&message.config)?;
    apply_gate_overrides(&mut config)?;
    message.config = serde_json::to_string(&config)?;
    Ok(response.with_body(encode_unary(&message, framed)))
}

fn local_response() -> Result<Response<Body>> {
    let generated_at_ms = chrono::Utc::now().timestamp_millis() as u64;
    let mut config = json!({
        "feature_gates": {},
        "dynamic_configs": {},
        "layer_configs": {},
        "user": {
            "userID": "local_ultra",
            "customIDs": { "localUserID": "local_ultra" }
        },
        "has_updates": true,
        "hash_used": "none",
        "sdkParams": {
            "stableID": "local_ultra",
            "disableDiagnosticsLogging": true
        },
        "time": generated_at_ms
    });
    apply_gate_overrides(&mut config)?;
    let message = BootstrapStatsigResponse {
        config: serde_json::to_string(&config)?,
        generated_at_ms,
    };
    let body = message.encode_to_vec();
    let mut response = Response::new(Body::from(body.clone()));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        body.len()
            .to_string()
            .parse()
            .expect("body length is a valid header value"),
    );
    Ok(response)
}

/// 本地网关需要的门禁取值：打开 agent 重试，关掉 Agent Run 的 WebSocket 传输，
/// 关掉 Agent Host（总开关与本地循环）让客户端留在网关实现了的 legacy `RunSSE` 链路。
fn apply_gate_overrides(config: &mut Value) -> Result<()> {
    set_gate(config, AGENT_RETRIES_GATE, true)?;
    set_gate(config, AGENT_WEBSOCKET_GATE, false)?;
    set_gate(config, CURSOR_AGENT_HOST_GATE, false)?;
    set_gate(config, AGENT_HOST_LOCAL_LOOP_GATE, false)
}

fn set_gate(config: &mut Value, name: &str, value: bool) -> Result<()> {
    let gate_key = statsig_key(config, name);
    let root = config
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Statsig bootstrap config must be an object".into()))?;
    let gates = root
        .entry("feature_gates")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Statsig feature_gates must be an object".into()))?;
    gates.insert(gate_key.clone(), local_gate(&gate_key, value));
    Ok(())
}

fn statsig_key(config: &Value, name: &str) -> String {
    match config.get("hash_used").and_then(Value::as_str) {
        Some("djb2") => djb2(name),
        Some("sha256") => STANDARD.encode(Sha256::digest(name.as_bytes())),
        _ => name.to_owned(),
    }
}

fn djb2(value: &str) -> String {
    value
        .encode_utf16()
        .fold(0_u32, |hash, character| {
            hash.wrapping_mul(31).wrapping_add(u32::from(character))
        })
        .to_string()
}

fn local_gate(name: &str, value: bool) -> Value {
    json!({
        "name": name,
        "value": value,
        "rule_id": LOCAL_RULE,
        "ruleID": LOCAL_RULE,
        "group_name": LOCAL_RULE,
        "groupName": LOCAL_RULE,
        "secondary_exposures": [],
        "secondaryExposures": [],
        "undelegated_secondary_exposures": [],
        "undelegatedSecondaryExposures": [],
        "is_device_based": false,
        "isDeviceBased": false,
        "id_type": "userID",
        "idType": "userID"
    })
}

fn unary_payload(body: &Bytes) -> Result<(bool, &[u8])> {
    if body.len() < 5 {
        return Ok((false, body));
    }
    let flags = body[0];
    let length = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    if length != body.len() - 5 {
        return Ok((false, body));
    }
    if flags != 0 {
        return Err(Error::Protocol(format!(
            "cannot patch compressed or terminal Statsig frame: flags={flags}"
        )));
    }
    Ok((true, &body[5..]))
}

fn encode_unary(message: &impl Message, framed: bool) -> Bytes {
    let payload = message.encode_to_vec();
    if !framed {
        return Bytes::from(payload);
    }
    let mut output = BytesMut::with_capacity(5 + payload.len());
    output.put_u8(0);
    output.put_u32(payload.len() as u32);
    output.extend_from_slice(&payload);
    output.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate<'a>(config: &'a Value, name: &str) -> &'a Value {
        &config["feature_gates"][statsig_key(config, name)]["value"]
    }

    /// 官方账号的上游 bootstrap 打开了 `nal_websocket_client`：补丁后必须是关，
    /// 否则 Agent Run 走 WebSocket 绕过本机网关（issue #8）。三种 key 哈希都要覆盖。
    #[test]
    fn upstream_websocket_gate_is_forced_off_for_every_hash() {
        // Cursor 3.29.9 客户端 Statsig 查门禁：`e[name] ?? e[djb2(name)]`，djb2 为
        // `(t<<5)-t+charCode` 取 32 位后 `String(t>>>0)`；这是它对该名字算出的值。
        assert_eq!(djb2(AGENT_WEBSOCKET_GATE), "579159465");
        for hash in ["none", "djb2", "sha256"] {
            let mut config = json!({"hash_used": hash, "feature_gates": {}});
            let key = statsig_key(&config, AGENT_WEBSOCKET_GATE);
            config["feature_gates"][&key] = json!({"name": key, "value": true});

            apply_gate_overrides(&mut config).unwrap();

            assert_eq!(gate(&config, AGENT_WEBSOCKET_GATE), &json!(false), "{hash}");
            assert_eq!(gate(&config, AGENT_RETRIES_GATE), &json!(true), "{hash}");
            assert_eq!(
                gate(&config, CURSOR_AGENT_HOST_GATE),
                &json!(false),
                "{hash}"
            );
            assert_eq!(
                gate(&config, AGENT_HOST_LOCAL_LOOP_GATE),
                &json!(false),
                "{hash}"
            );
        }
    }

    /// 实际调用 `patch_upstream`：Connect 帧进、帧出，WebSocket 门禁被关，无关门禁原样保留。
    #[tokio::test]
    async fn patch_upstream_disables_the_agent_websocket_and_keeps_other_gates() {
        let unrelated = djb2("some_unrelated_gate");
        let config = json!({
            "hash_used": "djb2",
            "feature_gates": {
                djb2(AGENT_WEBSOCKET_GATE): {"value": true},
                &unrelated: {"name": &unrelated, "value": true}
            }
        });
        let upstream = proxy::BufferedResponse {
            status: StatusCode::OK,
            headers: axum::http::HeaderMap::new(),
            body: encode_unary(
                &BootstrapStatsigResponse {
                    config: config.to_string(),
                    generated_at_ms: 7,
                },
                true,
            ),
        };

        let response = patch_upstream(upstream).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let (framed, payload) = unary_payload(&body).unwrap();
        assert!(framed, "上游是 Connect 帧，补丁后也必须是帧");
        let message = BootstrapStatsigResponse::decode(payload).unwrap();
        assert_eq!(message.generated_at_ms, 7);
        let config: Value = serde_json::from_str(&message.config).unwrap();
        assert_eq!(gate(&config, AGENT_WEBSOCKET_GATE), &json!(false));
        assert_eq!(gate(&config, AGENT_RETRIES_GATE), &json!(true));
        assert_eq!(gate(&config, CURSOR_AGENT_HOST_GATE), &json!(false));
        assert_eq!(gate(&config, AGENT_HOST_LOCAL_LOOP_GATE), &json!(false));
        assert_eq!(
            config["feature_gates"][&unrelated],
            json!({"name": &unrelated, "value": true})
        );
    }

    #[tokio::test]
    async fn local_bootstrap_keeps_the_agent_websocket_off() {
        let response = local_response().unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let message = BootstrapStatsigResponse::decode(body.as_ref()).unwrap();
        let config: Value = serde_json::from_str(&message.config).unwrap();
        assert_eq!(gate(&config, AGENT_WEBSOCKET_GATE), &json!(false));
        assert_eq!(gate(&config, AGENT_RETRIES_GATE), &json!(true));
        assert_eq!(gate(&config, CURSOR_AGENT_HOST_GATE), &json!(false));
        assert_eq!(gate(&config, AGENT_HOST_LOCAL_LOOP_GATE), &json!(false));
    }
}
