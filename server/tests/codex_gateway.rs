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

/// 端到端：模型库里配 800k，发布的目录逐模型写 800k，config.toml 不再有全局窗口。
///
/// 旧版本把“默认模型”的窗口写成 `model_context_window`：客户端在界面里切到 800k 的
/// 模型后，配置不会更新，客户端仍按默认模型的 286k × 95% 显示（用户看到的 272k），
/// 自动压缩也停在 228k。窗口现在只由目录里的 `context_window` 决定。
#[tokio::test]
async fn a_configured_800k_window_reaches_the_catalog_without_a_global_override() {
    let (directory, store, gateway, id) =
        configured_gateway("responses", "http://127.0.0.1:1".into()).await;
    let model = store.model(&id).await.unwrap().unwrap();
    let mut input = model.config_input("800k model".into(), model.sort_order);
    input.context_window_tokens = Some(800_000);
    store.update_model(&id, &input).await.unwrap();
    let mut settings = store.codex_settings().await.unwrap();
    settings.port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    store.set_codex_settings(settings).await.unwrap();
    gateway.apply().await.unwrap();

    let home = directory.path().join("codex-home");
    let doc = std::fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert!(
        doc.get("model_context_window").is_none(),
        "全局窗口会盖住目录里的逐模型窗口: {doc}"
    );
    assert!(
        doc.get("model_auto_compact_token_limit").is_none(),
        "全局压缩阈值会压住目录里的逐模型阈值: {doc}"
    );

    let catalog: serde_json::Value =
        serde_json::from_slice(&std::fs::read(published_catalog_path(&home)).unwrap()).unwrap();
    let entry = &catalog["models"][0];
    assert_eq!(entry["slug"], id.as_str());
    assert_eq!(entry["context_window"], 800_000);
    assert_eq!(entry["max_context_window"], 800_000);
    assert_eq!(entry["auto_compact_token_limit"], 640_000);
}

#[tokio::test]
async fn adding_a_model_to_an_applied_gateway_refreshes_the_catalog_without_restoring() {
    let (directory, store, gateway, first_id) =
        configured_gateway("responses", "http://127.0.0.1:1".into()).await;
    let first = store.model(&first_id).await.unwrap().unwrap();
    let mut settings = store.codex_settings().await.unwrap();
    settings.port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    store.set_codex_settings(settings).await.unwrap();
    gateway.apply().await.unwrap();

    let mut second_input = first.config_input("Second model".into(), 2);
    second_input.model_id = "vendor-model-second".into();
    let second = store.create_model(&second_input).await.unwrap();
    let home = directory.path().join("codex-home");
    let before = published_catalog_path(&home);

    let mut changed = store.codex_settings().await.unwrap();
    changed.model_hashes.push(second.model_hash.clone());
    let saved = gateway.save_settings(changed).await.unwrap();
    assert_eq!(
        saved.model_hashes,
        vec![first_id.clone(), second.model_hash.clone()]
    );
    assert_eq!(saved.default_model_hash.as_deref(), Some(first_id.as_str()));

    let after = published_catalog_path(&home);
    assert_ne!(before, after, "新增模型后目录没有刷新");
    let catalog: serde_json::Value =
        serde_json::from_slice(&std::fs::read(after).unwrap()).unwrap();
    assert_eq!(catalog["models"].as_array().unwrap().len(), 2, "{catalog}");
    assert!(gateway.status().await.unwrap().configured);

    let mut changed_default = saved;
    changed_default.default_model_hash = Some(second.model_hash);
    assert!(
        gateway.save_settings(changed_default).await.is_err(),
        "接管期间改默认模型必须撤销后重新应用"
    );
    gateway.shutdown().await.unwrap();
}

/// 已发布目录的路径：客户端读的就是 config.toml 里的这个指针。
fn published_catalog_path(home: &std::path::Path) -> std::path::PathBuf {
    let doc = std::fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    std::path::PathBuf::from(doc["model_catalog_json"].as_str().unwrap())
}

/// 断言发布目录里该 slug 记的就是当前模型的路由指纹。
async fn assert_catalog_is_current(
    store: &cursor_server::store::Store,
    home: &std::path::Path,
    id: &str,
) {
    let model = store.model(id).await.unwrap().unwrap();
    let current = cursor_server::codex::catalog::build_catalog(std::slice::from_ref(&model))
        .unwrap()["models"][0]["haxsd_byok_routing_fingerprint"]
        .clone();
    let published: serde_json::Value =
        serde_json::from_slice(&std::fs::read(published_catalog_path(home)).unwrap()).unwrap();
    let entry = published["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["slug"] == id)
        .unwrap_or_else(|| panic!("the published catalog has no {id}: {published}"));
    assert_eq!(entry["haxsd_byok_routing_fingerprint"], current);
}

/// 模型在模型库里被编辑之后目录会过期：请求继续服务（而不是 409），并把目录
/// 刷新成当前配置，客户端下一次会话会读到新目录。
#[tokio::test]
async fn a_model_edited_after_apply_republishes_the_catalog_instead_of_failing() {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let router = Router::new().route(
        "/upstream",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().await.push(body);
                Json(
                    json!({"id":"resp_vendor","object":"response","status":"completed","output":[],
                    "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/upstream", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, store, gateway, id) = configured_gateway("responses", url).await;
    let mut settings = store.codex_settings().await.unwrap();
    settings.port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    store.set_codex_settings(settings).await.unwrap();
    gateway.apply().await.unwrap();
    let home = std::path::PathBuf::from(&store.codex_settings().await.unwrap().home);
    let before = published_catalog_path(&home);
    // 用户在模型库里把思考强度改成 high：路由指纹随之变化，目录从此过期。
    sqlx::query("UPDATE model_configs SET reasoning_effort = 'high' WHERE model_hash = ?")
        .bind(&id)
        .execute(store.pool())
        .await
        .unwrap();
    let response = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header(
                    "authorization",
                    format!("Bearer {}", store.codex_token().await.unwrap()),
                )
                .body(Body::from(
                    json!({"model":id,"stream":false,"input":"go"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(seen.lock().await.len(), 1, "请求没有到达桩上游");
    let after = published_catalog_path(&home);
    assert_ne!(before, after, "目录指针没有换到新文件");
    assert!(!before.exists(), "上一份目录文件没有被删掉");
    assert_catalog_is_current(&store, &home, &id).await;
    // 目录刷新后配置仍算已应用：客户端读到的还是我们发布的路由。
    assert!(gateway.status().await.unwrap().configured);
    gateway.shutdown().await.unwrap();
    task.abort();
}

/// 目录被外部改过（文件名哈希与实际内容不匹配）不是"模型被编辑"：不能自愈。
/// 但请求路径上的刷新是旁路——这次请求仍会发给上游（这里上游不可达，所以是 502），
/// 篡改过的目录保持原样，不会被覆盖。
#[tokio::test]
async fn an_externally_tampered_catalog_is_never_overwritten_by_the_request_path() {
    let (_dir, store, gateway, id) =
        configured_gateway("responses", "http://127.0.0.1:1/responses".into()).await;
    let mut settings = store.codex_settings().await.unwrap();
    settings.port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    store.set_codex_settings(settings).await.unwrap();
    gateway.apply().await.unwrap();
    let home = std::path::PathBuf::from(&store.codex_settings().await.unwrap().home);
    std::fs::write(published_catalog_path(&home), "[]").unwrap();
    let response = gateway
        .router()
        .oneshot(
            Request::post("/v1/responses")
                .header(
                    "authorization",
                    format!("Bearer {}", store.codex_token().await.unwrap()),
                )
                .body(Body::from(
                    serde_json::json!({"model":id,"stream":false,"input":"go"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        response.status(),
        StatusCode::CONFLICT,
        "目录刷新是旁路，失败不能把整轮请求挡在 409"
    );
    assert_eq!(
        std::fs::read_to_string(published_catalog_path(&home)).unwrap(),
        "[]",
        "刷新失败不能覆盖外部改动"
    );
    gateway.shutdown().await.unwrap();
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
async fn an_update_plan_call_is_not_advertised_when_the_request_omits_the_tool() {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};

    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let router = Router::new().route(
        "/chat",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().await.push(body);
                let stream = concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-plan\",\"type\":\"function\",\"function\":{\"name\":\"update_plan\",\"arguments\":\"{\\\"plan\\\":[]}\"}}]},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                    "data: [DONE]\n\n"
                );
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], stream)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/chat", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, store, gateway, id) = configured_gateway("chat", url).await;
    let body = json!({
        "model": id,
        "stream": false,
        "instructions": "## Planning\ncall update_plan when useful\n\n## Other\nkeep this",
        "input": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object"}}]
    });
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
    assert_eq!(output["status"], "completed");
    assert_eq!(output["output"], json!([]));
    let upstream = seen.lock().await;
    let system = upstream[0]["messages"][0]["content"].as_str().unwrap();
    assert!(!system.contains("update_plan"), "{system}");
    assert!(system.contains("## Other"), "{system}");
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

/// 用户在 Codex 自己的界面里切联网搜索（客户端写 `web_search = "live"`）：这不是路由
/// 冲突，应用照旧生效、撤销也仍然可用。状态 JSON 给出 `restorable` 和逐字段的
/// `conflicts`，界面据此说明"哪一项被改成了什么"，而不是把两个按钮都锁死。
#[tokio::test]
async fn a_client_web_search_switch_keeps_the_route_applied_and_restorable() {
    let (dir, store, gateway, _) =
        configured_gateway("responses", "http://127.0.0.1:1/responses".into()).await;
    let home = dir.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), "web_search = \"live\"\n").unwrap();
    let mut settings = store.codex_settings().await.unwrap();
    settings.port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    store.set_codex_settings(settings).await.unwrap();
    gateway.apply().await.unwrap();
    // 客户端自己的开关被写回 live，就像用户在 Codex 界面里点了一下。
    let path = home.join("config.toml");
    let applied = std::fs::read_to_string(&path).unwrap();
    assert!(applied.contains("web_search = \"disabled\""), "{applied}");
    std::fs::write(
        &path,
        applied.replace("web_search = \"disabled\"", "web_search = \"live\""),
    )
    .unwrap();
    let status = gateway.status().await.unwrap();
    assert!(status.configured && !status.config_conflict, "{status:?}");
    assert!(status.restorable, "{status:?}");
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["restorable"], serde_json::json!(true), "{json}");
    assert_eq!(
        json["conflicts"],
        serde_json::json!([{"key": "web_search", "applied": "\"disabled\"", "current": "\"live\""}]),
        "{json}"
    );
    // 撤销仍然可用：受管字段回到接管前，客户端写下的 live 也在其中。
    let restored = gateway.restore().await.unwrap();
    assert!(
        !restored.configured && !restored.config_conflict && !restored.restorable,
        "{restored:?}"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("web_search = \"live\""), "{text}");
    assert!(!text.contains("haxsd-byok"), "{text}");
    gateway.shutdown().await.unwrap();
}

/// 模型在应用之后被编辑（这里改 model_id）时，请求照常服务：网关按模型库里的
/// 当前配置转发，同时把发布目录刷新成这份配置，客户端下一次会话会看到它，所以
/// 切换不是静默的。旧语义是直接 409（"restore, reapply and start a new
/// conversation"）：目录是写给客户端的快照，让它过期的代价不该是整条链路不可用。
#[tokio::test]
async fn changing_an_applied_model_serves_the_turn_and_refreshes_the_published_catalog() {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let router = Router::new().route(
        "/upstream",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().await.push(body);
                Json(
                    json!({"id":"resp_vendor","object":"response","status":"completed","output":[],
                    "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/upstream", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, store, gateway, id) = configured_gateway("responses", url).await;
    let mut settings = store.codex_settings().await.unwrap();
    settings.port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    store.set_codex_settings(settings).await.unwrap();
    assert!(gateway.apply().await.unwrap().configured);
    let home = std::path::PathBuf::from(&store.codex_settings().await.unwrap().home);
    let before = published_catalog_path(&home);
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
                    serde_json::json!({"model":id,"stream":false,"input":"go"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // 请求走的是模型库里的当前配置，而不是目录里记的那份。
    assert_eq!(seen.lock().await[0]["model"], "changed-upstream");
    assert_ne!(before, published_catalog_path(&home), "目录没有刷新");
    assert!(!before.exists(), "上一份目录文件没有被删掉");
    assert_catalog_is_current(&store, &home, &id).await;
    // 这次切换不静默：调用有记录，客户端下次会话读到的目录也已经是新路由。
    assert_eq!(store.llm_calls(10).await.unwrap().len(), 1);
    gateway.shutdown().await.unwrap();
    task.abort();
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
async fn a_gateway_failure_before_any_content_is_retried_on_the_codex_passthrough() {
    use axum::{response::IntoResponse, routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let attempts = std::sync::Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    let router = Router::new().route(
        "/upstream",
        post(move || {
            let counted = counted.clone();
            async move {
                if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    return (StatusCode::BAD_GATEWAY, "gateway is down").into_response();
                }
                Json(
                    json!({"id":"resp_vendor","object":"response","status":"completed","output":[],
                    "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}),
                )
                .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/upstream", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, store, gateway, id) = configured_gateway("responses", url).await;
    let body = json!({"model":id,"stream":false,"input":[{"role":"user","content":"go"}]});
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
    assert_eq!(output["id"], "resp_vendor");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "第一次 502 之后必须再发一次"
    );
    // 重试是同一个调用内部的第二次尝试：记录仍然只有一条，状态码是最新的那次。
    let calls = store.llm_calls(10).await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, "completed");
    assert_eq!(calls[0].http_status, Some(200));
    task.abort();
}

#[tokio::test]
async fn a_persistent_gateway_failure_names_the_upstream_and_the_retry_count() {
    use axum::{response::IntoResponse, routing::post, Router};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let attempts = std::sync::Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    let router = Router::new().route(
        "/upstream",
        post(move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                (StatusCode::BAD_GATEWAY, "gateway is down").into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/upstream", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, store, gateway, id) = configured_gateway("responses", url).await;
    let body = json!({"model":id,"stream":false,"input":[{"role":"user","content":"go"}]});
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
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let output: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    // 界面要能一眼看出这是来源网关的、试过几次的失败。
    assert_eq!(
        output["error"]["message"],
        "upstream HTTP 502 Bad Gateway (after 2 retries): gateway is down"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 3, "首次 + 两次重试");
    let calls = store.llm_calls(10).await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, "error");
    assert_eq!(calls[0].http_status, Some(502));
    let recorded = calls[0].error_message.clone().unwrap_or_default();
    assert!(
        recorded.contains("upstream HTTP 502 Bad Gateway (after 2 retries): gateway is down"),
        "unexpected recorded error: {recorded}"
    );
    task.abort();
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
        // 只有模型配置了思考强度，目录才会宣告档位；配一档让真实引擎读到的
        // 目录带着非空的 supported_reasoning_levels，形状由引擎自己验收。
        sqlx::query(
            "UPDATE model_configs SET reasoning_effort = 'high', anthropic_thinking_effort = 'high' WHERE model_hash = ?",
        )
        .bind(&model)
        .execute(store.pool())
        .await
        .unwrap();
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
                    // 目录条目就是引擎真正接受的那一份：档位形状（对象数组）、
                    // 默认档位和五个可选值都在这里被引擎自己读出来。
                    let loaded = event["result"]["data"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|entry| entry["id"] == model || entry["model"] == model)
                        .unwrap_or_else(|| {
                            panic!("desktop did not load the applied model catalog: {event}")
                        });
                    assert_eq!(loaded["defaultReasoningEffort"], "high", "{event}");
                    let efforts = loaded["supportedReasoningEfforts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|level| level["reasoningEffort"].as_str())
                        .collect::<Vec<_>>();
                    assert_eq!(
                        efforts,
                        ["low", "medium", "high", "xhigh", "max"],
                        "{event}"
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

/// 只认 low…max 的上游：客户端界面里的占位强度必须被网关摘掉，而不是原样转出去 400。
#[tokio::test]
async fn a_vendor_that_rejects_an_unknown_thinking_level_still_serves_the_turn() {
    use axum::{response::IntoResponse, routing::post, Json, Router};
    use serde_json::{json, Value};
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let router = Router::new().route(
        "/upstream",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            async move {
                let effort = body
                    .pointer("/reasoning/effort")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                captured.lock().await.push(body);
                if effort.as_deref().is_some_and(|effort| {
                    !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max")
                }) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error":{"message":format!("unsupported reasoning effort {effort:?}")}})),
                    )
                        .into_response();
                }
                Json(json!({"id":"resp_vendor","object":"response","status":"completed","output":[],
                    "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}))
                .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/upstream", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, store, gateway, id) = configured_gateway("responses", url).await;
    let token = store.codex_token().await.unwrap();
    let send = |body: Value| {
        let gateway = gateway.clone();
        let token = token.clone();
        async move {
            gateway
                .router()
                .oneshot(
                    Request::post("/v1/responses")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap()
        }
    };
    let placeholder = send(json!({"model":id,"stream":false,"input":"go",
        "reasoning":{"effort":"none"}}))
    .await;
    assert_eq!(placeholder.status(), StatusCode::OK);
    let effort = send(json!({"model":id,"stream":false,"input":"go",
        "reasoning":{"effort":"low"}}))
    .await;
    assert_eq!(effort.status(), StatusCode::OK);
    sqlx::query("UPDATE model_configs SET reasoning_effort = 'high' WHERE model_hash = ?")
        .bind(&id)
        .execute(store.pool())
        .await
        .unwrap();
    let configured = send(json!({"model":id,"stream":false,"input":"go",
        "reasoning":{"effort":"none"}}))
    .await;
    assert_eq!(configured.status(), StatusCode::OK);
    let seen = seen.lock().await;
    assert_eq!(seen.len(), 3);
    assert!(seen[0].get("reasoning").is_none(), "{}", seen[0]);
    assert_eq!(seen[1]["reasoning"]["effort"], "low");
    assert_eq!(seen[2]["reasoning"]["effort"], "high");
    assert_eq!(store.llm_calls(10).await.unwrap().len(), 3);
    drop(seen);
    task.abort();
}

/// 真实引擎验收：客户端切换模型（此前每个请求 409）与客户端写的思考强度占位值
/// （此前上游 400）都不再中断任务。
async fn two_model_gateway(
    upstream_url: String,
) -> (
    tempfile::TempDir,
    cursor_server::store::Store,
    CodexGateway,
    String,
    String,
) {
    use cursor_server::model::{ModelConfigInput, ModelType};
    let (directory, store) = fixtures::temp_store().await;
    let mut hashes = Vec::new();
    for model_id in ["vendor-model-a", "vendor-model-b"] {
        let model = store
            .create_model(&ModelConfigInput {
                sort_order: 0,
                display_name: "Codex test model".into(),
                group_name: None,
                model_type: ModelType::OpenAi,
                base_url: upstream_url.clone(),
                use_full_url: true,
                api_key: "vendor-secret".into(),
                tooltip_data: "Codex integration test".into(),
                model_id: model_id.into(),
                reasoning_effort: None,
                openai_endpoint: "/v1/responses".into(),
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
        hashes.push(model.model_hash);
    }
    store
        .set_codex_settings(CodexSettings {
            enabled: true,
            model_hashes: hashes.clone(),
            default_model_hash: Some(hashes[0].clone()),
            home: directory.path().join("codex-home").display().to_string(),
            ..CodexSettings::default()
        })
        .await
        .unwrap();
    let gateway = CodexGateway::new(store.clone(), NetworkClients::new(store.clone()));
    (
        directory,
        store,
        gateway,
        hashes[0].clone(),
        hashes[1].clone(),
    )
}

#[tokio::test]
#[ignore = "需要通过 HAXSD_BYOK_CODEX_TEST_BINARY 指定现有桌面客户端的内置引擎"]
async fn the_real_engine_survives_a_client_model_switch_and_a_placeholder_thinking_level() {
    use axum::{response::IntoResponse, routing::post, Json, Router};
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let binary = std::env::var_os("HAXSD_BYOK_CODEX_TEST_BINARY")
        .expect("set the installed desktop engine path");
    // 上游像只认 low…max 的真实模型：别的强度直接 400，并记录实际收到的正文。
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let app = Router::new().route(
        "/inference",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            async move {
                let effort = body
                    .pointer("/reasoning/effort")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                eprintln!(
                    "upstream request: model={} reasoning.effort={:?}",
                    body["model"], effort
                );
                captured.lock().await.push(body);
                if effort.as_deref().is_some_and(|effort| {
                    !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max")
                }) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error":{"type":"invalid_request_error",
                            "message":format!("unsupported reasoning effort {effort:?}")}})),
                    )
                        .into_response();
                }
                let item = json!({"id":"msg_final","type":"message","role":"assistant",
                    "status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]});
                let text = [
                    json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress","output":[]}}),
                    json!({"type":"response.output_item.done","output_index":0,"item":item}),
                    json!({"type":"response.completed","response":{"id":"resp_1","status":"completed",
                        "output":[item],"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}),
                ]
                .iter()
                .map(|chunk| format!("data: {chunk}\n\n"))
                .collect::<String>();
                ([("content-type", "text/event-stream")], text).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/inference", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (_directory, store, gateway, first, second) = two_model_gateway(url).await;
    let mut settings = store.codex_settings().await.unwrap();
    let home = std::path::PathBuf::from(&settings.home);
    settings.port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    store.set_codex_settings(settings.clone()).await.unwrap();
    gateway.apply().await.unwrap();
    // 客户端在它自己的界面里换模型并改思考强度：就是重写这两行。
    let path = home.join("config.toml");
    let applied = std::fs::read_to_string(&path).unwrap();
    let switched = applied
        .replace(
            &format!("model = \"{first}\""),
            &format!("model = \"{second}\""),
        )
        .replace(
            "model_provider = \"haxsd-byok\"",
            "model_reasoning_effort = \"none\"\nmodel_provider = \"haxsd-byok\"",
        );
    assert_ne!(switched, applied);
    std::fs::write(&path, &switched).unwrap();
    let status = cursor_server::codex::config::inspect(&home).unwrap();
    eprintln!(
        "after the client switch: applied={} conflict={} message={}",
        status.applied, status.conflict, status.message
    );
    // 直接量错误码：此前每次请求都是 409 configuration error。
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let direct = client
        .post(format!("http://127.0.0.1:{}/v1/responses", settings.port))
        .header(
            "authorization",
            format!("Bearer {}", store.codex_token().await.unwrap()),
        )
        .json(&json!({"model":second,"stream":false,"store":false,
            "input":[{"role":"user","content":"hello"}],"reasoning":{"effort":"none"}}))
        .send()
        .await
        .unwrap();
    eprintln!(
        "gateway answered HTTP {} after the client switch",
        direct.status()
    );
    assert!(status.applied && !status.conflict, "{}", status.message);
    assert!(!gateway.status().await.unwrap().config_conflict);
    assert_eq!(direct.status(), StatusCode::OK);
    let engine_directory =
        tempfile::tempdir_in(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"))
            .unwrap();
    let workspace = engine_directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
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
        json!({"id":3,"method":"thread/start","params":{"cwd":workspace,"model":second,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true,"config":{"windows.sandbox":"unelevated"}}}),
    ];
    let mut thread = None;
    for request in requests {
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
        loop {
            let line = tokio::time::timeout(std::time::Duration::from_secs(30), stdout.next_line())
                .await
                .expect("desktop RPC timed out")
                .unwrap()
                .expect("desktop engine terminated");
            let event: Value = serde_json::from_str(&line).unwrap();
            if event["id"] != request["id"] {
                continue;
            }
            assert!(event.get("error").is_none(), "RPC error: {event}");
            if request["id"] == 2 {
                assert!(
                    event["result"]["data"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["id"] == second),
                    "desktop did not load the applied catalog: {event}"
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
    let request = json!({"id":4,"method":"turn/start","params":{"threadId":thread.unwrap(),
        "input":[{"type":"text","text":"Reply with the single word done."}]}});
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        let line = tokio::time::timeout_at(deadline, stdout.next_line())
            .await
            .expect("the switched route never completed a turn")
            .unwrap()
            .expect("desktop engine terminated");
        let event: Value = serde_json::from_str(&line).unwrap();
        assert!(event.get("error").is_none(), "RPC error: {event}");
        if event["method"] == "turn/completed" {
            assert_eq!(event["params"]["turn"]["status"], "completed", "{event}");
            break;
        }
    }
    // 对照：客户端把强度设成上游认识的档位时，它确实会进入上游请求；这样上面
    // 观察到的“没有 effort”才能说明是网关摘掉了占位值，而不是客户端从不发送强度。
    let control = json!({"id":5,"method":"thread/start","params":{"cwd":workspace,"model":second,
        "approvalPolicy":"never","sandbox":"read-only","ephemeral":true,
        "config":{"windows.sandbox":"unelevated","model_reasoning_effort":"low"}}});
    stdin
        .write_all(format!("{control}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let control_thread = loop {
        let line = tokio::time::timeout(std::time::Duration::from_secs(30), stdout.next_line())
            .await
            .expect("desktop RPC timed out")
            .unwrap()
            .expect("desktop engine terminated");
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["id"] != 5 {
            continue;
        }
        assert!(event.get("error").is_none(), "RPC error: {event}");
        break event
            .pointer("/result/thread/id")
            .and_then(Value::as_str)
            .map(String::from);
    };
    let control_turn = json!({"id":6,"method":"turn/start","params":{
        "threadId":control_thread.unwrap(),
        "input":[{"type":"text","text":"Reply with the single word done."}]}});
    stdin
        .write_all(format!("{control_turn}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        let line = tokio::time::timeout_at(deadline, stdout.next_line())
            .await
            .expect("the control turn never completed")
            .unwrap()
            .expect("desktop engine terminated");
        let event: Value = serde_json::from_str(&line).unwrap();
        assert!(event.get("error").is_none(), "RPC error: {event}");
        if event["method"] == "turn/completed" {
            assert_eq!(event["params"]["turn"]["status"], "completed", "{event}");
            break;
        }
    }
    let requests = seen.lock().await;
    assert!(
        requests
            .iter()
            .any(|body| body["model"] == "vendor-model-b"),
        "the switched model never reached the upstream: {requests:?}"
    );
    let efforts = requests
        .iter()
        .filter_map(|body| body.pointer("/reasoning/effort").and_then(Value::as_str))
        .collect::<Vec<_>>();
    eprintln!("upstream saw reasoning efforts: {efforts:?}");
    assert!(
        !efforts
            .iter()
            .any(|effort| matches!(*effort, "none" | "off" | "disabled")),
        "a placeholder effort reached the vendor: {efforts:?}"
    );
    assert_eq!(store.llm_calls(10).await.unwrap().len(), requests.len());
    drop(requests);
    let _ = child.kill().await;
    let _ = child.wait().await;
    stderr_task.abort();
    let _ = stderr_task.await;
    gateway.shutdown().await.unwrap();
    server.abort();
    drop(gateway);
    drop(store);
    drop(engine_directory);
    eprintln!("client model switch and placeholder effort verified");
    // 被强杀的引擎进程在 Windows 上可能让运行时收尾的阻塞 join 一直等下去。
    std::process::exit(0);
}
