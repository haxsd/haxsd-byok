//! Codex 请求出网与观测；原生 Responses 不经过共享消息投影。
use super::{
    bridge::{translate_request_owned, BridgeStream, UpstreamFormat},
    gateway::{protocol_error, ActiveRequest},
};
use crate::{
    model::{CallClient, ModelConfig, NewLlmCall, ProviderType, Usage},
    network::NetworkClients,
    provider::{CallRecorder, FinishReason, ModelEvent},
    store::Store,
    Error, Result,
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

struct RecordingGuard(CallRecorder);
impl Drop for RecordingGuard {
    fn drop(&mut self) {
        if !self.0.is_finished() {
            let recorder = self.0.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if let Err(error) = recorder.cancelled().await {
                        tracing::warn!(%error, "Codex cancellation record failed");
                    }
                });
            }
        }
    }
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
    match prepare(store, clients, context, active).await {
        Ok(response) => response,
        Err(error) => protocol_error(
            if matches!(error, Error::Protocol(_) | Error::Config(_)) {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            },
            error.to_string(),
        ),
    }
}

async fn prepare(
    store: Store,
    clients: NetworkClients,
    context: RequestContext,
    active: ActiveRequest,
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
    let configured_effort = if model.provider_type() == ProviderType::Anthropic {
        &model.anthropic_thinking_effort
    } else {
        &model.reasoning_effort
    };
    if source.pointer("/reasoning/effort").is_none() {
        if let Some(effort) = configured_effort {
            if source.get("reasoning").is_none_or(Value::is_null) {
                source["reasoning"] = json!({});
            }
            source["reasoning"]
                .as_object_mut()
                .ok_or_else(|| Error::Protocol("reasoning must be an object".into()))?
                .insert("effort".into(), json!(effort));
        }
    }
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
    let max_tokens = model.max_output_tokens().unwrap_or(4096);
    let (mut body, bridge) = if let Some(format) = format {
        let owner = super::catalog::routing_fingerprint(&model)?;
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
    if provider_type == ProviderType::Anthropic
        && body.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled")
    {
        if let Some(budget) = model.thinking_budget_tokens {
            body["thinking"]["budget_tokens"] = json!(budget.max(1024).min(
                body["max_tokens"]
                    .as_u64()
                    .unwrap_or(max_tokens)
                    .saturating_sub(1)
            ));
        }
    }
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
    if format.is_some() {
        body["stream"] = json!(true);
    }
    let url = model.request_url()?;
    let call_id = format!("codex:{}", uuid::Uuid::new_v4());
    let recorder = CallRecorder::start(
        store,
        NewLlmCall {
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
    let guard = RecordingGuard(recorder.clone());
    recorder
        .request(json!({"content-type":"application/json"}), &body)
        .await?;
    let client = clients.provider_client(timeout).await?;
    let mut request = client.post(&url).json(&body);
    if model.custom_headers_enabled {
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
            request = request.header(
                name,
                value
                    .as_str()
                    .ok_or_else(|| Error::Config("header values must be strings".into()))?,
            );
        }
    }
    request = if provider_type == ProviderType::Anthropic {
        request
            .header("x-api-key", &model.api_key)
            .header("anthropic-version", "2023-06-01")
    } else {
        request.bearer_auth(&model.api_key)
    };
    let sent = tokio::select! {
        _ = cancellation.cancelled() => { recorder.cancelled().await?; return Err(Error::Cancelled); }
        response = request.send() => response,
    };
    let upstream = match sent {
        Ok(response) => response,
        Err(error) => {
            let error = Error::Http(error);
            recorder.failed(&error).await?;
            return Err(error);
        }
    };
    recorder
        .response_headers(upstream.status().as_u16())
        .await?;
    if !upstream.status().is_success() {
        let status = upstream.status();
        let bytes =
            record_result(read_body(upstream, idle, &cancellation).await, &recorder).await?;
        let text = String::from_utf8_lossy(&bytes).replace(&model.api_key, "[REDACTED]");
        recorder
            .failed(&Error::Provider(format!("upstream HTTP {status}: {text}")))
            .await?;
        return Ok(protocol_error(
            status,
            format!("upstream HTTP {status}: {text}"),
        ));
    }
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
        let bytes =
            record_result(read_body(upstream, idle, &cancellation).await, &recorder).await?;
        recorder.response_chunk(&bytes).await?;
        let value: Value = record_result(
            serde_json::from_slice(&bytes).map_err(Error::from),
            &recorder,
        )
        .await?;
        record_response(&recorder, &value, provider_type).await?;
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
                Err(_) => { let error = Error::Provider("Codex upstream stream idle timeout".into()); recorder.failed(&error).await?; Err(error)?; unreachable!(); }
            };
            let Some(event) = next else { break; };
            let event = match event {
                Ok(event) => event,
                Err(error) => { let error = Error::Provider(format!("Codex upstream SSE error: {error}")); recorder.failed(&error).await?; Err(error)?; unreachable!(); }
            };
            if event.data == "[DONE]" { break; }
            let mut value: Value = serde_json::from_str(&event.data)?;
            if value["type"] == "error" || value["type"] == "response.failed" || value.get("error").is_some_and(|error| !error.is_null()) {
                redact_value(&mut value, &model.api_key);
            }
            let raw = format!("event: {}\ndata: {}\n\n", event.event, value);
            recorder.response_chunk(raw.as_bytes()).await?;
            let generated = if let Some(bridge) = &mut bridge { bridge.push(&value)? } else { vec![value] };
            for value in generated {
                record_event(&recorder, &value).await?;
                if let Some(response) = terminal_response(&value) {
                    record_response(&recorder, response, provider_type).await?;
                    terminal = true;
                }
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
                        if let Err(record_error) = recorder_on_error.failed(&error).await { tracing::warn!(%record_error, "Codex failure record failed"); }
                        yield Ok(sse_bytes(&json!({"type":"response.failed", "sequence_number":sequence, "response":{"id":failed_id,"object":"response","status":"failed","error":{"message":error.to_string(),"code":"upstream_error"},"output":[]}})));
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

async fn read_body(
    mut response: reqwest::Response,
    idle: Duration,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            _=cancellation.cancelled()=>return Err(Error::Cancelled),
            result=tokio::time::timeout(idle,response.chunk())=>result.map_err(|_|Error::Provider("Codex upstream response body idle timeout".into()))??,
        };
        let Some(chunk) = chunk else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > 64 * 1024 * 1024 {
            return Err(Error::Provider(
                "Codex upstream response body exceeds size limit".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
async fn record_result<T>(result: Result<T>, recorder: &CallRecorder) -> Result<T> {
    if let Err(error) = &result {
        if matches!(error, Error::Cancelled) {
            recorder.cancelled().await?;
        } else {
            recorder.failed(error).await?;
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
