//! Codex 桥接的行为测试：Responses 请求 ↔ Chat/Anthropic 上游请求，上游分片 → Responses SSE。
//!
//! 断言只看桥的输入与输出（请求 JSON、事件 JSON），不读取实现文件文本。
use cursor_server::codex::bridge::{
    translate_request, BridgeRequest, BridgeStream, UpstreamFormat,
};
use serde_json::{json, Value};

const REPLAY_PREFIX: &str = "haxsd-codex-replay-v1:";

fn chat(source: Value) -> BridgeRequest {
    translate_request(&source, UpstreamFormat::Chat, "vendor-model", 4096).unwrap()
}

fn anthropic(source: Value) -> BridgeRequest {
    translate_request(&source, UpstreamFormat::Anthropic, "claude-vendor", 4096).unwrap()
}

fn stream(mapped: BridgeRequest, format: UpstreamFormat, model: &str) -> BridgeStream {
    BridgeStream::new(format, model.to_string(), mapped.tools)
}

/// 依次喂入上游分片后再调用 finish；finish 幂等，所以终态已经出现时它是空操作。
fn drain(stream: &mut BridgeStream, chunks: &[Value]) -> Vec<Value> {
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(stream.push(chunk).expect("chunk is accepted"));
    }
    events.extend(stream.finish().expect("stream has a terminal"));
    events
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| event["type"].as_str().map(str::to_string))
        .collect()
}

fn events_of<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["type"] == kind)
        .collect()
}

fn terminal(events: &[Value]) -> Value {
    events
        .iter()
        .find(|event| {
            matches!(
                event["type"].as_str(),
                Some("response.completed" | "response.incomplete" | "response.failed")
            )
        })
        .expect("stream produced a terminal event")["response"]
        .clone()
}

fn output_items(response: &Value) -> Vec<Value> {
    response["output"]
        .as_array()
        .expect("output is an array")
        .clone()
}

fn item_of_type(response: &Value, kind: &str) -> Value {
    output_items(response)
        .into_iter()
        .find(|item| item["type"] == kind)
        .unwrap_or_else(|| panic!("response output has no {kind} item"))
}

// ===================== 请求转换：指令与正文 =====================

#[test]
fn instructions_and_developer_history_merge_into_one_chat_system_message() {
    let mapped = chat(json!({
        "instructions": "base instructions",
        "store": false,
        "input": [
            {"type": "message", "role": "developer", "content": "dev rules"},
            {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "legacy system"}]},
            {"type": "message", "role": "user", "content": "hello"}
        ]
    }));
    assert_eq!(
        mapped.body["messages"][0],
        json!({"role": "system", "content": "base instructions\n\ndev rules\n\nlegacy system"})
    );
    assert_eq!(
        mapped.body["messages"][1],
        json!({"role": "user", "content": "hello"})
    );
    assert_eq!(mapped.body["messages"].as_array().unwrap().len(), 2);
}

#[test]
fn anthropic_hoists_instructions_and_developer_history_into_system() {
    let mapped = anthropic(json!({
        "instructions": "base instructions",
        "input": [
            {"type": "message", "role": "developer", "content": "dev rules"},
            {"type": "message", "role": "user", "content": "hello"}
        ]
    }));
    assert_eq!(mapped.body["system"], "base instructions\n\ndev rules");
    assert_eq!(mapped.body["max_tokens"], 4096);
    assert_eq!(mapped.body["stream"], true);
    assert_eq!(
        mapped.body["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])
    );
}

#[test]
fn assistant_text_history_survives_both_formats() {
    let source = json!({
        "input": [
            {"role": "user", "content": "question"},
            {"role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}
        ]
    });
    assert_eq!(
        chat(source.clone()).body["messages"][1],
        json!({"role": "assistant", "content": "answer"})
    );
    assert_eq!(
        anthropic(source).body["messages"][1]["content"][0],
        json!({"type": "text", "text": "answer"})
    );
}

#[test]
fn images_map_to_chat_parts_and_anthropic_sources() {
    let source = json!({
        "input": [{"role": "user", "content": [
            {"type": "input_text", "text": "look"},
            {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high"},
            {"type": "input_image", "image_url": "https://example.test/shot.png"}
        ]}]
    });
    let mapped = chat(source.clone());
    assert_eq!(
        mapped.body["messages"][0]["content"],
        json!([
            {"type": "text", "text": "look"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA", "detail": "high"}},
            {"type": "image_url", "image_url": {"url": "https://example.test/shot.png"}}
        ])
    );
    let mapped = anthropic(source);
    assert_eq!(
        mapped.body["messages"][0]["content"][1],
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}})
    );
    assert_eq!(
        mapped.body["messages"][0]["content"][2],
        json!({"type": "image", "source": {"type": "url", "url": "https://example.test/shot.png"}})
    );
}

// ===================== 请求转换：工具声明与映射 =====================

#[test]
fn custom_tool_becomes_object_function_with_string_input() {
    let mapped = chat(json!({
        "input": [{"role": "user", "content": "修改文件"}],
        "tools": [{"type": "custom", "name": "apply_patch", "description": "patch"}],
        "stream": true
    }));
    assert_eq!(mapped.body["tools"][0]["type"], "function");
    assert_eq!(mapped.body["tools"][0]["function"]["name"], "apply_patch");
    assert_eq!(
        mapped.body["tools"][0]["function"]["parameters"]["properties"]["input"]["type"],
        "string"
    );
    assert_eq!(
        mapped.body["tools"][0]["function"]["parameters"]["required"],
        json!(["input"])
    );
    assert!(mapped.body["tools"][0]["function"]["description"]
        .as_str()
        .unwrap()
        .contains("patch"));
}

#[test]
fn function_tools_keep_their_declared_name_and_schema() {
    let mapped = chat(json!({
        "input": "run it",
        "tools": [{"type": "function", "name": "shell", "description": "run a command",
            "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}}]
    }));
    assert_eq!(mapped.body["tools"][0]["function"]["name"], "shell");
    assert_eq!(
        mapped.body["tools"][0]["function"]["parameters"],
        json!({"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]})
    );
    assert_eq!(
        mapped.body["messages"][0],
        json!({"role": "user", "content": "run it"})
    );
}

#[test]
fn namespace_tools_flatten_to_a_reversible_upstream_name() {
    let source = json!({
        "input": [
            {"role": "user", "content": "list"},
            {"type": "function_call", "name": "read", "namespace": "mcp__files", "call_id": "call_1", "arguments": "{\"path\":\"a\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "contents"}
        ],
        "tools": [{"type": "namespace", "name": "mcp__files", "tools": [
            {"type": "function", "name": "read", "description": "read a file", "parameters": {"type": "object", "properties": {}}}
        ]}]
    });
    let mapped = chat(source.clone());
    assert_eq!(
        mapped.body["tools"][0]["function"]["name"],
        "mcp__files__read"
    );
    assert_eq!(
        mapped.body["messages"][1]["tool_calls"][0],
        json!({"id": "call_1", "type": "function",
            "function": {"name": "mcp__files__read", "arguments": "{\"path\":\"a\"}"}})
    );
    // Anthropic 用同一套扁平名，才能让历史 tool_use 与声明对得上。
    let mapped = anthropic(source);
    assert_eq!(mapped.body["tools"][0]["name"], "mcp__files__read");
    assert_eq!(
        mapped.body["messages"][1]["content"][0]["name"],
        "mcp__files__read"
    );
}

#[test]
fn namespace_collisions_keep_both_tools_and_stay_reversible() {
    let mapped = chat(json!({
        "input": "x",
        "tools": [
            {"type": "namespace", "name": "mcp", "tools": [
                {"type": "function", "name": "list", "description": "namespaced", "parameters": {"type": "object", "properties": {}}}
            ]},
            {"type": "function", "name": "mcp__list", "description": "plain", "parameters": {"type": "object", "properties": {}}}
        ]
    }));
    let names: Vec<&str> = mapped.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "neither tool may be dropped");
    assert_eq!(names[0], "mcp__list");
    assert!(
        names[1].starts_with("mcp__list__"),
        "collision gets a stable suffix: {}",
        names[1]
    );
    assert!(names[1].len() <= 64, "{}", names[1]);
}

#[test]
fn custom_tool_history_becomes_a_function_call_and_result_not_plain_text() {
    let mapped = chat(json!({
        "input": [
            {"role": "user", "content": "patch"},
            {"type": "custom_tool_call", "call_id": "c1", "name": "apply_patch", "input": "*** Begin Patch"},
            {"type": "custom_tool_call_output", "call_id": "c1", "output": "Success"}
        ],
        "tools": [{"type": "custom", "name": "apply_patch"}]
    }));
    assert_eq!(mapped.body["messages"][1]["tool_calls"][0]["id"], "c1");
    let arguments: Value = serde_json::from_str(
        mapped.body["messages"][1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(arguments["input"], "*** Begin Patch");
    assert_eq!(
        mapped.body["messages"][2],
        json!({"role": "tool", "tool_call_id": "c1", "content": "Success"})
    );
}

#[test]
fn anthropic_custom_tool_history_wraps_raw_input_in_tool_use() {
    let mapped = anthropic(json!({
        "input": [
            {"role": "user", "content": "patch"},
            {"type": "custom_tool_call", "call_id": "c1", "name": "apply_patch", "input": "*** Begin Patch"},
            {"type": "custom_tool_call_output", "call_id": "c1", "output": "Success"}
        ],
        "tools": [{"type": "custom", "name": "apply_patch", "description": "patch"}]
    }));
    assert_eq!(
        mapped.body["tools"][0]["input_schema"]["properties"]["input"]["type"],
        "string"
    );
    assert_eq!(
        mapped.body["messages"][1]["content"][0],
        json!({"type": "tool_use", "id": "c1", "name": "apply_patch", "input": {"input": "*** Begin Patch"}})
    );
    assert_eq!(
        mapped.body["messages"][2],
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "c1", "content": "Success"}]})
    );
}

#[test]
fn parallel_tool_results_stay_grouped_in_one_anthropic_user_turn() {
    let mapped = anthropic(json!({
        "input": [
            {"role": "user", "content": "go"},
            {"type": "function_call", "call_id": "a", "name": "f", "arguments": "{}"},
            {"type": "function_call", "call_id": "b", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "a", "output": "first"},
            {"type": "function_call_output", "call_id": "b", "output": "second"}
        ],
        "tools": [{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}}]
    }));
    assert_eq!(
        mapped.body["messages"][1]["content"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        mapped.body["messages"][2]["content"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(mapped.body["messages"][2]["content"][0]["tool_use_id"], "a");
    assert_eq!(mapped.body["messages"][2]["content"][1]["tool_use_id"], "b");
}

#[test]
fn instruction_messages_between_parallel_tool_results_preserve_complete_history() {
    for role in ["developer", "system"] {
        let source = json!({
            "input": [
                {"role": "user", "content": "inspect images"},
                {"type": "function_call", "call_id": "a", "name": "f", "arguments": "{}"},
                {"type": "function_call", "call_id": "b", "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "a", "output": "first image"},
                {"role": role, "content": "Image in the preceding tool output was resized."},
                {"type": "function_call_output", "call_id": "b", "output": "second image"},
                {"role": "user", "content": "continue"}
            ],
            "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}]
        });
        for format in [UpstreamFormat::Chat, UpstreamFormat::Anthropic] {
            let mapped = translate_request(&source, format, "vendor", 4096).unwrap();
            let messages = mapped.body["messages"].as_array().unwrap();
            match format {
                UpstreamFormat::Chat => {
                    assert!(messages[0]["content"]
                        .as_str()
                        .unwrap()
                        .contains("was resized"));
                    assert_eq!(messages[2]["tool_calls"].as_array().unwrap().len(), 2);
                    assert_eq!(messages[3]["tool_call_id"], "a");
                    assert_eq!(messages[3]["content"], "first image");
                    assert_eq!(messages[4]["tool_call_id"], "b");
                    assert_eq!(messages[4]["content"], "second image");
                }
                UpstreamFormat::Anthropic => {
                    assert!(mapped.body["system"].to_string().contains("was resized"));
                    assert_eq!(messages[1]["content"].as_array().unwrap().len(), 2);
                    assert_eq!(messages[2]["content"][0]["tool_use_id"], "a");
                    assert_eq!(messages[2]["content"][0]["content"], "first image");
                    assert_eq!(messages[2]["content"][1]["tool_use_id"], "b");
                    assert_eq!(messages[2]["content"][1]["content"], "second image");
                }
            }
        }
    }
}

#[test]
fn instruction_messages_do_not_hide_missing_tool_results() {
    for role in ["developer", "system", "user", "assistant"] {
        let source = json!({
            "input": [
                {"role": "user", "content": "go"},
                {"type": "function_call", "call_id": "a", "name": "f", "arguments": "{}"},
                {"role": role, "content": "continue"}
            ],
            "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}]
        });
        for format in [UpstreamFormat::Chat, UpstreamFormat::Anthropic] {
            assert!(translate_request(&source, format, "vendor", 4096).is_err());
        }
    }
}

#[test]
fn orphan_tool_results_are_explicitly_rejected() {
    let source = json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call_output", "call_id": "gone", "output": "stale output"}
        ],
        "tools": [{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}}]
    });
    for format in [UpstreamFormat::Chat, UpstreamFormat::Anthropic] {
        assert!(translate_request(&source, format, "vendor", 4096).is_err());
    }
}

#[test]
fn trailing_unanswered_tool_call_is_rejected_instead_of_dropped() {
    // 未配对的尾随 tool_use/tool_calls 会让 Chat 与 Anthropic 上游直接 400。
    let source = json!({
        "input": [
            {"role": "user", "content": "go"},
            {"type": "function_call", "call_id": "a", "name": "f", "arguments": "{}"}
        ],
        "tools": [{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}}]
    });
    for format in [UpstreamFormat::Chat, UpstreamFormat::Anthropic] {
        assert!(translate_request(&source, format, "vendor", 4096).is_err());
    }
}

// ===================== 请求转换：tool_choice / parallel / 推理 =====================

fn choice_tools() -> Value {
    json!([
        {"type": "namespace", "name": "mcp", "tools": [
            {"type": "function", "name": "read", "parameters": {"type": "object", "properties": {}}}
        ]},
        {"type": "custom", "name": "apply_patch"}
    ])
}

#[test]
fn chat_tool_choice_maps_to_openai_forms() {
    for (choice, expected) in [
        (json!("required"), json!("required")),
        (json!("auto"), json!("auto")),
        (json!("none"), json!("none")),
        (
            json!({"type": "function", "name": "read", "namespace": "mcp"}),
            json!({"type": "function", "function": {"name": "mcp__read"}}),
        ),
        (
            json!({"type": "custom", "name": "apply_patch"}),
            json!({"type": "function", "function": {"name": "apply_patch"}}),
        ),
    ] {
        let mapped = chat(json!({"input": "x", "tools": choice_tools(), "tool_choice": choice}));
        assert_eq!(mapped.body["tool_choice"], expected, "choice {choice}");
    }
}

#[test]
fn anthropic_tool_choice_maps_to_native_forms() {
    for (choice, expected) in [
        (json!("required"), json!({"type": "any"})),
        (json!("auto"), json!({"type": "auto"})),
        (json!("none"), json!({"type": "none"})),
        (
            json!({"type": "function", "name": "read", "namespace": "mcp"}),
            json!({"type": "tool", "name": "mcp__read"}),
        ),
        (
            json!({"type": "custom", "name": "apply_patch"}),
            json!({"type": "tool", "name": "apply_patch"}),
        ),
    ] {
        let mapped =
            anthropic(json!({"input": "x", "tools": choice_tools(), "tool_choice": choice}));
        assert_eq!(mapped.body["tool_choice"], expected, "choice {choice}");
    }
}

#[test]
fn parallel_tool_calls_is_forwarded_per_protocol() {
    let tools = json!([{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}}]);
    let mapped = chat(json!({"input": "x", "tools": tools.clone(), "parallel_tool_calls": false}));
    assert_eq!(mapped.body["parallel_tool_calls"], false);
    let mapped = anthropic(json!({"input": "x", "tools": tools, "parallel_tool_calls": false}));
    assert_eq!(
        mapped.body["tool_choice"],
        json!({"type": "auto", "disable_parallel_tool_use": true})
    );
}

#[test]
fn chat_replays_reasoning_content_on_the_tool_call_message() {
    let mapped = chat(json!({
        "input": [
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "weigh options"}]},
            {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"}
        ],
        "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object", "properties": {}}}]
    }));
    let assistant = &mapped.body["messages"][0];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["reasoning_content"], "weigh options");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "shell");
    // 普通消息不写推理字段。
    assert!(mapped.body["messages"][2]
        .get("reasoning_content")
        .is_none());
}

#[test]
fn chat_replays_reasoning_attached_to_the_function_call_item() {
    let mapped = chat(json!({
        "input": [
            {"role": "user", "content": "go"},
            {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}", "reasoning_content": "kept by the client"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"}
        ],
        "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object", "properties": {}}}]
    }));
    assert_eq!(
        mapped.body["messages"][1]["reasoning_content"],
        "kept by the client"
    );
}

#[test]
fn chat_preserves_missing_reasoning_instead_of_fabricating_it() {
    let mapped = chat(json!({
        "input": [
            {"role": "user", "content": "run"},
            {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"}
        ],
        "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object", "properties": {}}}]
    }));
    let assistant = &mapped.body["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert!(
        assistant.get("reasoning_content").is_none(),
        "missing reasoning must not be fabricated"
    );
    assert!(mapped.body["messages"][0]
        .get("reasoning_content")
        .is_none());
}

#[test]
fn chat_reasoning_effort_is_forwarded_only_when_meaningful() {
    let mapped = chat(json!({"input": "x", "reasoning": {"effort": "high"}}));
    assert_eq!(mapped.body["reasoning_effort"], "high");
    let mapped = chat(json!({"input": "x", "reasoning": {"effort": "none"}}));
    assert!(
        mapped.body.get("reasoning_effort").is_none(),
        "none/off 不是合法的 OpenAI effort"
    );
}

#[test]
fn anthropic_thinking_budget_follows_reasoning_effort() {
    let source = |effort: &str| {
        json!({"input": "x", "reasoning": {"effort": effort},
            "tools": [{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}}]})
    };
    let high = translate_request(
        &source("high"),
        UpstreamFormat::Anthropic,
        "claude-vendor",
        65_536,
    )
    .unwrap();
    assert_eq!(
        high.body["thinking"],
        json!({"type": "enabled", "budget_tokens": 16384})
    );
    let medium = translate_request(
        &source("medium"),
        UpstreamFormat::Anthropic,
        "claude-vendor",
        65_536,
    )
    .unwrap();
    assert_eq!(medium.body["thinking"]["budget_tokens"], 8192);
    let off = anthropic(json!({"input": "x", "reasoning": {"effort": "none"}, "temperature": 0.7}));
    assert!(off.body.get("thinking").is_none());
    assert_eq!(
        off.body["temperature"], 0.7,
        "关掉 thinking 时采样参数照常发送"
    );
    // thinking 与采样参数互斥：开启后不能再发 temperature。
    let mapped =
        anthropic(json!({"input": "x", "reasoning": {"effort": "high"}, "temperature": 0.7}));
    assert!(mapped.body.get("temperature").is_none());
}

#[test]
fn anthropic_thinking_budget_is_capped_by_max_tokens() {
    let source = json!({"input": "x", "reasoning": {"effort": "high"}});
    let mapped =
        translate_request(&source, UpstreamFormat::Anthropic, "claude-vendor", 4096).unwrap();
    assert_eq!(
        mapped.body["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    let tiny =
        translate_request(&source, UpstreamFormat::Anthropic, "claude-vendor", 1024).unwrap();
    assert!(
        tiny.body.get("thinking").is_none(),
        "预算不足 1024 时不应假装开启 extended thinking"
    );
}

#[test]
fn anthropic_forced_tool_choice_disables_thinking_instead_of_degrading() {
    let mapped = anthropic(json!({
        "input": "x",
        "reasoning": {"effort": "high"},
        "tool_choice": "required",
        "tools": [{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}}]
    }));
    assert_eq!(mapped.body["tool_choice"], json!({"type": "any"}));
    assert!(mapped.body.get("thinking").is_none());
}

#[test]
fn anthropic_tool_continuation_requires_signed_thinking_to_keep_thinking_on() {
    let unsigned = anthropic(json!({
        "input": [
            {"role": "user", "content": "go"},
            {"type": "function_call", "call_id": "a", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "a", "output": "ok"}
        ],
        "reasoning": {"effort": "high"},
        "tools": [{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}}]
    }));
    assert!(
        unsigned.body.get("thinking").is_none(),
        "没有可回放的签名 thinking 时继续开启 thinking 会被 Anthropic 400"
    );
}

// ===================== 请求转换：明确拒绝 =====================

#[test]
fn unsupported_responses_semantics_are_rejected_for_both_protocols() {
    let tools = json!([{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}}]);
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "previous_response_id",
            json!({"previous_response_id": "resp_1", "input": "hi"}),
            "previous_response_id",
        ),
        ("store", json!({"store": true, "input": "hi"}), "store"),
        (
            "remote compaction",
            json!({"input": [{"type": "compaction", "encrypted_content": "gAAAA"}], "tools": tools.clone()}),
            "compaction",
        ),
        (
            "compaction trigger",
            json!({"input": "hi", "compaction_trigger": "auto"}),
            "compaction",
        ),
        (
            "hosted web search",
            json!({"input": "hi", "tools": [{"type": "web_search"}]}),
            "web_search",
        ),
        (
            "hosted mcp",
            json!({"input": "hi", "tools": [{"type": "mcp", "server_label": "s"}]}),
            "mcp",
        ),
        (
            "local shell call",
            json!({"input": [{"type": "local_shell_call", "call_id": "c", "action": {}}]}),
            "local_shell_call",
        ),
        (
            "computer call",
            json!({"input": [{"type": "computer_call", "call_id": "c"}], "tools": tools.clone()}),
            "computer_call",
        ),
        (
            "allowed_tools choice",
            json!({"input": "hi", "tools": tools.clone(),
            "tool_choice": {"type": "allowed_tools", "mode": "required", "tools": []}}),
            "tool_choice",
        ),
        (
            "file input",
            json!({"input": [{"role": "user", "content": [{"type": "input_file", "file_id": "f"}]}]}),
            "input_file",
        ),
    ];
    for (label, source, keyword) in cases {
        for format in [UpstreamFormat::Chat, UpstreamFormat::Anthropic] {
            let error = translate_request(&source, format, "vendor-model", 4096)
                .expect_err(&format!("{label} must be rejected for {format:?}"))
                .to_string();
            assert!(
                error.contains(keyword),
                "{label}/{format:?} error should name the field: {error}"
            );
        }
    }
}

#[test]
fn requests_without_input_are_rejected() {
    let error = translate_request(
        &json!({"tools": []}),
        UpstreamFormat::Chat,
        "vendor-model",
        4096,
    )
    .expect_err("empty request has nothing to send")
    .to_string();
    assert!(error.contains("input"), "{error}");
}

// ===================== Chat 上游分片 → Responses 事件 =====================

fn chat_chunk(delta: Value, finish: Value) -> Value {
    json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1_700_000_000u64,
        "model": "vendor-model", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

#[test]
fn chat_text_stream_emits_full_lifecycle_and_terminal_usage() {
    let mapped = chat(json!({"input": "hi", "stream": true}));
    assert_eq!(mapped.body["stream"], true);
    assert_eq!(
        mapped.body["stream_options"],
        json!({"include_usage": true})
    );
    assert_eq!(mapped.body["max_tokens"], 4096);
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let events = drain(
        &mut bridge,
        &[
            chat_chunk(
                json!({"role": "assistant", "content": "Hello"}),
                Value::Null,
            ),
            chat_chunk(json!({"content": " world"}), Value::Null),
            chat_chunk(json!({}), json!("stop")),
            json!({"id": "chatcmpl-1", "choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}}),
        ],
    );
    let kinds = types(&events);
    for kind in [
        "response.created",
        "response.in_progress",
        "response.output_item.added",
        "response.content_part.added",
        "response.output_text.delta",
        "response.output_text.done",
        "response.output_item.done",
        "response.completed",
    ] {
        assert!(
            kinds.contains(&kind.to_string()),
            "missing {kind} in {kinds:?}"
        );
    }
    let deltas: Vec<&str> = events_of(&events, "response.output_text.delta")
        .iter()
        .map(|event| event["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, vec!["Hello", " world"]);
    let response = terminal(&events);
    assert_eq!(response["status"], "completed");
    let item = item_of_type(&response, "message");
    assert_eq!(item["content"][0]["text"], "Hello world");
    assert_eq!(item["role"], "assistant");
    assert_eq!(response["usage"]["input_tokens"], 7);
    assert_eq!(response["usage"]["output_tokens"], 2);
    assert_eq!(response["usage"]["total_tokens"], 9);
    assert!(bridge.is_finished());
}

#[test]
fn chat_function_call_stream_restores_call_id_name_and_arguments() {
    let mapped = chat(
        json!({"input": "go", "tools": [{"type": "function", "name": "shell",
        "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}}]}),
    );
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let events = drain(
        &mut bridge,
        &[
            chat_chunk(
                json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
            "function": {"name": "shell", "arguments": "{\"cmd\":"}}]}),
                Value::Null,
            ),
            chat_chunk(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"ls\"}"}}]}),
                json!("tool_calls"),
            ),
        ],
    );
    let added = events_of(&events, "response.output_item.added");
    assert_eq!(added[0]["item"]["type"], "function_call");
    assert_eq!(added[0]["item"]["call_id"], "call_a");
    assert_eq!(added[0]["item"]["name"], "shell");
    let delta: String = events_of(&events, "response.function_call_arguments.delta")
        .iter()
        .map(|event| event["delta"].as_str().unwrap())
        .collect();
    assert_eq!(delta, "{\"cmd\":\"ls\"}");
    let done = events_of(&events, "response.function_call_arguments.done");
    assert_eq!(done[0]["arguments"], "{\"cmd\":\"ls\"}");
    let response = terminal(&events);
    assert_eq!(response["status"], "completed");
    let item = item_of_type(&response, "function_call");
    assert_eq!(item["call_id"], "call_a");
    assert_eq!(item["name"], "shell");
    assert_eq!(item["arguments"], "{\"cmd\":\"ls\"}");
    assert_eq!(item["status"], "completed");
}

#[test]
fn chat_custom_tool_stream_returns_raw_freeform_input() {
    let mapped = chat(json!({"input": "go", "tools": [{"type": "custom", "name": "apply_patch"}]}));
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let events = drain(
        &mut bridge,
        &[
            chat_chunk(
                json!({"tool_calls": [{"index": 0, "id": "call_p", "function": {"name": "apply_patch",
            "arguments": "{\"input\":\"*** Begin Patch"}}]}),
                Value::Null,
            ),
            chat_chunk(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "\\n*** End Patch\"}"}}]}),
                json!("tool_calls"),
            ),
        ],
    );
    let response = terminal(&events);
    let item = item_of_type(&response, "custom_tool_call");
    assert_eq!(item["name"], "apply_patch");
    assert_eq!(item["call_id"], "call_p");
    assert_eq!(item["input"], "*** Begin Patch\n*** End Patch");
    let done = events_of(&events, "response.custom_tool_call_input.done");
    assert_eq!(done[0]["input"], "*** Begin Patch\n*** End Patch");
}

#[test]
fn chat_namespaced_call_restores_original_name_and_namespace() {
    let mapped = chat(
        json!({"input": "go", "tools": [{"type": "namespace", "name": "mcp", "tools": [
        {"type": "function", "name": "list", "description": "l", "parameters": {"type": "object", "properties": {}}}]}]}),
    );
    let upstream = mapped.body["tools"][0]["function"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let events = drain(
        &mut bridge,
        &[chat_chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_n",
        "function": {"name": upstream, "arguments": "{}"}}]}),
            json!("tool_calls"),
        )],
    );
    let item = item_of_type(&terminal(&events), "function_call");
    assert_eq!(item["name"], "list");
    assert_eq!(item["namespace"], "mcp");
}

#[test]
fn chat_unknown_tool_name_fails_the_turn() {
    let mapped = chat(json!({"input": "go"}));
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let result=bridge.push(&chat_chunk(json!({"tool_calls":[{"index":0,"id":"call_x","function":{"name":"hallucinated","arguments":"{}"}}]}),json!("tool_calls"))).and_then(|_|bridge.finish());
    assert!(result.is_err());
}

#[test]
fn chat_reasoning_deltas_become_a_replayable_reasoning_item() {
    let mapped = chat(json!({"input": "hi", "reasoning": {"effort": "medium"}}));
    assert_eq!(mapped.body["reasoning_effort"], "medium");
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let events = drain(
        &mut bridge,
        &[
            chat_chunk(json!({"reasoning_content": "weigh "}), Value::Null),
            chat_chunk(
                json!({"reasoning_content": "options", "content": "done"}),
                json!("stop"),
            ),
        ],
    );
    let kinds = types(&events);
    assert!(
        kinds.contains(&"response.reasoning_summary_text.delta".to_string()),
        "{kinds:?}"
    );
    let response = terminal(&events);
    let reasoning = item_of_type(&response, "reasoning");
    assert_eq!(reasoning["summary"][0]["text"], "weigh options");
    let encrypted = reasoning["encrypted_content"]
        .as_str()
        .expect("chat reasoning is replayable");
    assert!(encrypted.starts_with(REPLAY_PREFIX), "{encrypted}");
    assert_eq!(
        item_of_type(&response, "message")["content"][0]["text"],
        "done"
    );
}

#[test]
fn chat_finish_reason_length_marks_the_response_incomplete() {
    let mapped = chat(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let events = drain(
        &mut bridge,
        &[
            chat_chunk(json!({"content": "partial"}), Value::Null),
            chat_chunk(json!({}), json!("length")),
        ],
    );
    assert!(!types(&events).contains(&"response.completed".to_string()));
    let response = terminal(&events);
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
    assert_eq!(
        item_of_type(&response, "message")["content"][0]["text"],
        "partial"
    );
}

#[test]
fn chat_stream_without_a_finish_reason_never_reports_success() {
    let mapped = chat(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let events = bridge
        .push(&chat_chunk(json!({"content": "half"}), Value::Null))
        .unwrap();
    let error = bridge
        .finish()
        .expect_err("EOF without a stop marker is not a completion");
    assert!(!error.to_string().is_empty());
    assert!(!types(&events)
        .iter()
        .any(|kind| kind.starts_with("response.completed")));
    assert!(!bridge.is_finished());
    // finish 可重复调用，结论一致。
    assert!(bridge.finish().is_err());
}

#[test]
fn chat_empty_upstream_turn_is_not_reported_as_completed() {
    let mapped = chat(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    bridge.push(&chat_chunk(json!({}), json!("stop"))).unwrap();
    assert!(
        bridge.finish().is_err(),
        "没有任何输出的 completed 会让 Codex 静默收尾"
    );
}

#[test]
fn chat_upstream_error_chunk_fails_the_stream() {
    let mapped = chat(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let error = bridge
        .push(&json!({"error": {"message": "upstream exploded", "type": "server_error"}}))
        .expect_err("error chunks are not deltas");
    assert!(error.to_string().contains("upstream exploded"), "{error}");
}

#[test]
fn chat_data_after_the_terminal_response_is_rejected() {
    let mapped = chat(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let mut events = bridge
        .push(&chat_chunk(json!({"content": "done"}), json!("stop")))
        .unwrap();
    events.extend(bridge.finish().unwrap());
    assert!(types(&events).contains(&"response.completed".to_string()));
    assert!(bridge
        .push(&chat_chunk(json!({"content": "late"}), Value::Null))
        .is_err());
    assert!(bridge.finish().unwrap().is_empty(), "finish 幂等");
}

#[test]
fn bridge_exposes_the_final_response_for_non_streaming_clients() {
    let mapped = chat(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Chat, "vendor-model");
    let events = drain(
        &mut bridge,
        &[chat_chunk(json!({"content": "done"}), json!("stop"))],
    );
    let response = terminal(&events);
    assert_eq!(
        bridge.output(),
        response["output"].as_array().unwrap().clone()
    );
    assert_eq!(bridge.usage(), response["usage"].clone());
    assert_eq!(bridge.response_id(), response["id"].as_str().unwrap());
}

// ===================== Anthropic 上游分片 → Responses 事件 =====================

#[test]
fn anthropic_thinking_and_tool_use_stream_completes_with_usage() {
    let mapped = anthropic(
        json!({"input": "go", "tools": [{"type": "function", "name": "shell",
        "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}}]}),
    );
    let mut bridge = stream(mapped, UpstreamFormat::Anthropic, "claude-vendor");
    let events = drain(
        &mut bridge,
        &[
            json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-vendor",
            "usage": {"input_tokens": 11, "cache_read_input_tokens": 3}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "plan first"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-1"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"ls\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
            json!({"type": "message_stop"}),
        ],
    );
    let response = terminal(&events);
    assert_eq!(response["status"], "completed");
    let reasoning = item_of_type(&response, "reasoning");
    assert_eq!(reasoning["summary"][0]["text"], "plan first");
    assert!(reasoning["encrypted_content"]
        .as_str()
        .unwrap()
        .starts_with(REPLAY_PREFIX));
    let call = item_of_type(&response, "function_call");
    assert_eq!(call["call_id"], "toolu_1");
    assert_eq!(call["name"], "shell");
    assert_eq!(call["arguments"], "{\"cmd\":\"ls\"}");
    assert_eq!(response["usage"]["input_tokens"], 14);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        3
    );
    assert_eq!(response["usage"]["output_tokens"], 7);
    assert_eq!(response["usage"]["total_tokens"], 21);
}

#[test]
fn anthropic_text_stream_completes_a_message_item() {
    let mapped = anthropic(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Anthropic, "claude-vendor");
    let events = drain(
        &mut bridge,
        &[
            json!({"type": "message_start", "message": {"id": "msg_2", "usage": {"input_tokens": 2}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": " there"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
            json!({"type": "message_stop"}),
        ],
    );
    let response = terminal(&events);
    assert_eq!(
        item_of_type(&response, "message")["content"][0]["text"],
        "Hi there"
    );
    assert_eq!(response["usage"]["output_tokens"], 3);
}

#[test]
fn anthropic_truncation_after_message_delta_is_an_error() {
    // message_delta 已经给出 stop_reason，只是缺 message_stop：终态合法，但截断不能算成功。
    let mapped = anthropic(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Anthropic, "claude-vendor");
    for chunk in [
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "cut"}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 1}}),
    ] {
        bridge.push(&chunk).unwrap();
    }
    assert!(
        bridge.finish().is_err(),
        "message_stop is required to distinguish truncation from a complete stream"
    );
}

#[test]
fn anthropic_stream_without_stop_reason_is_not_completed() {
    let mapped = anthropic(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Anthropic, "claude-vendor");
    bridge
        .push(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}))
        .unwrap();
    let events = bridge
        .push(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "half"}}))
        .unwrap();
    assert!(!types(&events)
        .iter()
        .any(|kind| kind.starts_with("response.completed")));
    assert!(bridge.finish().is_err());
}

#[test]
fn anthropic_unsigned_thinking_is_not_faked_into_replayable_state() {
    let mapped = anthropic(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Anthropic, "claude-vendor");
    let events = drain(
        &mut bridge,
        &[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "no signature"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "answer"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ],
    );
    let response = terminal(&events);
    assert_eq!(response["status"], "completed");
    let reasoning = item_of_type(&response, "reasoning");
    assert_eq!(reasoning["summary"][0]["text"], "no signature");
    assert!(
        reasoning.get("encrypted_content").is_none(),
        "没有签名就不能声称可以回放"
    );
}

#[test]
fn anthropic_unknown_content_block_type_fails_instead_of_dropping_it() {
    let mapped = anthropic(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Anthropic, "claude-vendor");
    let error = bridge
        .push(&json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "server_tool_use", "id": "srv_1", "name": "web_search"}}))
        .expect_err("无法表达的块不能被丢掉");
    assert!(error.to_string().contains("server_tool_use"), "{error}");
}

#[test]
fn anthropic_error_event_fails_the_stream() {
    let mapped = anthropic(json!({"input": "hi"}));
    let mut bridge = stream(mapped, UpstreamFormat::Anthropic, "claude-vendor");
    let error = bridge
        .push(&json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}))
        .expect_err("error events are not content");
    assert!(error.to_string().contains("busy"), "{error}");
}

// ===================== 推理状态归属与回放 =====================

#[test]
fn signed_anthropic_thinking_round_trips_into_the_next_request() {
    let tools = json!([{"type": "function", "name": "shell", "parameters": {"type": "object", "properties": {}}}]);
    let first =
        anthropic(json!({"input": "go", "tools": tools.clone(), "reasoning": {"effort": "high"}}));
    let mut bridge = stream(first, UpstreamFormat::Anthropic, "claude-vendor");
    let events = drain(
        &mut bridge,
        &[
            json!({"type": "message_start", "message": {"id": "msg_9", "usage": {"input_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "check the tree"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-9"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_9", "name": "shell", "input": {"cmd": "ls"}}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 4}}),
            json!({"type": "message_stop"}),
        ],
    );
    let response = terminal(&events);
    let reasoning = item_of_type(&response, "reasoning");
    let call = item_of_type(&response, "function_call");

    let next = anthropic(json!({
        "input": [
            {"role": "user", "content": "go"},
            reasoning,
            call,
            {"type": "function_call_output", "call_id": "toolu_9", "output": "src"}
        ],
        "tools": tools,
        "reasoning": {"effort": "high"}
    }));
    let messages = next.body["messages"].as_array().unwrap();
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"][0],
        json!({"type": "thinking", "thinking": "check the tree", "signature": "sig-9"})
    );
    assert_eq!(messages[1]["content"][1]["type"], "tool_use");
    assert_eq!(messages[1]["content"][1]["id"], "toolu_9");
    assert_eq!(
        next.body["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048}),
        "签名 thinking 可以续轮，预算必须低于 max_tokens 4096"
    );
}

#[test]
fn anthropic_reasoning_state_from_another_model_is_not_replayed() {
    // 契约变化（审计 13-codex/P1-1）：回放归属以前是"整条路由指纹"，改一次模型配置
    // （换 API Key、改参数、改上限）就让进行中会话的旧密文被判成别的模型，整轮 400 而
    // 400 不可重试。现在归属只认模型身份 + 协议，不是本模型签发的状态一律降级成
    // "这轮不回放"：请求照常发出，只是没有那段推理，绝不会把别人的密文发上去。
    let tools = json!([{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}}]);
    let foreign = translate_request(
        &json!({"input": "go", "tools": tools.clone()}),
        UpstreamFormat::Anthropic,
        "other-model",
        4096,
    )
    .unwrap();
    let mut bridge = stream(foreign, UpstreamFormat::Anthropic, "other-model");
    let events = drain(
        &mut bridge,
        &[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "x"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "ok"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ],
    );
    let reasoning = item_of_type(&terminal(&events), "reasoning");
    let issued = reasoning["encrypted_content"]
        .as_str()
        .expect("这轮的状态带着密文")
        .to_owned();

    let next = translate_request(
        &json!({"input": [{"role": "user", "content": "again"}, reasoning],
            "tools": tools, "reasoning": {"effort": "high"}}),
        UpstreamFormat::Anthropic,
        "claude-vendor",
        4096,
    )
    .expect("别的模型签发的状态必须降级成不回放，而不是让这一轮 400");
    let body = next.body.to_string();
    assert!(
        !body.contains(&issued),
        "别人的密文不能出现在这一轮发出的请求里: {body}"
    );
    assert!(
        !body.contains("\"type\":\"thinking\""),
        "没有本模型签名的状态不能冒充 thinking 块: {body}"
    );
}

#[test]
fn foreign_encrypted_reasoning_is_dropped_instead_of_failing_the_turn() {
    // 契约变化（审计 13-codex/P1-1）：外来密文（官方模型或别的工具写的
    // `encrypted_content`）以前明确报错、整轮 400；现在降级成"这轮不回放"，请求照常
    // 发出，密文本身既不解析也不转发。
    const BLOB: &str = "gAAAAAB-openai-blob";

    // Anthropic：没有签名就不能冒充 thinking 块，这段状态直接丢掉。
    let anthropic_body = translate_request(
        &json!({"input": [
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "encrypted_content": BLOB,
             "summary": [{"type": "summary_text", "text": "visible"}]}
        ]}),
        UpstreamFormat::Anthropic,
        "vendor-model",
        4096,
    )
    .expect("外来密文必须降级而不是让整轮 400")
    .body;
    let anthropic_body = anthropic_body.to_string();
    assert!(!anthropic_body.contains(BLOB), "{anthropic_body}");
    assert!(
        !anthropic_body.contains("\"type\":\"thinking\""),
        "外来状态不能变成 thinking 块: {anthropic_body}"
    );

    // Chat：退回可见摘要，密文本身不转发。
    let chat_body = translate_request(
        &json!({"input": [
            {"type": "reasoning", "encrypted_content": BLOB,
             "summary": [{"type": "summary_text", "text": "visible"}]},
            {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"},
            {"type":"function_call_output","call_id":"c1","output":"ok"}
        ]}),
        UpstreamFormat::Chat,
        "vendor-model",
        4096,
    )
    .expect("外来密文必须降级而不是让整轮 400")
    .body;
    assert_eq!(chat_body["messages"][0]["reasoning_content"], "visible");
    assert!(!chat_body.to_string().contains(BLOB), "{chat_body}");

    // 空字符串等同于没有密文：退回可见摘要，不算错误。
    let mapped = chat(json!({"input": [
        {"type": "reasoning", "encrypted_content": "", "summary": [{"type": "summary_text", "text": "visible"}]},
        {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"},
        {"type":"function_call_output","call_id":"c1","output":"ok"}
    ]}));
    assert_eq!(mapped.body["messages"][0]["reasoning_content"], "visible");
}

/// 降级只针对"不是本模型/本协议签发的密文"。我们自己的数据被改坏（base64 或 JSON
/// 坏掉、缺 state）仍然是错误：那是真的丢了状态，静默吞掉只会让下一轮悄悄少一段推理。
#[test]
fn structurally_broken_reasoning_state_is_still_rejected() {
    for format in [UpstreamFormat::Chat, UpstreamFormat::Anthropic] {
        for broken in [
            format!("{REPLAY_PREFIX}not base64!!"),
            // `ew==` 是 `{` 的 base64：解出来不是合法 JSON。
            format!("{REPLAY_PREFIX}ew=="),
        ] {
            let error = translate_request(
                &json!({"input": [
                    {"role": "user", "content": "hi"},
                    {"type": "reasoning", "encrypted_content": broken}
                ]}),
                format,
                "vendor-model",
                4096,
            )
            .expect_err("坏掉的密文不能被当成'没有状态'")
            .to_string();
            assert!(
                error.contains("reasoning state"),
                "{format:?} 的报错必须指认是推理状态: {error}"
            );
        }
    }
}

#[test]
fn chat_reasoning_round_trips_into_reasoning_content() {
    let tools = json!([{"type": "function", "name": "shell", "parameters": {"type": "object", "properties": {}}}]);
    let first = chat(json!({"input": "go", "tools": tools.clone()}));
    let mut bridge = stream(first, UpstreamFormat::Chat, "vendor-model");
    let events = drain(
        &mut bridge,
        &[chat_chunk(
            json!({"reasoning_content": "check", "tool_calls": [{"index": 0, "id": "call_1",
            "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}}]}),
            json!("tool_calls"),
        )],
    );
    let response = terminal(&events);
    let reasoning = item_of_type(&response, "reasoning");
    let call = item_of_type(&response, "function_call");

    let next = chat(json!({
        "input": [
            {"role": "user", "content": "go"},
            reasoning,
            call,
            {"type": "function_call_output", "call_id": "call_1", "output": "src"}
        ],
        "tools": tools
    }));
    let assistant = &next.body["messages"][1];
    assert_eq!(assistant["reasoning_content"], "check");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "shell");
}
