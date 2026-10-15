//! Codex 默认关闭、设置持久化和独立鉴权边界。
#[path = "support/fixtures.rs"]
mod fixtures;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use cursor_server::{
    codex::{CodexGateway, CodexSettings},
    network::NetworkClients,
};
use tower::ServiceExt;

#[tokio::test]
async fn codex_is_disabled_until_explicitly_configured() {
    let (_directory, store) = fixtures::temp_store().await;
    let settings = store.codex_settings().await.unwrap();
    assert!(!settings.enabled);
    assert!(settings.model_hashes.is_empty());
    let gateway = CodexGateway::new(store.clone(), NetworkClients::new(store));
    let status = gateway.status().await.unwrap();
    assert!(!status.listening);
    let response = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"unconfigured","input":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn gateway_token_is_stable_and_never_serialized_with_settings() {
    let (_directory, store) = fixtures::temp_store().await;
    let first = store.codex_token().await.unwrap();
    assert!(first.len() >= 32);
    assert_eq!(first, store.codex_token().await.unwrap());
    let serialized = serde_json::to_string(&store.codex_settings().await.unwrap()).unwrap();
    assert!(!serialized.contains(&first));
    let gateway = CodexGateway::new(store.clone(), NetworkClients::new(store));
    let response = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {first}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"unconfigured","input":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert!(serde_json::from_slice::<serde_json::Value>(&body)
        .unwrap()
        .get("error")
        .is_some());
}

#[tokio::test]
async fn selected_models_are_validated_before_settings_are_persisted() {
    let (_directory, store) = fixtures::temp_store().await;
    let settings = CodexSettings {
        model_hashes: vec!["missing".into()],
        default_model_hash: Some("missing".into()),
        ..CodexSettings::default()
    };
    assert!(store.set_codex_settings(settings).await.is_err());
    assert!(store
        .codex_settings()
        .await
        .unwrap()
        .model_hashes
        .is_empty());
}

#[tokio::test]
async fn unknown_routes_are_never_forwarded_to_cursor_or_an_upstream() {
    let (_directory, store) = fixtures::temp_store().await;
    let gateway = CodexGateway::new(store.clone(), NetworkClients::new(store));
    let response = gateway
        .router()
        .oneshot(
            Request::get("/aiserver.v1.AiService/AvailableModels")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

async fn configured_gateway(
    protocol: &str,
    upstream_url: String,
) -> (
    tempfile::TempDir,
    cursor_server::store::Store,
    CodexGateway,
    String,
) {
    use cursor_server::model::{ModelConfigInput, ModelType};
    let (directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&ModelConfigInput {
            sort_order: 0,
            display_name: "Codex test model".into(),
            group_name: None,
            model_type: if protocol == "anthropic" {
                ModelType::Anthropic
            } else {
                ModelType::OpenAi
            },
            base_url: upstream_url,
            use_full_url: true,
            api_key: "vendor-secret".into(),
            tooltip_data: "Codex integration test".into(),
            model_id: "vendor-model".into(),
            reasoning_effort: None,
            openai_endpoint: if protocol == "chat" {
                "/v1/chat/completions".into()
            } else {
                "/v1/responses".into()
            },
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: Some(32768),
            max_completion_tokens: Some(4096),
            anthropic_max_tokens: Some(4096),
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        })
        .await
        .unwrap();
    store
        .set_codex_settings(CodexSettings {
            enabled: true,
            model_hashes: vec![model.model_hash.clone()],
            default_model_hash: Some(model.model_hash.clone()),
            home: directory.path().join("codex-home").display().to_string(),
            ..CodexSettings::default()
        })
        .await
        .unwrap();
    let gateway = CodexGateway::new(store.clone(), NetworkClients::new(store.clone()));
    (directory, store, gateway, model.model_hash)
}

#[tokio::test]
async fn native_responses_preserve_custom_items_and_inject_only_vendor_credentials() {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let captured = seen.clone();
    let router = Router::new().route("/upstream", post(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
        let captured = captured.clone();
        async move {
            *captured.lock().await = Some((headers, body));
            Json(json!({"id":"resp_vendor","object":"response","status":"completed","output":[
                {"id":"custom-1","type":"custom_tool_call","call_id":"call-1","name":"apply_patch","input":"*** Begin Patch\n*** End Patch"}
            ],"usage":{"input_tokens":12,"output_tokens":3,"total_tokens":15}}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/upstream", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, store, gateway, id) = configured_gateway("responses", url).await;
    sqlx::query("UPDATE model_configs SET reasoning_effort = 'high' WHERE model_hash = ?")
        .bind(&id)
        .execute(store.pool())
        .await
        .unwrap();
    let body = json!({"model":id,"stream":false,"store":false,"input":[{"role":"user","content":"patch"}],
        "tools":[{"type":"custom","name":"apply_patch","format":{"type":"text"}}]});
    let response = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header(
                    "authorization",
                    format!("Bearer {}", store.codex_token().await.unwrap()),
                )
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let output: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(output["output"][0]["type"], "custom_tool_call");
    let guard = seen.lock().await;
    let (headers, upstream) = guard.as_ref().unwrap();
    assert_eq!(headers["authorization"], "Bearer vendor-secret");
    assert_eq!(upstream["model"], "vendor-model");
    assert_eq!(upstream["reasoning"]["effort"], "high");
    assert_eq!(upstream["max_output_tokens"], 4096);
    assert_eq!(upstream["tools"], body["tools"]);
    assert_eq!(upstream["input"], body["input"]);
    drop(guard);
    let calls = store.llm_calls(10).await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, "completed");
    assert_eq!(
        serde_json::to_value(&calls[0]).unwrap()["client_source"],
        "codex"
    );
    assert_eq!(calls[0].input_tokens, Some(12));
    task.abort();
}

#[tokio::test]
async fn chat_model_can_request_a_codex_custom_tool_and_receive_its_result() {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    let requests = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let router = Router::new().route("/chat", post(move |Json(body): Json<Value>| {
        let captured = captured.clone();
        async move {
            let mut requests = captured.lock().await;
            let tool_name = body["tools"][0]["function"]["name"].as_str().unwrap().to_string();
            requests.push(body);
            let chunks = if requests.len() == 1 {
                vec![json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-patch","type":"function","function":{"name":tool_name,"arguments":"{\"input\":\"*** Begin Patch\\n*** End Patch\"}"}}]},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":10,"total_tokens":30}})]
            } else {
                vec![json!({"choices":[{"index":0,"delta":{"content":"完成"},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":35,"completion_tokens":3,"total_tokens":38}})]
            };
            let stream = chunks.iter().map(|chunk| format!("data: {chunk}\n\n")).collect::<String>() + "data: [DONE]\n\n";
            ([("content-type", "text/event-stream")], stream)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/chat", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, store, gateway, id) = configured_gateway("chat", url).await;
    let mut body = json!({"model":id,"stream":false,"input":[{"role":"user","content":"修改文件"}],
        "tools":[{"type":"custom","name":"apply_patch","description":"Apply patch"}]});
    let token = store.codex_token().await.unwrap();
    let first = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first: Value =
        serde_json::from_slice(&to_bytes(first.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    let tool = first["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "custom_tool_call")
        .unwrap();
    assert_eq!(tool["name"], "apply_patch");
    assert_eq!(tool["input"], "*** Begin Patch\n*** End Patch");
    body["input"].as_array_mut().unwrap().push(tool.clone());
    body["input"].as_array_mut().unwrap().push(
        json!({"type":"custom_tool_call_output","call_id":tool["call_id"],"output":"Success"}),
    );
    let second = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second: Value =
        serde_json::from_slice(&to_bytes(second.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(second["status"], "completed");
    assert!(second["output"].to_string().contains("完成"));
    let seen = requests.lock().await;
    assert_eq!(seen.len(), 2);
    assert!(seen[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool" && m["content"] == "Success"));
    assert_eq!(store.llm_calls(10).await.unwrap().len(), 2);
    task.abort();
}

#[tokio::test]
async fn enabling_and_disabling_the_gateway_changes_the_real_listener() {
    let (_dir, store, gateway, _id) =
        configured_gateway("responses", "http://127.0.0.1:1/responses".into()).await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut settings = store.codex_settings().await.unwrap();
    settings.port = port;
    gateway.save_settings(settings.clone()).await.unwrap();
    assert!(gateway.status().await.unwrap().listening);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    settings.enabled = false;
    gateway.save_settings(settings).await.unwrap();
    assert!(!gateway.status().await.unwrap().listening);
    assert!(client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .await
        .is_err());
}

#[tokio::test]
async fn a_port_conflict_leaves_saved_settings_and_client_files_unchanged() {
    let (directory, store, gateway, _id) =
        configured_gateway("responses", "http://127.0.0.1:1/responses".into()).await;
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let before = store.codex_settings().await.unwrap();
    let mut changed = before.clone();
    changed.port = occupied.local_addr().unwrap().port();
    assert!(gateway.save_settings(changed).await.is_err());
    assert_eq!(store.codex_settings().await.unwrap(), before);
    assert!(!directory.path().join("codex-home/config.toml").exists());
    assert!(gateway.status().await.unwrap().last_error.is_some());
}

#[tokio::test]
async fn configuration_conflict_does_not_overwrite_a_model_catalog_or_start_listener() {
    let (dir, store, gateway, _) =
        configured_gateway("responses", "http://127.0.0.1:1/responses".into()).await;
    let home = dir.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "model_provider = 'another-tool'\n",
    )
    .unwrap();
    std::fs::write(home.join("haxsd-byok-models.json"), "foreign catalog").unwrap();
    let mut settings = store.codex_settings().await.unwrap();
    settings.port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    store.set_codex_settings(settings).await.unwrap();
    assert!(gateway.apply().await.is_err());
    assert_eq!(
        std::fs::read_to_string(home.join("haxsd-byok-models.json")).unwrap(),
        "foreign catalog"
    );
    assert!(!gateway.status().await.unwrap().listening);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn changing_an_applied_model_cannot_silently_switch_codex_upstreams() {
    let (_dir, store, gateway, id) =
        configured_gateway("responses", "http://127.0.0.1:1/responses".into()).await;
    let mut settings = store.codex_settings().await.unwrap();
    settings.port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    store.set_codex_settings(settings).await.unwrap();
    assert!(gateway.apply().await.unwrap().configured);
    sqlx::query("UPDATE model_configs SET model_id = ? WHERE model_hash = ?")
        .bind("changed-upstream")
        .bind(&id)
        .execute(store.pool())
        .await
        .unwrap();
    let token = store.codex_token().await.unwrap();
    let response = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    serde_json::json!({"model":id,"input":"go","stream":false}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(store.llm_calls(10).await.unwrap().is_empty());
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn non_streaming_upstream_body_timeout_is_recorded_as_failure_not_cancellation() {
    use axum::{routing::post, Router};
    let app = Router::new().route(
        "/slow",
        post(|| async {
            (
                [("content-type", "application/json")],
                Body::from_stream(futures_util::stream::pending::<
                    Result<bytes::Bytes, std::convert::Infallible>,
                >()),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/slow", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (_dir, store, _gateway, id) = configured_gateway("responses", url).await;
    let gateway = CodexGateway::with_timeouts(
        store.clone(),
        NetworkClients::new(store.clone()),
        std::time::Duration::from_secs(2),
        std::time::Duration::from_millis(50),
    );
    let token = store.codex_token().await.unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        gateway.router().oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    serde_json::json!({"model":id,"input":"go","stream":false}).to_string(),
                ))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let calls = store.llm_calls(10).await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, "error");
    server.abort();
}

#[tokio::test]
async fn malformed_streams_and_vendor_errors_are_failures_without_leaking_keys() {
    use axum::{routing::post, Router};
    use serde_json::{json, Value};
    let app = Router::new().route("/broken", post(|| async {
        ([("content-type", "text/event-stream")], "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\ndata: {\"error\":{\"message\":\"vendor-secret is invalid\"}}\n\n")
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/broken", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (_dir, store, gateway, id) = configured_gateway("chat", url).await;
    let response = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header(
                    "authorization",
                    format!("Bearer {}", store.codex_token().await.unwrap()),
                )
                .body(Body::from(
                    json!({"model":id,"input":"go","stream":true}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        !text.contains("vendor-secret"),
        "vendor credential leaked to Codex"
    );
    let events: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "response.failed")
            .count(),
        1
    );
    let first_id = &events[0]["response"]["id"];
    assert_eq!(&events.last().unwrap()["response"]["id"], first_id);
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], json!(index));
    }
    assert_eq!(store.llm_calls(10).await.unwrap()[0].status, "error");
    server.abort();
}

#[tokio::test]
async fn native_stream_requests_reject_json_instead_of_returning_a_false_success() {
    use axum::{routing::post, Json, Router};
    use serde_json::json;
    let app = Router::new().route(
        "/json",
        post(|| async { Json(json!({"id":"resp","status":"completed","output":[]})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/json", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (_dir, store, gateway, id) = configured_gateway("responses", url).await;
    let response = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header(
                    "authorization",
                    format!("Bearer {}", store.codex_token().await.unwrap()),
                )
                .body(Body::from(
                    json!({"model":id,"input":"go","stream":true}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(store.llm_calls(10).await.unwrap()[0].status, "error");
    server.abort();
}

#[tokio::test]
#[ignore = "需要通过 HAXSD_BYOK_CODEX_TEST_BINARY 指定现有桌面客户端的内置引擎"]
async fn installed_desktop_engine_edits_a_file_runs_a_command_and_continues() {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let binary = std::env::var_os("HAXSD_BYOK_CODEX_TEST_BINARY")
        .expect("set the installed desktop engine path");
    let protocols: Vec<String> = std::env::var("HAXSD_BYOK_CODEX_TEST_PROTOCOLS")
        .unwrap_or_else(|_| "responses,chat,anthropic".into())
        .split(',')
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    for protocol in protocols {
        let protocol = protocol.as_str().to_string();
        eprintln!("[{protocol}] starting");
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let captured = seen.clone();
        let handler_protocol = protocol.clone();
        let app=Router::new().route("/inference",post(move |Json(body):Json<Value>| {
            let captured=captured.clone();
            let protocol=handler_protocol.clone();
            async move {
                let mut seen=captured.lock().await;
                seen.push(body.clone());
                // 按历史里已有的工具结果推进，而不是按请求序号：客户端重试同一轮时
                // 仍然应该收到同一个工具调用。
                let step=match protocol.as_str() {
                    "chat" => body["messages"].as_array().map(|messages|messages.iter().filter(|message|message["role"]=="tool").count()).unwrap_or(0),
                    "anthropic" => body["messages"].as_array().map(|messages|messages.iter().filter_map(|message|message["content"].as_array()).flatten().filter(|block|block["type"]=="tool_result").count()).unwrap_or(0),
                    _ => body["input"].as_array().map(|items|items.iter().filter(|item|matches!(item["type"].as_str(),Some("function_call_output"|"custom_tool_call_output"))).count()).unwrap_or(0),
                };
                let tools=body["tools"].as_array().expect("real Codex must declare tools");
                let name=|wanted:&str| -> String {
                    tools.iter().find_map(|tool| {
                        let name=tool.pointer("/function/name").or_else(||tool.get("name"))?.as_str()?;
                        (name==wanted).then(||name.to_string())
                    }).unwrap_or_else(|| { eprintln!("desktop did not declare {wanted}"); String::new() })
                };
                let patch="*** Begin Patch\n*** Add File: proof.txt\n+codex-harness-ok\n*** End Patch";
                let (tool,args)=if step==0 { (name("apply_patch"),json!({"input":patch})) }
                    else if step==1 { (name("exec_command"),json!({"cmd":"Get-Content proof.txt"})) }
                    else { (String::new(),json!({})) };
                let final_turn=step>=2;
                let chunks=match protocol.as_str() {
                    "chat" => vec![json!({"choices":[{"index":0,"delta":if final_turn {json!({"content":"Verified codex-harness-ok"})} else {json!({"tool_calls":[{"index":0,"id":format!("call_{step}"),"type":"function","function":{"name":tool,"arguments":args.to_string()}}]})},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":if final_turn {"stop"} else {"tool_calls"}}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}})],
                    "anthropic" => vec![json!({"type":"message_start","message":{"id":format!("msg_{step}"),"type":"message","role":"assistant","content":[],"usage":{"input_tokens":100,"output_tokens":0}}}),
                        json!({"type":"content_block_start","index":0,"content_block":if final_turn {json!({"type":"text","text":"Verified codex-harness-ok"})} else {json!({"type":"tool_use","id":format!("call_{step}"),"name":tool,"input":args})}}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":if final_turn {"end_turn"} else {"tool_use"}},"usage":{"output_tokens":20}}),json!({"type":"message_stop"})],
                    _ => {
                        let item=if final_turn { json!({"id":"msg_final","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Verified codex-harness-ok","annotations":[]}]}) }
                            else if step==0 {json!({"id":"patch","type":"custom_tool_call","call_id":"call_0","name":tool,"input":patch,"status":"completed"})}
                            else {json!({"id":"command","type":"function_call","call_id":"call_1","name":tool,"arguments":args.to_string(),"status":"completed"})};
                        vec![json!({"type":"response.created","response":{"id":format!("resp_{step}"),"status":"in_progress","output":[]}}),
                            json!({"type":"response.output_item.done","output_index":0,"item":item}),
                            json!({"type":"response.completed","response":{"id":format!("resp_{step}"),"status":"completed","output":[item],"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120}}})]
                    }
                };
                let text=chunks.iter().map(|chunk|format!("data: {chunk}\n\n")).collect::<String>();
                ([("content-type","text/event-stream")],text)
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/inference", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (_dir, store, gateway, model) = configured_gateway(&protocol, url).await;
        let engine_directory = tempfile::tempdir_in(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"),
        )
        .unwrap();
        let workspace = engine_directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mut settings = store.codex_settings().await.unwrap();
        settings.home = engine_directory
            .path()
            .join("codex-home")
            .display()
            .to_string();
        settings.port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        store.set_codex_settings(settings.clone()).await.unwrap();
        gateway.apply().await.unwrap();
        eprintln!("[{protocol}] gateway applied on port {}", settings.port);
        let mut child = tokio::process::Command::new(&binary)
            .args(["app-server", "--stdio", "--strict-config"])
            .env("CODEX_HOME", &settings.home)
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .current_dir(&workspace)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("desktop engine: {line}");
            }
        });
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let requests = [
            json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"haxsd-byok-test","version":"1"},"capabilities":{"experimentalApi":true}}}),
            json!({"id":2,"method":"model/list","params":{}}),
            json!({"id":3,"method":"thread/start","params":{"cwd":workspace,"model":model,"approvalPolicy":"never","sandbox":"workspace-write","ephemeral":true,"config":{"windows.sandbox":"unelevated"}}}),
        ];
        let mut thread = None;
        eprintln!("[{protocol}] engine started");
        for request in requests {
            stdin
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            stdin.flush().await.unwrap();
            loop {
                let line =
                    tokio::time::timeout(std::time::Duration::from_secs(30), stdout.next_line())
                        .await
                        .expect("desktop RPC timed out")
                        .unwrap()
                        .expect("desktop engine terminated");
                let event: Value = serde_json::from_str(&line).unwrap();
                if event["id"] != request["id"] {
                    continue;
                }
                assert!(
                    event.get("error").is_none(),
                    "{protocol} RPC error: {event}"
                );
                if request["id"] == 2 {
                    assert!(
                        event["result"]["data"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|entry| entry["id"] == model || entry["model"] == model),
                        "desktop did not load the applied model catalog: {event}"
                    );
                }
                if request["id"] == 3 {
                    thread = event
                        .pointer("/result/thread/id")
                        .and_then(Value::as_str)
                        .map(String::from);
                }
                break;
            }
            if request["id"] == 1 {
                stdin
                    .write_all(b"{\"method\":\"initialized\"}\n")
                    .await
                    .unwrap();
                stdin.flush().await.unwrap();
            }
        }
        let request = json!({"id":4,"method":"turn/start","params":{"threadId":thread.unwrap(),"input":[{"type":"text","text":"Create proof.txt containing codex-harness-ok with apply_patch, read it with shell_command, then report verification."}]}});
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
        let mut command_verified = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
        loop {
            let line = tokio::time::timeout_at(deadline, stdout.next_line())
                .await
                .expect("desktop tool loop timed out")
                .unwrap()
                .expect("desktop engine terminated");
            let event: Value = serde_json::from_str(&line).unwrap();
            if event["method"] == "item/completed"
                && event.pointer("/params/item/type").and_then(Value::as_str)
                    == Some("commandExecution")
            {
                let item = &event["params"]["item"];
                assert_eq!(
                    item["exitCode"],
                    json!(0),
                    "{protocol} command failed: {item}"
                );
                assert!(
                    item["aggregatedOutput"]
                        .as_str()
                        .unwrap_or("")
                        .contains("codex-harness-ok"),
                    "{protocol} command did not read the created file: {item}"
                );
                command_verified = true;
            }
            assert!(
                event.get("error").is_none(),
                "{protocol} RPC error: {event}"
            );
            if event["method"] == "turn/completed" {
                assert_eq!(
                    event["params"]["turn"]["status"], "completed",
                    "{protocol}: {event}"
                );
                break;
            }
        }
        assert!(
            command_verified,
            "{protocol} never completed a real command"
        );
        eprintln!("[{protocol}] turn completed with a real command");
        assert_eq!(
            std::fs::read_to_string(workspace.join("proof.txt"))
                .unwrap()
                .trim(),
            "codex-harness-ok"
        );
        let requests = seen.lock().await;
        assert!(
            requests.len() >= 3,
            "{protocol} did not complete two tool rounds"
        );
        assert!(
            requests
                .last()
                .unwrap()
                .to_string()
                .contains("codex-harness-ok"),
            "the real command result was not replayed"
        );
        assert_eq!(store.llm_calls(10).await.unwrap().len(), requests.len());
        drop(requests);
        eprintln!("[{protocol}] tool rounds and call records verified; stopping the engine");
        let _ = child.kill().await;
        let _ = child.wait().await;
        eprintln!("[{protocol}] engine stopped");
        stderr_task.abort();
        let _ = stderr_task.await;
        gateway.shutdown().await.unwrap();
        server.abort();
        eprintln!("[{protocol}] server aborted");
        let _ = server.await;
        eprintln!("[{protocol}] server task joined");
        drop(seen);
        eprintln!("[{protocol}] upstream state dropped");
        drop(gateway);
        eprintln!("[{protocol}] gateway dropped");
        drop(store);
        eprintln!("[{protocol}] store dropped");
        drop(engine_directory);
        eprintln!("[{protocol}] engine directory removed");
        eprintln!("[{protocol}] done");
    }
    eprintln!("all protocols verified");
    // 断言已全部通过：被强杀的引擎进程在 Windows 上可能让运行时收尾的阻塞 join 一直等下去，
    // 而这里要验的是桥接行为，不是测试进程的退出路径，所以显式结束进程。
    std::process::exit(0);
}

#[test]
fn invalid_settings_cannot_enable_an_unroutable_gateway() {
    let mut settings = CodexSettings {
        enabled: true,
        ..CodexSettings::default()
    };
    assert!(settings.validate().is_err());
    settings.model_hashes = vec!["hash".into()];
    settings.default_model_hash = Some("hash".into());
    assert!(settings.validate().is_ok());
    settings.model_hashes.push("hash".into());
    assert!(settings.validate().is_err());
    settings.model_hashes.pop();
    settings.port = 0;
    assert!(settings.validate().is_err());
}
