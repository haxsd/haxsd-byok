//! Codex 请求出网与观测；原生 Responses 不经过共享消息投影。
use super::{
    bridge::{translate_request_owned, BridgeStream, UpstreamFormat},
    gateway::{protocol_failure, ActiveRequest},
};
use crate::{
    model::{CallClient, ModelConfig, NewLlmCall, ProviderType, Usage},
    network::{NetworkClients, OutboundClient},
    provider::{effort_fallback, CallRecorder, FinishReason, ModelEvent},
    store::Store,
    upstream, Error, Result,
};
use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

/// 自定义头：整表先校验，重试会把同一个请求原样重放，头部构造就不能留在每次尝试里。
/// 无效的头值因此变成发请求之前的配置错误，而不是发送阶段才暴露的传输层错误。
fn custom_headers(model: &ModelConfig) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    if !model.custom_headers_enabled {
        return Ok(headers);
    }
    for (name, value) in model
        .custom_headers
        .as_object()
        .ok_or_else(|| Error::Config("custom headers must be an object".into()))?
    {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| Error::Config(e.to_string()))?;
        if matches!(
            name.as_str(),
            "authorization"
                | "x-api-key"
                | "host"
                | "connection"
                | "content-length"
                | "transfer-encoding"
                | "cookie"
        ) {
            continue;
        }
        let value = value
            .as_str()
            .ok_or_else(|| Error::Config("header values must be strings".into()))?;
        headers.insert(
            name,
            reqwest::header::HeaderValue::from_str(value)
                .map_err(|e| Error::Config(e.to_string()))?,
        );
    }
    Ok(headers)
}

pub(super) struct RequestContext {
    pub model: ModelConfig,
    pub source: Value,
    pub timeout: Duration,
    pub idle: Duration,
    pub cancellation: tokio_util::sync::CancellationToken,
}

pub(super) async fn respond(
    store: Store,
    clients: NetworkClients,
    context: RequestContext,
    active: ActiveRequest,
) -> Response {
    let reference = format!("codex:{}", uuid::Uuid::new_v4());
    match prepare(store, clients, context, active, reference.clone()).await {
        Ok(response) => response,
        Err(error) => {
            let status = if matches!(error, Error::Protocol(_) | Error::Config(_)) {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            let error = error.conversation(&reference);
            tracing::warn!(reference, error = %error.diagnostic_message(), "Codex model request failed");
            protocol_failure(status, &error, None)
        }
    }
}

async fn prepare(
    store: Store,
    clients: NetworkClients,
    context: RequestContext,
    active: ActiveRequest,
    call_id: String,
) -> Result<Response> {
    let RequestContext {
        model,
        mut source,
        timeout,
        idle,
        cancellation,
    } = context;
    if !source.is_object() {
        return Err(Error::Protocol("Codex request must be an object".into()));
    }
    normalize_reasoning(
        &mut source,
        if model.provider_type() == ProviderType::Anthropic {
            model.anthropic_thinking_effort.as_deref()
        } else {
            model.reasoning_effort.as_deref()
        },
    )?;
    if let Some(limit) = model.max_output_tokens() {
        source["max_output_tokens"] = json!(source
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(limit)
            .min(limit));
    }
    let streaming = source
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let provider_type = model.provider_type();
    let format = match provider_type {
        ProviderType::OpenAiChat => Some(UpstreamFormat::Chat),
        ProviderType::Anthropic => Some(UpstreamFormat::Anthropic),
        ProviderType::OpenAiResponses => None,
        ProviderType::Plugin => {
            return Err(Error::Config(
                "Codex gateway requires a configured API model".into(),
            ))
        }
    };
    // 桥接始终读取上游 SSE，非流式客户端只在最终 response 就绪后返回。
    // 模型没配输出上限时不替用户发明一个：Chat 干脆不写这个字段，Anthropic 是必填
    // 字段，用与 provider 路径同一份兜底（见 `DEFAULT_ANTHROPIC_MAX_TOKENS`）。
    let max_tokens = model.max_output_tokens();
    let (mut body, bridge) = if let Some(format) = format {
        let owner = super::catalog::replay_owner(&model)?;
        let translated =
            translate_request_owned(&source, format, &model.model_id, max_tokens, &owner)?;
        (
            translated.body,
            Some(
                BridgeStream::new(format, model.model_id.clone(), translated.tools)
                    .with_replay_owner(owner),
            ),
        )
    } else {
        let mut body = source.clone();
        body.as_object_mut()
            .ok_or_else(|| Error::Protocol("Codex request must be an object".into()))?
            .insert("model".into(), json!(model.model_id));
        (body, None)
    };
    for (key, value) in model
        .extra_params()
        .as_object()
        .ok_or_else(|| Error::Config("model extra params must be an object".into()))?
    {
        if matches!(
            key.as_str(),
            "model"
                | "input"
                | "messages"
                | "tools"
                | "instructions"
                | "system"
                | "stream"
                | "tool_choice"
                | "parallel_tool_calls"
        ) {
            return Err(Error::Config(format!(
                "model extra params cannot replace {key}"
            )));
        }
        body[key] = value.clone();
    }
    // 夹取必须放在额外参数之后：extra params 可以改 `max_tokens`，先夹后改会留下
    // `budget_tokens >= max_tokens` 这种必然 400 的组合。
    if provider_type == ProviderType::Anthropic
        && body.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled")
    {
        if let (Some(configured), Some(max_tokens)) =
            (model.thinking_budget_tokens, body["max_tokens"].as_u64())
        {
            match crate::provider::wire::anthropic::thinking_budget(Some(configured), max_tokens) {
                Some(budget) => body["thinking"]["budget_tokens"] = json!(budget),
                // 装不下合法预算：退回 adaptive，而不是发一个必然被拒的 enabled。
                None => body["thinking"] = json!({"type": "adaptive", "display": "summarized"}),
            }
        }
    }
    if format.is_some() {
        body["stream"] = json!(true);
    }
    let url = model.request_url()?;
    let recorder = CallRecorder::start(
        store,
        NewLlmCall {
            purpose: crate::model::CallPurpose::Chat,
            client_source: CallClient::Codex,
            call_id: call_id.clone(),
            run_id: call_id.clone(),
            conversation_id: call_id,
            provider_call_index: 0,
            model_hash: model.model_hash.clone(),
            provider_type,
            provider_url: url.clone(),
            request_type: provider_type,
            request_url: url.clone(),
            model_id: model.model_id.clone(),
            display_name: model.display_name.clone(),
            reasoning_effort: source
                .pointer("/reasoning/effort")
                .and_then(Value::as_str)
                .map(String::from),
            fast: source
                .get("service_tier")
                .and_then(Value::as_str)
                .is_some_and(|s| matches!(s, "fast" | "priority")),
            message_count: source
                .get("input")
                .and_then(Value::as_array)
                .map_or(1, Vec::len),
            tool_count: source
                .get("tools")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            detailed: false,
        },
    )
    .await?;
    let guard = recorder.cancel_on_drop();
    record_result(
        recorder
            .request(json!({"content-type":"application/json"}), &body)
            .await,
        &recorder,
    )
    .await?;
    let custom_headers = record_result(custom_headers(&model), &recorder).await?;
    let outbound =
        record_result(OutboundClient::provider(clients, timeout).await, &recorder).await?;
    let effort_field = match provider_type {
        ProviderType::OpenAiChat => effort_fallback::EffortField::Chat,
        ProviderType::OpenAiResponses => effort_fallback::EffortField::Responses,
        // The budget-based Anthropic bridge sends no effort string, so adaptation stays inactive.
        ProviderType::Anthropic | ProviderType::Plugin => effort_fallback::EffortField::Anthropic,
    };
    let sent = upstream::send_json(
        &upstream::UpstreamCall {
            label: "Codex Responses",
            model: &model.model_id,
            url: &url,
        },
        &outbound,
        upstream::JsonRequest {
            body: &mut body,
            effort: effort_field,
            timeout,
        },
        |client, body| {
            let request = client.post(&url).json(body).headers(custom_headers.clone());
            if provider_type == ProviderType::Anthropic {
                request
                    .header("x-api-key", model.api_key.as_str())
                    .header("anthropic-version", "2023-06-01")
            } else {
                request.bearer_auth(model.api_key.as_str())
            }
        },
        &cancellation,
        Some(&recorder),
    )
    .await;
    let upstream = match sent {
        Ok(Some(response)) => response,
        Ok(None) => {
            recorder.cancelled().await?;
            return Err(Error::Cancelled);
        }
        Err(error) => {
            let error = error.conversation(&recorder.call_id());
            if let Err(record_error) = recorder.failed(&error).await {
                tracing::warn!(%record_error, "failed to record the model error");
            }
            if let Some(status) = error
                .upstream_status()
                .and_then(|status| StatusCode::from_u16(status).ok())
            {
                return Ok(protocol_failure(status, &error, None));
            }
            return Err(error);
        }
    };
    let is_sse = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| h.contains("text/event-stream"));
    if bridge.is_none() && !is_sse {
        if streaming {
            let error = Error::Provider(
                "upstream ignored the streaming request and returned a non-SSE response".into(),
            );
            recorder.failed(&error).await?;
            return Err(error);
        }
        let bytes = record_result(
            read_body(upstream, idle, &cancellation, SUCCESS_BODY_LIMIT).await,
            &recorder,
        )
        .await?;
        recorder.response_chunk(&bytes).await?;
        let mut value: Value = record_result(
            serde_json::from_slice(&bytes).map_err(|error| {
                Error::Provider(format!("invalid JSON in upstream response: {error}"))
            }),
            &recorder,
        )
        .await?;
        if !normalize_failure_event(&mut value, &recorder, &model.api_key).await {
            record_response(&recorder, &value, provider_type).await?;
        }
        drop(active);
        drop(guard);
        return Ok(Json(value).into_response());
    }
    if !is_sse {
        recorder
            .failed(&Error::Provider(
                "upstream did not return an event stream".into(),
            ))
            .await?;
        return Err(Error::Provider(
            "upstream did not return an event stream".into(),
        ));
    }
    let mut failed_id = bridge
        .as_ref()
        .map(|bridge| bridge.response_id().to_string())
        .unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4().simple()));
    let mut bridge = bridge;
    let mut events = upstream.bytes_stream().eventsource();
    let recorder_on_error = recorder.clone();
    let output = async_stream::try_stream! {
        let _active = active;
        let mut terminal = false;
        loop {
            let waited = tokio::select! {
                _ = cancellation.cancelled() => None,
                result = tokio::time::timeout(idle, events.next()) => Some(result),
            };
            let waited = match waited {
                Some(waited) => waited,
                None => { recorder.cancelled().await?; Err(Error::Cancelled)?; unreachable!(); }
            };
            let next = match waited {
                Ok(next) => next,
                Err(_) => { let error = Error::Timeout("Codex upstream stream idle timeout".into()); recorder.failed(&error).await?; Err(error)?; unreachable!(); }
            };
            let Some(event) = next else { break; };
            let event = match event {
                Ok(event) => event,
                Err(error) => { let error = Error::Provider(format!("Codex upstream SSE error: {error}")); recorder.failed(&error).await?; Err(error)?; unreachable!(); }
            };
            if event.data == "[DONE]" { break; }
            let mut value: Value = serde_json::from_str(&event.data)
                .map_err(|error| Error::Provider(format!("invalid JSON in upstream event: {error}")))?;
            let failed_event = normalize_failure_event(&mut value, &recorder, &model.api_key).await;
            let raw = format!("event: {}\ndata: {}\n\n", event.event, value);
            recorder.response_chunk(raw.as_bytes()).await?;
            let generated = if let Some(bridge) = &mut bridge { bridge.push(&value)? } else { vec![value] };
            for value in generated {
                if !failed_event { record_event(&recorder, &value).await?; }
                if let Some(response) = terminal_response(&value) {
                    if !failed_event { record_response(&recorder, response, provider_type).await?; }
                    terminal = true;
                }
                if failed_event { terminal = true; }
                yield sse_bytes(&value);
            }
            if terminal { break; }
        }
        if !terminal {
            if let Some(bridge) = &mut bridge {
                for value in bridge.finish()? {
                    record_event(&recorder, &value).await?;
                    if let Some(response) = terminal_response(&value) { record_response(&recorder, response, provider_type).await?; terminal = true; }
                    yield sse_bytes(&value);
                }
            }
        }
        if !terminal {
            let error = Error::Provider("Codex upstream stream ended without a terminal response".into());
            recorder.failed(&error).await?;
            Err(error)?;
        }
    };
    if streaming {
        // 把流内错误转为明确的 Responses failed 事件；Body 丢弃仍触发 guard 取消。
        let mut output = Box::pin(output);
        let stream = async_stream::stream! {
            let _guard = guard;
            let mut sequence=0;
            while let Some(item) = output.next().await {
                match item {
                    Ok(bytes) => {
                        if let Ok(raw)=std::str::from_utf8(&bytes) {
                            if let Some(data)=raw.lines().find_map(|line|line.strip_prefix("data: ")) {
                                if let Ok(event)=serde_json::from_str::<Value>(data) {
                                    if let Some(id)=event.pointer("/response/id").and_then(Value::as_str) { failed_id=id.into(); }
                                    if let Some(last)=event.get("sequence_number").and_then(Value::as_u64) { sequence=last.saturating_add(1); }
                                }
                            }
                        }
                        yield Ok::<_, std::convert::Infallible>(bytes);
                    },
                    Err(error) => {
                        let error: Error = error;
                        let error = error.conversation(&recorder_on_error.call_id());
                        if error.code() == crate::error::code::CANCELLED {
                            let _ = recorder_on_error.cancelled().await;
                        } else if let Err(record_error) = recorder_on_error.failed(&error).await {
                            tracing::warn!(error = %record_error.diagnostic_message(), "Codex failure record failed");
                        }
                        yield Ok(sse_bytes(&json!({"type":"response.failed", "sequence_number":sequence, "response":{"id":failed_id,"object":"response","status":"failed","error":{"message":error.public_message(),"code":"upstream_error"},"output":[]}})));
                        break;
                    }
                }
            }
        };
        Ok((
            [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            Body::from_stream(stream),
        )
            .into_response())
    } else {
        let _guard = guard;
        futures_util::pin_mut!(output);
        let mut final_response = None;
        while let Some(bytes) = output.next().await {
            let bytes: bytes::Bytes = match bytes {
                Ok(bytes) => bytes,
                Err(error) => {
                    recorder_on_error.failed(&error).await?;
                    return Err(error);
                }
            };
            let raw = std::str::from_utf8(&bytes).map_err(|e| Error::Protocol(e.to_string()))?;
            if let Some(data) = raw.lines().find_map(|l| l.strip_prefix("data: ")) {
                let value: Value = serde_json::from_str(data)?;
                if let Some(response) = terminal_response(&value) {
                    final_response = Some(response.clone());
                }
            }
        }
        Ok(Json(
            final_response.ok_or_else(|| Error::Provider("missing final Codex response".into()))?,
        )
        .into_response())
    }
}

/// Codex 把它界面里的“思考强度”原样发过来，其中 `none` 之类的值上游不一定接受
/// （例如只认 low…max 的模型会直接 400）。客户端明确选择优先，未指定才用模型默认值；
/// 显式关闭保持关闭，不得被模型默认强度重新开启。
fn normalize_reasoning(source: &mut Value, configured: Option<&str>) -> Result<()> {
    let requested = source
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    let effective = if source.pointer("/reasoning/effort").is_some() {
        requested
    } else {
        configured.map(str::trim).unwrap_or_default().to_owned()
    };
    let object = source
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Codex request must be an object".into()))?;
    if matches!(effective.as_str(), "" | "none" | "off" | "disabled") {
        let mut drop_reasoning = false;
        match object.get_mut("reasoning") {
            Some(Value::Object(reasoning)) => {
                reasoning.remove("effort");
                drop_reasoning = reasoning.is_empty();
            }
            Some(Value::Null) => drop_reasoning = true,
            Some(_) => {
                return Err(Error::Protocol("reasoning must be an object".into()));
            }
            None => {}
        }
        if drop_reasoning {
            object.remove("reasoning");
        }
        return Ok(());
    }
    match object.get_mut("reasoning") {
        Some(Value::Object(reasoning)) => {
            reasoning.insert("effort".into(), json!(effective));
        }
        Some(value) if value.is_null() => *value = json!({"effort": effective}),
        Some(_) => return Err(Error::Protocol("reasoning must be an object".into())),
        None => {
            object.insert("reasoning".into(), json!({"effort": effective}));
        }
    }
    Ok(())
}

/// Keep the provider's response identity and envelope, but replace externally controlled error text.
async fn normalize_failure_event(value: &mut Value, recorder: &CallRecorder, secret: &str) -> bool {
    let failed = matches!(
        value.get("type").and_then(Value::as_str),
        Some("error" | "response.failed")
    ) || value.get("error").is_some_and(|error| !error.is_null())
        || value.get("status").and_then(Value::as_str) == Some("failed")
        || value.pointer("/response/status").and_then(Value::as_str) == Some("failed");
    if !failed {
        return false;
    }
    redact_value(value, secret);
    let detail = value
        .pointer("/response/error")
        .or_else(|| value.get("error"))
        .cloned()
        .unwrap_or_else(|| json!({"message":value.get("message"),"code":value.get("code")}));
    let error = Error::Provider(format!("upstream stream rejected the request: {detail}"))
        .conversation(&recorder.call_id());
    if let Err(record_error) = recorder.failed(&error).await {
        tracing::warn!(error = %record_error.diagnostic_message(), "failed to record the upstream event error");
    }
    let summary = error.public_message();
    let mut report = json!({"code":"upstream_error","message":summary});
    // Preserve protocol error categories, never externally supplied prose in those fields.
    for field in ["code", "type"] {
        if let Some(text) = detail.get(field).and_then(Value::as_str).filter(|text| {
            text.len() <= 64
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }) {
            report[field] = json!(text);
        }
    }
    if let Some(response) = value.get_mut("response").filter(|value| value.is_object()) {
        response["error"] = report;
    } else if value.get("error").is_some()
        || value.get("status").and_then(Value::as_str) == Some("failed")
    {
        value["error"] = report;
    }
    if value.get("message").is_some() || value["type"] == "error" {
        value["message"] = json!(summary);
    }
    true
}

fn redact_value(value: &mut Value, secret: &str) {
    if secret.is_empty() {
        return;
    }
    match value {
        Value::String(text) => *text = text.replace(secret, "[REDACTED]"),
        Value::Array(items) => items.iter_mut().for_each(|item| redact_value(item, secret)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|item| redact_value(item, secret)),
        _ => {}
    }
}

/// Success bodies must be complete to parse; rejected bodies are bounded in the shared sender.
const SUCCESS_BODY_LIMIT: usize = 64 * 1024 * 1024;

fn take_chunk(body: &mut Vec<u8>, chunk: &[u8], limit: usize) -> Result<()> {
    if body.len().saturating_add(chunk.len()) > limit {
        return Err(Error::Provider(
            "Codex upstream response body exceeds size limit".into(),
        ));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

async fn read_body(
    mut response: reqwest::Response,
    idle: Duration,
    cancellation: &tokio_util::sync::CancellationToken,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            _=cancellation.cancelled()=>return Err(Error::Cancelled),
            result=tokio::time::timeout(idle,response.chunk())=>result.map_err(|_|Error::Timeout("Codex upstream response body idle timeout".into()))??,
        };
        let Some(chunk) = chunk else {
            break;
        };
        take_chunk(&mut body, &chunk, limit)?;
    }
    Ok(body)
}
async fn record_result<T>(result: Result<T>, recorder: &CallRecorder) -> Result<T> {
    if let Err(error) = &result {
        let recorded = if matches!(error, Error::Cancelled) {
            recorder.cancelled().await
        } else {
            recorder.failed(error).await
        };
        if let Err(record_error) = recorded {
            tracing::warn!(error = %record_error.diagnostic_message(), "failed to record the original Codex error");
        }
    }
    result
}

fn terminal_response(event: &Value) -> Option<&Value> {
    if matches!(
        event.get("type").and_then(Value::as_str),
        Some("response.completed" | "response.incomplete" | "response.failed")
    ) {
        event.get("response")
    } else {
        None
    }
}
fn sse_bytes(value: &Value) -> bytes::Bytes {
    format!(
        "event: {}\ndata: {}\n\n",
        value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message"),
        value
    )
    .into()
}
async fn record_event(recorder: &CallRecorder, event: &Value) -> Result<()> {
    match event.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => {
            recorder
                .event(&ModelEvent::TextDelta(
                    event
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                ))
                .await?
        }
        Some(
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta",
        ) => {
            recorder
                .event(&ModelEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: String::new(),
                })
                .await?
        }
        Some("error") => {
            let error = Error::Provider(event.to_string());
            recorder.failed(&error).await?;
            return Err(error);
        }
        _ => {}
    }
    Ok(())
}
async fn record_response(
    recorder: &CallRecorder,
    response: &Value,
    provider: ProviderType,
) -> Result<()> {
    let usage = response.get("usage").unwrap_or(&Value::Null);
    let context_input = usage.get("input_tokens").and_then(Value::as_u64);
    let fresh_input = if provider == ProviderType::Anthropic {
        context_input.map(|input| {
            input
                .saturating_sub(
                    usage
                        .pointer("/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                )
                .saturating_sub(
                    usage
                        .pointer("/input_tokens_details/cache_creation_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                )
        })
    } else {
        context_input
    };
    recorder
        .usage(Usage {
            input_tokens: fresh_input,
            context_input_tokens: context_input,
            output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
            total_tokens: usage.get("total_tokens").and_then(Value::as_u64),
            cache_read_tokens: usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64),
            cache_write_tokens: usage
                .pointer("/input_tokens_details/cache_creation_tokens")
                .and_then(Value::as_u64),
            reasoning_tokens: usage
                .pointer("/output_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64),
        })
        .await?;
    match response.get("status").and_then(Value::as_str) {
        Some("completed") => {
            let tool = response
                .get("output")
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter().any(|v| {
                        matches!(
                            v.get("type").and_then(Value::as_str),
                            Some("function_call" | "custom_tool_call")
                        )
                    })
                });
            recorder
                .completed(if tool {
                    FinishReason::ToolUse
                } else {
                    FinishReason::Stop
                })
                .await
        }
        Some("incomplete") => recorder.completed(FinishReason::Length).await,
        _ => {
            let error = Error::Provider(format!(
                "Codex upstream returned no completed response: {response}"
            ));
            recorder.failed(&error).await?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模型没有配置强度时，客户端界面里的占位值不进上游请求。
    #[test]
    fn a_placeholder_effort_is_never_sent_upstream() {
        for placeholder in ["none", "off", "disabled", "", "  "] {
            let mut source = json!({"input": "go", "reasoning": {"effort": placeholder}});
            normalize_reasoning(&mut source, None).unwrap();
            assert_eq!(source.get("reasoning"), None, "effort {placeholder:?}");
        }
    }

    /// 摘占位值只动 effort：同一 reasoning 对象里的其他字段继续转发。
    #[test]
    fn other_reasoning_fields_survive_placeholder_removal() {
        let mut source = json!({"reasoning": {"effort": "none", "summary": "auto"}});
        normalize_reasoning(&mut source, None).unwrap();
        assert_eq!(source["reasoning"], json!({"summary": "auto"}));
    }

    /// 显式客户端强度优先，只有未指定才使用模型默认值。
    #[test]
    fn the_client_effort_wins_over_the_configured_default() {
        let mut source = json!({"reasoning": {"effort": "low"}});
        normalize_reasoning(&mut source, Some(" high ")).unwrap();
        assert_eq!(source["reasoning"]["effort"], "low");
        let mut source = json!({"input": "go"});
        normalize_reasoning(&mut source, Some("high")).unwrap();
        assert_eq!(source["reasoning"]["effort"], "high");
    }

    /// 没有配置强度时保留客户端可用的强度。
    #[test]
    fn the_client_effort_is_kept_when_no_effort_is_configured() {
        let mut source = json!({"reasoning": {"effort": " medium "}});
        normalize_reasoning(&mut source, Some("")).unwrap();
        assert_eq!(source["reasoning"]["effort"], "medium");
    }

    /// 畸形 reasoning 明确拒绝，而不是把上游必然拒绝的正文转出去。
    #[test]
    fn a_malformed_reasoning_field_is_rejected() {
        let mut source = json!({"reasoning": "none"});
        assert!(normalize_reasoning(&mut source, None).is_err());
        let mut source = json!({"reasoning": "low"});
        assert!(normalize_reasoning(&mut source, Some("high")).is_err());
    }

    #[test]
    fn oversized_success_bodies_are_refused_not_silently_truncated() {
        let mut body = b"1234".to_vec();
        assert!(take_chunk(&mut body, b"56", 5).is_err());
        assert_eq!(body, b"1234");
        take_chunk(&mut body, b"5", 5).unwrap();
        assert_eq!(body, b"12345");
    }
}
