use cursor_server::codex::bridge::{translate_request, BridgeStream, UpstreamFormat};
use serde_json::json;
fn mapped() -> cursor_server::codex::bridge::BridgeRequest {
    translate_request(&json!({"input":"go","tools":[{"type":"function","name":"shell","parameters":{"type":"object"}}]}),UpstreamFormat::Chat,"vendor",4096).unwrap()
}
#[test]
fn missing_call_id_and_undeclared_tools_cannot_become_executable_output() {
    for (id, name, args) in [
        ("", "shell", "{}"),
        ("c", "unknown", "{}"),
        ("c", "shell", "not JSON"),
    ] {
        let m = mapped();
        let mut stream = BridgeStream::new(UpstreamFormat::Chat, "vendor".into(), m.tools);
        let result=stream.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"function":{"name":name,"arguments":args}}]},"finish_reason":"tool_calls"}]})).and_then(|_|stream.finish());
        assert!(result.is_err(), "accepted {id}/{name}/{args}");
    }
}
#[test]
fn tool_history_is_never_silently_dropped_or_fabricated() {
    for item in [
        json!({"type":"function_call","name":"shell","call_id":"c","arguments":"{}"}),
        json!({"type":"function_call_output","call_id":"c","output":"result"}),
    ] {
        for format in [UpstreamFormat::Chat, UpstreamFormat::Anthropic] {
            assert!(translate_request(
                &json!({"input":[{"role":"user","content":"go"},item]}),
                format,
                "vendor",
                4096
            )
            .is_err());
        }
    }
    let m=translate_request(&json!({"input":[{"role":"user","content":"go"},{"type":"function_call","name":"shell","call_id":"c","arguments":"{}"},{"type":"function_call_output","call_id":"c","output":"ok"}]}),UpstreamFormat::Chat,"vendor",4096).unwrap();
    assert!(m.body["messages"][1].get("reasoning_content").is_none());
}
#[test]
fn duplicate_tools_are_rejected_and_illegal_names_are_sanitized() {
    assert!(translate_request(&json!({"input":"go","tools":[{"type":"custom","name":"same"},{"type":"custom","name":"same"}]}),UpstreamFormat::Chat,"vendor",4096).is_err());
    let m=translate_request(&json!({"input":"go","tools":[{"type":"function","name":"a.b/中","parameters":{"type":"object"}}]}),UpstreamFormat::Chat,"vendor",4096).unwrap();
    let name = m.body["tools"][0]["function"]["name"].as_str().unwrap();
    assert!(
        name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    );
}
#[test]
fn terminal_events_have_contiguous_sequence_numbers_and_all_argument_fragments() {
    let m = mapped();
    let mut stream = BridgeStream::new(UpstreamFormat::Chat, "vendor".into(), m.tools);
    let mut events=stream.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"shell","arguments":"{\"cmd\":"}}]},"finish_reason":null}]})).unwrap();
    events.extend(stream.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":"tool_calls"}]})).unwrap());
    events.extend(stream.finish().unwrap());
    let args = events
        .iter()
        .filter(|e| e["type"] == "response.function_call_arguments.delta")
        .filter_map(|e| e["delta"].as_str())
        .collect::<String>();
    assert_eq!(args, "{\"cmd\":\"ls\"}");
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], json!(index));
    }
}
#[test]
fn unknown_finish_reason_and_missing_anthropic_message_stop_are_errors() {
    let m = mapped();
    let mut stream = BridgeStream::new(UpstreamFormat::Chat, "vendor".into(), m.tools);
    assert!(stream
        .push(&json!({"choices":[{"delta":{"content":"hi"},"finish_reason":"vendor_failed"}]}))
        .and_then(|_| stream.finish())
        .is_err());
    let m = translate_request(
        &json!({"input":"go"}),
        UpstreamFormat::Anthropic,
        "vendor",
        4096,
    )
    .unwrap();
    let mut stream = BridgeStream::new(UpstreamFormat::Anthropic, "vendor".into(), m.tools);
    stream.push(&json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"hi"}})).unwrap();
    stream
        .push(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}))
        .unwrap();
    assert!(stream.finish().is_err());
}
#[test]
fn images_in_tool_results_survive_both_protocols() {
    let source = json!({"input":[{"role":"user","content":"look"},{"type":"function_call","name":"view_image","call_id":"c","arguments":"{}"},
        {"type":"function_call_output","call_id":"c","output":[{"type":"input_text","text":"image"},{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]}]});
    let chat = translate_request(&source, UpstreamFormat::Chat, "vendor", 4096).unwrap();
    assert_eq!(
        chat.body["messages"][2]["content"][1]["image_url"]["url"],
        "data:image/png;base64,AAAA"
    );
    let anthropic = translate_request(&source, UpstreamFormat::Anthropic, "vendor", 4096).unwrap();
    assert_eq!(
        anthropic.body["messages"][2]["content"][0]["content"][1]["source"]["data"],
        "AAAA"
    );
}

#[test]
fn sanitized_name_collisions_and_forced_choice_keep_distinct_tool_identities() {
    let source = json!({"input":"go","tools":[
        {"type":"function","name":"a.b","parameters":{"type":"object"}},
        {"type":"function","name":"a/b","parameters":{"type":"object"}},
        {"type":"function","name":"a?b","parameters":{"type":"object"}}
    ]});
    let m = translate_request(&source, UpstreamFormat::Chat, "vendor", 4096).unwrap();
    let names: Vec<_> = m.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );
    let mut forced = source;
    forced["tool_choice"] = json!({"type":"function","name":"a_b"});
    assert!(translate_request(&forced, UpstreamFormat::Chat, "vendor", 4096).is_err());
}

#[test]
fn anthropic_cached_input_is_counted_once_in_responses_usage() {
    let m = translate_request(
        &json!({"input":"go"}),
        UpstreamFormat::Anthropic,
        "vendor",
        4096,
    )
    .unwrap();
    let mut stream = BridgeStream::new(UpstreamFormat::Anthropic, "vendor".into(), m.tools);
    stream.push(&json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}}})).unwrap();
    stream.push(&json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}})).unwrap();
    stream.push(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}})).unwrap();
    let events = stream.push(&json!({"type":"message_stop"})).unwrap();
    let usage = &events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap()["response"]["usage"];
    assert_eq!(usage["input_tokens"], 60);
    assert_eq!(usage["total_tokens"], 65);
}

#[test]
fn redacted_thinking_and_initial_signature_round_trip_without_exposure() {
    for block in [
        json!({"type":"thinking","thinking":"consider","signature":"signed"}),
        json!({"type":"redacted_thinking","data":"opaque"}),
    ] {
        let m = translate_request(
            &json!({"input":"go"}),
            UpstreamFormat::Anthropic,
            "vendor",
            4096,
        )
        .unwrap();
        let mut stream = BridgeStream::new(UpstreamFormat::Anthropic, "vendor".into(), m.tools);
        let mut events = stream
            .push(&json!({"type":"content_block_start","index":0,"content_block":block}))
            .unwrap();
        events.extend(
            stream
                .push(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}))
                .unwrap(),
        );
        events.extend(stream.push(&json!({"type":"message_stop"})).unwrap());
        events.extend(stream.finish().unwrap());
        let item = events
            .iter()
            .find(|e| e["type"] == "response.completed")
            .unwrap()["response"]["output"][0]
            .clone();
        assert!(item["encrypted_content"].as_str().is_some());
        if block["type"] == "redacted_thinking" {
            assert!(!events.iter().any(|e| e["delta"] == "opaque"));
        }
        let replay=translate_request(&json!({"input":[{"role":"user","content":"go"},item,{"role":"assistant","content":"done"},{"role":"user","content":"continue"}]}),UpstreamFormat::Anthropic,"vendor",4096).unwrap();
        let blocks = replay.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "assistant")
            .flat_map(|m| m["content"].as_array().unwrap());
        assert!(blocks.into_iter().any(|value| value == &block));
    }
}
