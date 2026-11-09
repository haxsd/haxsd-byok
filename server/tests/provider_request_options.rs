//! Provider request contracts against isolated HTTP upstreams.
use axum::{http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use cursor_server::{
    config::{ProviderConfig, ProviderKind},
    model::{
        CallClient, CallPurpose, ModelInvocation, ModelRequest, ModelSpec, PromptSpec,
        ToolDefinition,
    },
    network::OutboundClient,
    provider::{OpenAiResponsesProvider, Provider},
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

const COMPLETED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n";

fn invocation() -> ModelInvocation {
    ModelInvocation {
        purpose: CallPurpose::Chat,
        client_source: CallClient::Claude,
        call_id: "options-call".into(),
        run_id: "options-run".into(),
        conversation_id: "options-conversation".into(),
        provider_call_index: 0,
        request: ModelRequest {
            prompt: PromptSpec {
                instructions: "test".into(),
                tools: vec![ToolDefinition {
                    name: "read_file".into(),
                    description: "read a test file".into(),
                    parameters: json!({"type":"object"}),
                }],
            },
            model: ModelSpec::new("fixture"),
            history: vec![],
        },
    }
}

fn provider(url: String) -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new(
        OutboundClient::fixed(reqwest::Client::builder().no_proxy().build().unwrap()),
        ProviderConfig {
            kind: ProviderKind::OpenAiResponses,
            request_url: url,
            api_key: "fixture-key".into(),
            custom_headers: Default::default(),
            max_output_tokens: None,
            request_timeout: Duration::from_secs(10),
        },
    )
}

#[tokio::test]
async fn responses_preserve_client_tool_choice_parallel_policy_and_temperature() {
    let seen = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let app = Router::new().route(
        "/responses",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().await.push(body);
                ([("content-type", "text/event-stream")], COMPLETED)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = provider(format!(
        "http://{}/responses",
        listener.local_addr().unwrap()
    ));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for (choice, expected) in [
        (json!({"type":"auto"}), json!("auto")),
        (json!({"type":"any"}), json!("required")),
        (json!({"type":"none"}), json!("none")),
        (
            json!({"type":"tool","name":"read_file"}),
            json!({"type":"function","name":"read_file"}),
        ),
    ] {
        let mut selected = invocation();
        selected.request.model.reasoning.enabled = Some(false);
        selected.request.model.client.tool_choice = Some(choice);
        selected.request.model.client.tool_choice.as_mut().unwrap()["disable_parallel_tool_use"] =
            json!(true);
        selected.request.model.client.temperature = Some(0.2);
        let mut stream = provider.stream(selected, CancellationToken::new());
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
        let requests = seen.lock().await;
        let body = requests.last().unwrap();
        assert_eq!(body["tool_choice"], expected);
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["temperature"], 0.2);
        assert!(body.get("reasoning").is_none());
    }
    task.abort();
}

#[tokio::test]
async fn an_explicitly_unsupported_cache_key_is_removed_without_changing_the_request() {
    let seen = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let app = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
        let captured = captured.clone();
        async move {
            let has_key = body.get("prompt_cache_key").is_some();
            captured.lock().await.push(body);
            if has_key {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":{
                    "param":"prompt_cache_key", "message":"Unrecognized request argument supplied: prompt_cache_key"
                }}))).into_response();
            }
            ([("content-type", "text/event-stream")], COMPLETED).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = provider(format!(
        "http://{}/responses",
        listener.local_addr().unwrap()
    ));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut selected = invocation();
    selected.request.model.reasoning.effort = Some("high".into());
    let mut stream = provider.stream(selected, CancellationToken::new());
    let mut failure = None;
    while let Some(event) = stream.next().await {
        if let Err(error) = event {
            failure = Some(error);
            break;
        }
    }
    task.abort();
    assert!(
        failure.is_none(),
        "optional cache routing must not break a supported request: {failure:?}"
    );
    let requests = seen.lock().await;
    assert_eq!(requests.len(), 2);
    let mut expected = requests[0].clone();
    assert!(expected
        .as_object_mut()
        .unwrap()
        .remove("prompt_cache_key")
        .is_some());
    assert_eq!(
        requests[1], expected,
        "only the explicitly rejected cache field may change"
    );
}
