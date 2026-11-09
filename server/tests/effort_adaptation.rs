//! Request-level effort regression coverage. All upstreams and stores are isolated fixtures.
#[path = "support/fixtures.rs"]
mod fixtures;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use cursor_server::{
    codex::{CodexGateway, CodexSettings},
    model::{
        CallClient, CallPurpose, ContentPart, ModelConfigInput, ModelInvocation, ModelRequest,
        ModelSpec, ModelType, ProjectedContent, ProjectedMessage, PromptSpec, Role,
    },
    network::NetworkClients,
    plugin::{PluginRegistry, PluginRuntime},
    provider::{FinishReason, ModelEvent, Provider, ProviderRouter},
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

fn isolate_runtime() {
    static DIRECTORY: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIRECTORY.get_or_init(|| {
        let directory = tempfile::tempdir().unwrap();
        std::env::set_var("HAXSD_BYOK_DATA_DIR", directory.path());
        directory
    });
}

fn model(protocol: &str, url: &str) -> ModelConfigInput {
    ModelConfigInput {
        sort_order: 0,
        display_name: "effort fixture".into(),
        group_name: None,
        model_type: if protocol == "anthropic" {
            ModelType::Anthropic
        } else {
            ModelType::OpenAi
        },
        base_url: url.into(),
        use_full_url: true,
        api_key: "vendor-secret".into(),
        tooltip_data: "isolated fixture".into(),
        model_id: "fixture-model".into(),
        reasoning_effort: (protocol != "anthropic").then(|| "high".into()),
        openai_endpoint: if protocol == "chat" {
            "/v1/chat/completions".into()
        } else {
            "/v1/responses".into()
        },
        openai_extra_params_enabled: false,
        openai_extra_params: json!({}),
        custom_headers_enabled: false,
        custom_headers: json!({}),
        anthropic_extra_params_enabled: false,
        anthropic_extra_params: json!({}),
        context_window_tokens: Some(32768),
        max_completion_tokens: Some(4096),
        anthropic_max_tokens: Some(4096),
        anthropic_thinking_effort: (protocol == "anthropic").then(|| "high".into()),
        thinking_budget_tokens: None,
    }
}

fn success_stream(protocol: &str) -> &'static str {
    match protocol {
        "chat" => "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        "responses" => "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"completed\",\"output\":[]}}\n\n",
        _ => "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fixture\",\"usage\":{\"input_tokens\":1}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
    }
}

async fn upstream(
    protocol: &'static str,
    accepted: &'static str,
) -> (
    String,
    Arc<tokio::sync::Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let captured = seen.clone();
    let path = match protocol {
        "chat" => "/reasoning_effort",
        "responses" => "/reasoning/effort",
        _ => "/output_config/effort",
    };
    let router = Router::new().route("/model", post(move |Json(body): Json<Value>| {
        let captured = captured.clone();
        async move {
            let effort = body.pointer(path).and_then(Value::as_str).map(str::to_owned);
            let streaming = body["stream"].as_bool().unwrap_or(false);
            captured.lock().await.push(body);
            if effort.as_deref() != Some(accepted) {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":{"message":"unsupported reasoning effort"}}))).into_response();
            }
            if protocol == "responses" && !streaming {
                return Json(json!({"id":"resp_fixture","object":"response","status":"completed","output":[]})).into_response();
            }
            ([("content-type", "text/event-stream")], success_stream(protocol)).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/model", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, seen, task)
}

#[tokio::test]
async fn cursor_and_devin_provider_routes_keep_supported_values_and_adapt_rejected_values() {
    isolate_runtime();
    for client in [CallClient::Cursor, CallClient::Devin] {
        for protocol in ["chat", "responses", "anthropic"] {
            for accepted in ["xhigh", "max"] {
                let (url, seen, task) = upstream(protocol, accepted).await;
                let (_directory, store) = fixtures::temp_store().await;
                store.set_detailed_logging(true).await.unwrap();
                let configured = store.create_model(&model(protocol, &url)).await.unwrap();
                let plugins = PluginRegistry::managed(
                    store.clone(),
                    PluginRuntime::managed().unwrap(),
                    "fixture".into(),
                )
                .unwrap();
                let router = ProviderRouter::new(
                    store.clone(),
                    plugins,
                    NetworkClients::new(store.clone()),
                    Duration::from_secs(5),
                    Duration::from_secs(5),
                );
                let mut selected = ModelSpec::new(configured.model_hash);
                selected.reasoning.enabled = Some(true);
                selected.reasoning.effort = Some("xhigh".into());
                let mut stream = router.stream(
                    ModelInvocation {
                        purpose: CallPurpose::Chat,
                        client_source: client,
                        call_id: "fixture-call".into(),
                        run_id: "fixture-run".into(),
                        conversation_id: "fixture-conversation".into(),
                        provider_call_index: 0,
                        request: ModelRequest {
                            prompt: PromptSpec {
                                instructions: "test".into(),
                                tools: vec![],
                            },
                            model: selected,
                            history: vec![ProjectedMessage {
                                message_id: "user-1".into(),
                                role: Role::User,
                                content: ProjectedContent::Parts(vec![ContentPart::Text {
                                    text: "hello".into(),
                                }]),
                            }],
                        },
                    },
                    CancellationToken::new(),
                );
                let mut events = Vec::new();
                while let Some(event) = stream.next().await {
                    events.push(event.unwrap());
                }
                assert_eq!(
                    events.last(),
                    Some(&ModelEvent::Done(FinishReason::Stop)),
                    "{protocol}"
                );
                let requests = seen.lock().await;
                assert_eq!(requests.len(), if accepted == "xhigh" { 1 } else { 2 });
                let field = match protocol {
                    "chat" => "/reasoning_effort",
                    "responses" => "/reasoning/effort",
                    _ => "/output_config/effort",
                };
                assert_eq!(requests[0].pointer(field).unwrap(), "xhigh");
                let mut expected = requests[0].clone();
                *expected.pointer_mut(field).unwrap() = json!(accepted);
                assert_eq!(
                    requests.last().unwrap(),
                    &expected,
                    "only effort may change"
                );
                let calls = store.llm_calls(10).await.unwrap();
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].status, "completed");
                assert_eq!(calls[0].client_source, client.as_str());
                assert_eq!(calls[0].reasoning_effort.as_deref(), Some(accepted));
                let recorded = store
                    .llm_call_request("fixture-call")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(recorded.body.pointer(field).unwrap(), accepted);
                task.abort();
            }
        }
    }
}

#[tokio::test]
async fn codex_configuration_errors_keep_the_same_code_in_the_client_and_call_record() {
    let (directory, store) = fixtures::temp_store().await;
    let mut input = model("responses", "http://127.0.0.1:1/model");
    input.custom_headers_enabled = true;
    input.custom_headers = json!({"bad header name":"fixture-value"});
    let configured = store.create_model(&input).await.unwrap();
    store
        .set_codex_settings(CodexSettings {
            enabled: true,
            model_hashes: vec![configured.model_hash.clone()],
            default_model_hash: Some(configured.model_hash.clone()),
            home: directory.path().join("codex-home").display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let token = store.codex_token().await.unwrap();
    let gateway = CodexGateway::new(store.clone(), NetworkClients::new(store.clone()));
    let response = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":configured.model_hash,"input":"hello","stream":false})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let text = String::from_utf8(
        to_bytes(response.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("BYOK-REQUEST"), "{text}");
    let call = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let calls = store.llm_calls(10).await.unwrap();
            if let Some(call) = calls.into_iter().find(|call| call.status != "running") {
                break call;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the rejected request must have a terminal call record");
    assert_eq!(call.status, "error");
    assert_eq!(call.error_kind.as_deref(), Some("BYOK-REQUEST"));
    assert!(text.contains(&call.call_id));
}

#[tokio::test]
async fn codex_failures_keep_one_terminal_response_and_the_recorded_reference() {
    for (kind, status, response_body) in [
        ("http", StatusCode::BAD_REQUEST, r#"{"error":{"message":"raw-upstream-command vendor-secret Bearer opaque-upstream-token","param":"messages"}}"#),
        ("failed", StatusCode::OK, "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"in_progress\"}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"raw-upstream-command vendor-secret Bearer opaque-upstream-token\"},\"output\":[]}}\n\n"),
        ("missing_error", StatusCode::OK, "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"failed\",\"output\":[]}}\n\n"),
        ("error", StatusCode::OK, "data: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"raw-upstream-command vendor-secret Bearer opaque-upstream-token\"}\n\n"),
    ] {
        let route = Router::new().route("/model", post(move || async move {
            let content_type = if kind == "http" {"application/json"} else {"text/event-stream"};
            (status, [("content-type",content_type)], response_body)
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/model", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, route).await.unwrap(); });
        let (directory, store) = fixtures::temp_store().await;
        let configured = store.create_model(&model("responses", &url)).await.unwrap();
        store.set_codex_settings(CodexSettings {
            enabled:true,model_hashes:vec![configured.model_hash.clone()],default_model_hash:Some(configured.model_hash.clone()),
            home:directory.path().join("codex-home").display().to_string(),..Default::default()
        }).await.unwrap();
        let token = store.codex_token().await.unwrap();
        let gateway = CodexGateway::new(store.clone(), NetworkClients::new(store.clone()));
        let response = gateway.router().oneshot(Request::post("/v1/responses")
            .header("authorization", format!("Bearer {token}")).header("content-type","application/json")
            .body(Body::from(json!({"model":configured.model_hash,"input":"hello","stream":true}).to_string())).unwrap()).await.unwrap();
        let body = to_bytes(response.into_body(), 65536).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        let calls = store.llm_calls(10).await.unwrap();
        assert_eq!(calls.len(), 1, "{kind}");
        assert_eq!(calls[0].status, "error", "{kind}: {text}");
        let expected_code = if kind == "http" {"BYOK-UPSTREAM_HTTP"} else {"BYOK-STREAM"};
        assert_eq!(calls[0].error_kind.as_deref(), Some(expected_code));
        assert!(text.contains(expected_code), "{kind}: {text}");
        assert!(text.contains(&calls[0].call_id), "{kind}: {text}");
        for secret in ["vendor-secret","opaque-upstream-token","raw-upstream-command"] { assert!(!text.contains(secret), "{kind}: {text}"); }
        if kind == "failed" || kind == "missing_error" {
            let frames:Vec<Value> = text.lines().filter_map(|line| line.strip_prefix("data: ")).filter_map(|data|serde_json::from_str(data).ok()).collect();
            let failed:Vec<_> = frames.iter().filter(|value| value["type"] == "response.failed").collect();
            assert_eq!(failed.len(), 1, "{text}");
            assert_eq!(failed[0]["response"]["id"], "resp_fixture");
            assert!(failed[0].pointer("/response/error/message").and_then(Value::as_str).is_some_and(|message|message.contains(&calls[0].call_id)), "{text}");
        } else if kind == "error" {
            assert_eq!(text.matches("\"type\":\"error\"").count(), 1, "{text}");
            assert!(!text.contains("\"type\":\"response.failed\""), "{text}");
        }
        task.abort();
    }
}

#[tokio::test]
async fn codex_independent_chat_and_responses_routes_honor_the_client_before_model_defaults() {
    for protocol in ["responses", "chat"] {
        for accepted in ["xhigh", "max"] {
            let (url, seen, task) = upstream(protocol, accepted).await;
            let (directory, store) = fixtures::temp_store().await;
            let configured = store.create_model(&model(protocol, &url)).await.unwrap();
            store
                .set_codex_settings(CodexSettings {
                    enabled: true,
                    model_hashes: vec![configured.model_hash.clone()],
                    default_model_hash: Some(configured.model_hash.clone()),
                    home: directory.path().join("codex-home").display().to_string(),
                    ..Default::default()
                })
                .await
                .unwrap();
            let token = store.codex_token().await.unwrap();
            let gateway = CodexGateway::new(store.clone(), NetworkClients::new(store.clone()));
            let response = gateway.router().oneshot(Request::post("/v1/responses")
                .header("authorization", format!("Bearer {token}")).header("content-type","application/json")
                .body(Body::from(json!({"model":configured.model_hash,"input":"hello","stream":false,"reasoning":{"effort":"xhigh"}}).to_string())).unwrap()).await.unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
            assert_eq!(
                status,
                StatusCode::OK,
                "{}",
                String::from_utf8_lossy(&bytes)
            );
            let response: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(response["status"], "completed");
            let requests = seen.lock().await;
            assert_eq!(requests.len(), if accepted == "xhigh" { 1 } else { 2 });
            let field = if protocol == "chat" {
                "/reasoning_effort"
            } else {
                "/reasoning/effort"
            };
            assert_eq!(requests[0].pointer(field).unwrap(), "xhigh");
            let mut expected = requests[0].clone();
            *expected.pointer_mut(field).unwrap() = json!(accepted);
            assert_eq!(requests.last().unwrap(), &expected);
            let calls = store.llm_calls(10).await.unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].status, "completed");
            assert_eq!(calls[0].reasoning_effort.as_deref(), Some(accepted));
            task.abort();
        }
    }
}
