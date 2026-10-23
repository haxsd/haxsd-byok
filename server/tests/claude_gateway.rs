//! Claude Code 入站网关的端到端行为：Anthropic SSE 形状与调用归属。
//!
//! 两条路径分开验证：
//! - 用共享的 `fake_provider`（一个 `Provider` 实现）确定性地检查 SSE 事件序列；
//! - 用真实的 `ProviderRouter` + 本地上游，确认生产链路上的 `llm_calls` 归属是
//!   `claude`、用量与耗时都落了库。`FakeProvider` 不经过路由器，所以记录只能在
//!   后一条路径上断言。
#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::{
    net::TcpListener,
    sync::{Arc, Once},
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use cursor_server::{
    anthropic::{ClaudeGateway, ClaudeSettings},
    model::{ModelConfigInput, ModelType, Usage},
    network::NetworkClients,
    plugin::{PluginRegistry, PluginRuntime},
    provider::{FinishReason, ModelEvent, ProviderRouter},
    store::Store,
};
use serde_json::{json, Value};
use tower::ServiceExt;

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

async fn claude_model(store: &Store, base_url: String, full_url: bool) -> String {
    store
        .create_model(&ModelConfigInput {
            sort_order: 0,
            display_name: "Claude Test".into(),
            group_name: None,
            model_type: ModelType::Anthropic,
            base_url,
            use_full_url: full_url,
            api_key: "vendor-secret".into(),
            tooltip_data: "Claude gateway test".into(),
            model_id: "vendor-claude".into(),
            reasoning_effort: None,
            openai_endpoint: String::new(),
            openai_extra_params_enabled: false,
            openai_extra_params: json!({}),
            custom_headers_enabled: false,
            custom_headers: json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: json!({}),
            context_window_tokens: Some(200_000),
            max_completion_tokens: None,
            anthropic_max_tokens: Some(8_192),
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        })
        .await
        .unwrap()
        .model_hash
}

fn enabled_settings(default_model_hash: String) -> ClaudeSettings {
    ClaudeSettings {
        enabled: true,
        port: free_port(),
        default_model_hash: Some(default_model_hash),
        ..ClaudeSettings::default()
    }
}

fn message_request(token: &str, model: &str) -> Request<Body> {
    Request::post("/v1/messages")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "model": model,
                "max_tokens": 512,
                "stream": true,
                "stop_sequences": ["STOP"],
                "system": "be brief",
                "messages": [{"role": "user", "content": "hi"}],
            })
            .to_string(),
        ))
        .unwrap()
}

/// The SSE body as `(event, data)` pairs. Anthropic always writes both lines.
fn parse_sse(body: &[u8]) -> Vec<(String, Value)> {
    String::from_utf8_lossy(body)
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            let event = frame
                .lines()
                .find_map(|line| line.strip_prefix("event: "))
                .unwrap_or_default()
                .to_owned();
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("SSE frame has data");
            (event, serde_json::from_str(data).expect("SSE data is JSON"))
        })
        .collect()
}

#[tokio::test]
async fn the_client_receives_the_complete_anthropic_event_sequence() {
    let (_directory, store) = fixtures::temp_store().await;
    let hash = claude_model(&store, "https://api.anthropic.com".into(), false).await;
    let settings = enabled_settings(hash);
    store.set_claude_settings(settings).await.unwrap();
    let token = store.claude_token().await.unwrap();

    let provider = fake_provider::FakeProvider::default();
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "call".into(),
        },
        ModelEvent::ThinkingStart,
        ModelEvent::ThinkingDelta("planning".into()),
        ModelEvent::ThinkingSignature("sig-1".into()),
        ModelEvent::ThinkingEnd,
        ModelEvent::TextStart,
        ModelEvent::TextDelta("hello".into()),
        ModelEvent::TextEnd,
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: "toolu_1".into(),
            name: "Bash".into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: "{\"command\":\"ls\"}".into(),
        },
        ModelEvent::ToolCallEnd { index: 0 },
        ModelEvent::Usage(Usage {
            input_tokens: Some(10),
            context_input_tokens: Some(30),
            output_tokens: Some(5),
            total_tokens: Some(35),
            cache_read_tokens: Some(20),
            cache_write_tokens: Some(0),
            reasoning_tokens: None,
        }),
        ModelEvent::Done(FinishReason::ToolUse),
    ]);
    let gateway = ClaudeGateway::new(
        store.clone(),
        Arc::new(provider.clone()),
        NetworkClients::new(store.clone()),
    );

    let response = gateway
        .router()
        .oneshot(message_request(&token, "claude-sonnet-4-5"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let body = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    let events = parse_sse(&body);
    let kinds = events
        .iter()
        .map(|(event, value)| {
            assert_eq!(
                event,
                value["type"].as_str().unwrap(),
                "event 名与 type 一致"
            );
            event.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );

    assert_eq!(events[0].1["message"]["role"], "assistant");
    assert_eq!(events[0].1["message"]["model"], "claude-sonnet-4-5");
    assert_eq!(events[2].1["delta"]["type"], "thinking_delta");
    assert_eq!(events[3].1["delta"]["type"], "signature_delta");
    assert_eq!(events[3].1["delta"]["signature"], "sig-1");
    assert_eq!(events[8].1["content_block"]["type"], "tool_use");
    assert_eq!(events[8].1["content_block"]["id"], "toolu_1");
    assert_eq!(events[9].1["delta"]["partial_json"], "{\"command\":\"ls\"}");
    assert_eq!(events[11].1["delta"]["stop_reason"], "tool_use");
    assert_eq!(events[11].1["usage"]["input_tokens"], 10);
    assert_eq!(events[11].1["usage"]["cache_read_input_tokens"], 20);
    assert_eq!(events[12].1["type"], "message_stop");

    // 请求映射真的走到了共享层：system 与 max_tokens 都进了 canonical request。
    let requests = provider.requests();
    assert_eq!(requests[0].prompt.instructions, "be brief");
    assert_eq!(requests[0].model.max_output_tokens, Some(512));
}

/// A local Anthropic upstream: one text block and a final usage. The request body
/// is captured so the test can prove `stop_sequences` / `system` / `max_tokens`
/// reached the outbound adapter instead of dying in the gateway.
async fn fake_anthropic_upstream() -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
    fake_anthropic_upstream_ending_with("end_turn", None).await
}

/// Same upstream, but it ends because a stop sequence matched: Anthropic reports
/// `stop_reason: "stop_sequence"` and the matched text in `delta.stop_sequence`.
async fn fake_anthropic_upstream_ending_with(
    stop_reason: &str,
    stop_sequence: Option<&str>,
) -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = captured.clone();
    let mut delta = json!({"stop_reason": stop_reason});
    if let Some(sequence) = stop_sequence {
        delta["stop_sequence"] = json!(sequence);
    }
    let final_delta = format!(
        "event: message_delta\ndata: {}\n\n",
        json!({"type": "message_delta", "delta": delta, "usage": {"output_tokens": 7}})
    );
    let app = Router::new().route(
        "/v1/messages",
        post(move |axum::extract::Json(body): axum::extract::Json<Value>| {
            let capture = capture.clone();
            let final_delta = final_delta.clone();
            async move {
                capture.lock().unwrap().push(body);
                let body = concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42}}}\n\n",
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"pong\"}}\n\n",
                    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                )
                .to_owned()
                    + &final_delta
                    + "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
                ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let url = String::from("http://") + &address.to_string() + "/v1/messages";
    (url, captured)
}

/// `PluginRegistry::managed` 需要数据目录；测试把它指到一个只属于本进程的临时目录，
/// 不碰用户真实的 `~/.haxsd-byok-devin-v3`。
fn isolate_managed_data() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let directory = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        std::env::set_var("HAXSD_BYOK_DATA_DIR", directory.path());
    });
}

#[tokio::test]
async fn the_production_router_records_the_call_as_claude() {
    isolate_managed_data();
    let (_directory, store) = fixtures::temp_store().await;
    let (upstream, captured) = fake_anthropic_upstream().await;
    let hash = claude_model(&store, upstream, true).await;
    store
        .set_claude_settings(enabled_settings(hash))
        .await
        .unwrap();
    let token = store.claude_token().await.unwrap();

    let clients = NetworkClients::new(store.clone());
    let plugins = PluginRegistry::managed(
        store.clone(),
        PluginRuntime::managed().unwrap(),
        "claude-gateway-test".into(),
    )
    .unwrap();
    let router = Arc::new(ProviderRouter::new(
        store.clone(),
        plugins,
        clients.clone(),
        Duration::from_secs(30),
        Duration::from_secs(30),
    ));
    let gateway = ClaudeGateway::new(store.clone(), router, clients);

    let response = gateway
        .router()
        .oneshot(message_request(&token, "claude-sonnet-4-5"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    let events = parse_sse(&body);
    assert_eq!(events.last().unwrap().0, "message_stop");
    assert!(
        events[0].1["message"]["usage"]["input_tokens"]
            .as_u64()
            .unwrap()
            > 0,
        "message_start 就要给出估算的输入用量，而不是 0"
    );
    let text = events
        .iter()
        .filter(|(_, value)| value["delta"]["type"] == "text_delta")
        .map(|(_, value)| value["delta"]["text"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(text, "pong");

    {
        let outbound = captured.lock().unwrap();
        let request = &outbound[0];
        assert_eq!(request["stop_sequences"], json!(["STOP"]));
        assert_eq!(request["system"][0]["text"], "be brief");
        assert_eq!(request["max_tokens"], 512);
    }

    let calls = store.llm_calls(10).await.unwrap();
    let call = calls
        .iter()
        .find(|call| call.client_source == "claude")
        .expect("网关调用必须写进 llm_calls");
    assert_eq!(call.provider_type, "anthropic");
    assert_eq!(call.purpose, "chat");
    assert_eq!(call.status, "completed");
    assert_eq!(call.input_tokens, Some(42));
    assert_eq!(call.output_tokens, Some(7));
    assert!(call.ttfb_ms.is_some(), "TTFB 必须落库");
    assert!(call.duration_ms.is_some(), "时长必须落库");
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.client_source == "claude")
            .count(),
        1
    );
}

/// 上游按停止序列收尾：`message_delta` 必须带 `stop_reason: "stop_sequence"` 与命中
/// 的那段文本。适配器把 `delta.stop_sequence` 单独交付，网关才能把它翻回协议字段，
/// 而不是把所有正常结束都折成 `end_turn`。
#[tokio::test]
async fn an_upstream_stop_sequence_reaches_the_client() {
    isolate_managed_data();
    let (_directory, store) = fixtures::temp_store().await;
    let (upstream, _captured) =
        fake_anthropic_upstream_ending_with("stop_sequence", Some("STOP")).await;
    let hash = claude_model(&store, upstream, true).await;
    store
        .set_claude_settings(enabled_settings(hash))
        .await
        .unwrap();
    let token = store.claude_token().await.unwrap();
    let clients = NetworkClients::new(store.clone());
    let plugins = PluginRegistry::managed(
        store.clone(),
        PluginRuntime::managed().unwrap(),
        "claude-stop-sequence-test".into(),
    )
    .unwrap();
    let router = Arc::new(ProviderRouter::new(
        store.clone(),
        plugins,
        clients.clone(),
        Duration::from_secs(30),
        Duration::from_secs(30),
    ));
    let gateway = ClaudeGateway::new(store.clone(), router, clients);

    let response = gateway
        .router()
        .oneshot(message_request(&token, "claude-sonnet-4-5"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    let events = parse_sse(&body);
    let delta = events
        .iter()
        .find(|(event, _)| event == "message_delta")
        .expect("message_delta is present")
        .1
        .clone();
    assert_eq!(delta["delta"]["stop_reason"], "stop_sequence");
    assert_eq!(delta["delta"]["stop_sequence"], "STOP");
    assert_eq!(delta["usage"]["output_tokens"], 7);
}
