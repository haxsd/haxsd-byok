//! Verifies explicit and automatic context compaction behavior.
#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::{collections::HashMap, sync::Arc, time::Duration};

use cursor_server::{
    cursor::prompting::{PromptAssets, PromptCompiler},
    cursor::{
        protocol::{connect, proto::agent::v1 as pb},
        TransportCommand, TransportRegistry,
    },
    model::{
        ContentPart, ConversationId, MessageContent, ModelConfigInput, ModelType, Origin,
        ProjectedContent, ProjectedMessage, Role, Usage, OPENAI_CHAT_ENDPOINT,
    },
    provider::{FinishReason, ModelEvent},
};
use prost::Message;

#[tokio::test]
async fn summarize_replaces_model_history_and_preserves_cursor_history() {
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&ModelConfigInput {
            sort_order: 0,
            display_name: "Test Model".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "test-key".into(),
            tooltip_data: "Test Model".into(),
            model_id: "test-model".into(),
            reasoning_effort: None,
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        })
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response("old answer", 4_000, 12));
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "summary-call".into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta("Durable ".into()),
        ModelEvent::TextDelta("summary".into()),
        ModelEvent::TextEnd,
        ModelEvent::Usage(Usage {
            input_tokens: Some(4_012),
            context_input_tokens: Some(4_012),
            output_tokens: Some(9),
            total_tokens: Some(4_021),
            ..Default::default()
        }),
        ModelEvent::Done(FinishReason::Stop),
    ]);
    provider.push(text_response("new answer", 900, 5));
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store.clone(),
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    );

    let first = run(
        &registry,
        "first",
        user_request(
            "conversation",
            "user-1",
            "remember alpha",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let first_state = first.checkpoints.last().unwrap().clone();
    let old_turns = first_state.turns.clone();
    let old_roots = first_state.root_prompt_messages_json.clone();
    assert!(old_roots.len() >= 3);

    let compacted = run(
        &registry,
        "compact",
        summary_request("conversation", &model.model_hash, first_state),
    )
    .await;
    assert_eq!(compacted.summary_started, 1);
    assert_eq!(compacted.summary, "Durable summary");
    assert_eq!(compacted.summary_completed, 1);
    assert_eq!(compacted.turn_ended, 1);
    assert_eq!(compacted.token_delta, 0);
    assert_eq!(compacted.checkpoints.len(), 3);
    assert!(compacted
        .checkpoints
        .windows(2)
        .all(|pair| pair[0] == pair[1]));

    let compacted_state = compacted.checkpoints.last().unwrap();
    assert_eq!(compacted_state.root_prompt_messages_json.len(), 2);
    assert!(compacted_state.turns.starts_with(&old_turns));
    assert_eq!(compacted_state.turns.len(), old_turns.len() + 1);
    assert_eq!(compacted_state.self_summary_count, 1);
    let summary_id = compacted_state.summary.as_ref().unwrap();
    let summary = pb::ConversationSummary::decode(compacted.blobs[summary_id].as_slice()).unwrap();
    assert!(
        summary.summary.starts_with("Durable summary"),
        "the model summary is preserved; deterministic anchors may be appended: {}",
        summary.summary
    );
    let archive_id = compacted_state.summary_archive.as_ref().unwrap();
    let archive =
        pb::ConversationSummaryArchive::decode(compacted.blobs[archive_id].as_slice()).unwrap();
    assert!(archive.summary.starts_with("Durable summary"));
    assert_eq!(archive.window_tail, 0);
    assert_eq!(archive.summarized_messages, old_roots[1..]);
    assert_eq!(
        archive.summary_message,
        *compacted_state.root_prompt_messages_json.last().unwrap()
    );

    let stored = store
        .load_current_messages(&ConversationId::new("conversation"))
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].origin, Origin::Runtime);
    assert_eq!(stored[0].role, Role::User);
    assert!(matches!(
        &stored[0].content,
        MessageContent::Parts { parts }
            if matches!(parts.as_slice(), [ContentPart::Text { text }]
                if text.starts_with("<conversation_summary>\nDurable summary"))
    ));

    let after = run(
        &registry,
        "after",
        user_request(
            "conversation",
            "user-2",
            "what remains?",
            &model.model_hash,
            Some(compacted_state.clone()),
        ),
    )
    .await;
    assert!(after
        .checkpoints
        .last()
        .unwrap()
        .root_prompt_messages_json
        .starts_with(&compacted_state.root_prompt_messages_json));
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].prompt.tools.is_empty());
    assert!(requests[1]
        .prompt
        .instructions
        .contains("compacting conversation history"));
    assert_eq!(requests[1].history.len(), 3);
    assert_eq!(
        requests[1].history[2].message_id, "compaction:instruction",
        "an assistant-terminated history gets the summarize instruction as its user tail"
    );
    assert_eq!(requests[2].history.len(), 2);
    let ProjectedContent::Parts(summary_parts) = &requests[2].history[0].content else {
        panic!("first post-compaction message must be the summary")
    };
    assert!(
        matches!(summary_parts.as_slice(), [ContentPart::Text { text }]
        if text.contains("Durable summary"))
    );
    let ProjectedContent::Parts(new_user_parts) = &requests[2].history[1].content else {
        panic!("second post-compaction message must be the new runtime user")
    };
    assert!(
        matches!(new_user_parts.as_slice(), [ContentPart::Text { text }]
        if text.contains("what remains?") && !text.contains("remember alpha"))
    );
}

#[tokio::test]
async fn automatic_compaction_preflights_provider_input_and_records_rebuilt_tokens() {
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&ModelConfigInput {
            sort_order: 0,
            display_name: "Auto Compact Model".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "test-key".into(),
            tooltip_data: "Auto Compact Model".into(),
            model_id: "auto-compact-model".into(),
            reasoning_effort: None,
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: Some(100_000),
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        })
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response(&"x".repeat(400_000), 150_000, 1_000));
    provider.push(text_response("automatic durable summary", 120_000, 20));
    provider.push(text_response("continued after compaction", 20_000, 20));
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    );

    let first = run(
        &registry,
        "auto-first",
        user_request(
            "auto-conversation",
            "auto-user-1",
            "start",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let first_state = first.checkpoints.last().unwrap().clone();
    assert!(first_state.token_details.as_ref().unwrap().used_tokens > 100_000);

    let second = run(
        &registry,
        "auto-second",
        user_request(
            "auto-conversation",
            "auto-user-2",
            "continue",
            &model.model_hash,
            Some(first_state),
        ),
    )
    .await;
    assert_eq!(second.summary_started, 1);
    assert_eq!(second.summary_completed, 1);
    assert_eq!(
        &second.interaction_events[..4],
        &[
            "token_delta:0",
            "summary_started",
            "summary_completed",
            "token_delta:0",
        ],
        "automatic compaction must publish estimated usage before summarizing and zero usage after"
    );
    let compacted_tokens = second
        .checkpoints
        .iter()
        .filter_map(|state| state.token_details.as_ref())
        .map(|details| details.used_tokens)
        .find(|tokens| *tokens > 0 && *tokens < 100_000)
        .expect("compacted checkpoint must record rebuilt context tokens");
    assert!(compacted_tokens < 90_000);

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].prompt.tools.is_empty());
    assert!(requests[1].prompt.tools.is_empty());
    assert!(!requests[2].prompt.tools.is_empty());
    assert!(requests[1]
        .history
        .iter()
        .any(|message| match &message.content {
            ProjectedContent::Assistant { text, .. } => text.len() == 400_000,
            _ => false,
        }));
    assert!(requests[2]
        .history
        .iter()
        .all(|message| match &message.content {
            ProjectedContent::Assistant { text, .. } => text.len() != 400_000,
            _ => true,
        }));
}

#[tokio::test]
async fn incremental_preflight_uses_conversation_anchor_across_model_switch() {
    let (_directory, store) = fixtures::temp_store().await;
    let model_a = store
        .create_model(&ModelConfigInput {
            sort_order: 0,
            display_name: "Anchor Model A".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "test-key".into(),
            tooltip_data: "Anchor Model A".into(),
            model_id: "anchor-model-a".into(),
            reasoning_effort: None,
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        })
        .await
        .unwrap();
    let model_b = store
        .create_model(&ModelConfigInput {
            sort_order: 1,
            display_name: "Anchor Model B".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "test-key".into(),
            tooltip_data: "Anchor Model B".into(),
            model_id: "anchor-model-b".into(),
            reasoning_effort: None,
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: Some(200_000),
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        })
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response("old answer", 103_904, 12));
    provider.push(text_response("new answer", 104_000, 12));
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    );

    let first = run(
        &registry,
        "anchor-first",
        user_request(
            "anchor-conversation",
            "anchor-user-1",
            &"x".repeat(400_000),
            &model_a.model_hash,
            None,
        ),
    )
    .await;
    let second = run(
        &registry,
        "anchor-second",
        user_request(
            "anchor-conversation",
            "anchor-user-2",
            "short follow-up",
            &model_b.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;

    assert_eq!(second.summary_started, 0);
    assert_eq!(second.summary_completed, 0);
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn irreducibly_oversized_current_input_fails_before_provider_dispatch() {
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&ModelConfigInput {
            sort_order: 0,
            display_name: "Overflow Model".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "test-key".into(),
            tooltip_data: "Overflow Model".into(),
            model_id: "overflow-model".into(),
            reasoning_effort: None,
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: Some(100_000),
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        })
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    );

    let output = run(
        &registry,
        "overflow-request",
        user_request(
            "overflow-conversation",
            "overflow-user",
            &"x".repeat(400_000),
            &model.model_hash,
            None,
        ),
    )
    .await;

    assert!(provider.requests().is_empty());
    assert_eq!(output.summary_started, 0);
    assert_eq!(output.summary_completed, 0);
}

#[tokio::test]
async fn automatic_compaction_uses_the_configured_summary_model() {
    // The summarize call may run on an independent (cheaper) model. It must
    // resolve through the router by hash and stay inside the configured output
    // cap without disturbing the conversation model.
    let (_directory, store) = fixtures::temp_store().await;
    let main = store
        .create_model(&windowed_model("main-model", Some(100_000)))
        .await
        .unwrap();
    let summary = store
        .create_model(&windowed_model("summary-model", Some(128_000)))
        .await
        .unwrap();
    store
        .set_compaction_settings(cursor_server::store::CompactionSettings {
            auto: true,
            summary_model_hash: summary.model_hash.clone(),
            summary_output_tokens: Some(1_024),
            ..Default::default()
        })
        .await
        .unwrap();

    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response(&"x".repeat(400_000), 150_000, 1_000));
    provider.push(text_response("short durable summary", 100_000, 20));
    provider.push(text_response("continued after compact", 20_000, 20));
    let registry = registry(store, &provider);

    let first = run(
        &registry,
        "summary-model-first",
        user_request(
            "summary-model-conversation",
            "summary-model-user-1",
            "start",
            &main.model_hash,
            None,
        ),
    )
    .await;
    let second = run(
        &registry,
        "summary-model-second",
        user_request(
            "summary-model-conversation",
            "summary-model-user-2",
            "continue",
            &main.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert_eq!(second.summary_started, 1);

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].model.model_id, summary.model_hash,
        "the summarize call must target the configured summary model"
    );
    assert_eq!(
        requests[1].model.max_output_tokens,
        Some(1_024),
        "the summarize call must honor the configured output cap"
    );
    assert_eq!(
        requests[2].model.model_id, main.model_hash,
        "the conversation keeps running on the main model"
    );
}

fn windowed_model(model_id: &str, context_window_tokens: Option<u64>) -> ModelConfigInput {
    ModelConfigInput {
        sort_order: 0,
        display_name: model_id.into(),
        group_name: None,
        model_type: ModelType::OpenAi,
        base_url: "https://example.com/v1/chat/completions".into(),
        use_full_url: true,
        api_key: "test-key".into(),
        tooltip_data: model_id.into(),
        model_id: model_id.into(),
        reasoning_effort: None,
        openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
        openai_extra_params_enabled: false,
        openai_extra_params: serde_json::json!({}),
        custom_headers_enabled: false,
        custom_headers: serde_json::json!({}),
        anthropic_extra_params_enabled: false,
        anthropic_extra_params: serde_json::json!({}),
        context_window_tokens,
        max_completion_tokens: None,
        anthropic_max_tokens: None,
        anthropic_thinking_effort: None,
        thinking_budget_tokens: None,
    }
}

#[tokio::test]
async fn provider_overflow_refusal_compacts_and_retries_once() {
    // The estimate cleared the compaction check, but the provider counted
    // more and refused. The refusal is the trigger the estimate missed.
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&windowed_model("refusal-model", Some(1_000_000)))
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response("first answer", 4_000, 12));
    provider.push_error(cursor_server::Error::Provider(
        "Anthropic 400 Bad Request: {\"type\":\"error\",\"error\":{\"type\":\
         \"invalid_request_error\",\"message\":\"prompt is too long: 1002148 tokens > \
         1000000 maximum\"}}"
            .into(),
    ));
    provider.push(text_response("durable summary", 3_000, 20));
    provider.push(text_response("answer after compaction", 500, 20));
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    );

    let first = run(
        &registry,
        "refusal-first",
        user_request(
            "refusal-conversation",
            "refusal-user-1",
            "start",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let second = run(
        &registry,
        "refusal-second",
        user_request(
            "refusal-conversation",
            "refusal-user-2",
            "continue",
            &model.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;

    assert_eq!(second.summary_started, 1);
    assert_eq!(second.summary_completed, 1);
    assert_eq!(second.turn_ended, 1);
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        4,
        "refused call, summary call, retried call"
    );
    assert!(requests[2].prompt.tools.is_empty());
    assert_eq!(
        requests[2].history.last().unwrap().role,
        Role::User,
        "the summarizer history must end with a user message"
    );
    assert!(!requests[3].prompt.tools.is_empty());
    assert!(requests[3]
        .history
        .iter()
        .any(|message| match &message.content {
            ProjectedContent::Parts(parts) =>
                matches!(parts.as_slice(), [ContentPart::Text { text }]
            if text.contains("durable summary")),
            _ => false,
        }));
}

#[tokio::test]
async fn a_length_stop_that_filled_the_window_compacts_and_retries() {
    // `FinishReason::Length` 有两个来源：输出预算打满（`max_tokens`）与窗口在生成
    // 中途打满（`model_context_window_exceeded`）。provider 层把两者归一成同一个
    // `Length`，过去 run 层一律当终态失败——于是窗口打满的那一轮硬失败、不压缩，
    // 下一次请求带着同一份超限 prompt 再来一遍。这里钉住：用量说明 prompt 已经吃满
    // 预算时，要回到溢出压缩阶梯。
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&windowed_model("length-overflow-model", Some(100_000)))
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response("first answer", 4_000, 12));
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "call-truncated".into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta("half an answer".into()),
        ModelEvent::TextEnd,
        ModelEvent::Usage(Usage {
            input_tokens: Some(99_500),
            context_input_tokens: Some(99_500),
            output_tokens: Some(30_000),
            total_tokens: Some(129_500),
            ..Default::default()
        }),
        ModelEvent::Done(FinishReason::Length),
    ]);
    provider.push(text_response("durable summary", 3_000, 20));
    provider.push(text_response("answer after compaction", 500, 20));
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    );

    let first = run(
        &registry,
        "length-first",
        user_request(
            "length-conversation",
            "length-user-1",
            "start",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let second = run(
        &registry,
        "length-second",
        user_request(
            "length-conversation",
            "length-user-2",
            "continue",
            &model.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;

    assert_eq!(second.summary_started, 1, "撞窗口要回到压缩阶梯");
    assert_eq!(second.summary_completed, 1);
    assert_eq!(second.turn_ended, 1, "压缩后的重试必须正常收尾");
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        4,
        "第一轮的调用 + 被截断的调用 + 摘要调用 + 压缩后的重试"
    );
    assert!(requests[2].prompt.tools.is_empty(), "摘要调用不带工具");
    assert!(
        requests[3].history.iter().any(
            |message| matches!(&message.content, ProjectedContent::Parts(parts)
                if matches!(parts.as_slice(), [ContentPart::Text { text }]
                    if text.contains("durable summary")))
        ),
        "重试要带着摘要"
    );
}

#[tokio::test]
async fn assistant_terminated_history_is_sent_with_a_user_tail() {
    // Cursor can resume a conversation whose committed history already ends
    // with the assistant. Anthropic refuses that as a prefill, so the run
    // appends a provider-visible continuation without persisting it.
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&windowed_model("tail-model", None))
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response("first answer", 400, 12));
    provider.push(text_response("resumed answer", 450, 12));
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store.clone(),
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    );

    let first = run(
        &registry,
        "tail-first",
        user_request(
            "tail-conversation",
            "tail-user-1",
            "start",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let resumed = run(
        &registry,
        "tail-resume",
        request(
            "tail-conversation",
            &model.model_hash,
            first.checkpoints.last().cloned(),
            pb::conversation_action::Action::ResumeAction(pb::ResumeAction::default()),
        ),
    )
    .await;
    assert_eq!(resumed.turn_ended, 1);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let tail = requests[1].history.last().unwrap();
    assert_eq!(tail.role, Role::User);
    assert_eq!(tail.message_id, "runtime:continue");
    assert_eq!(
        requests[1].history[..requests[1].history.len() - 1]
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>()
            .len(),
        requests[0].history.len() + 1,
        "committed history plus the first answer, then the transient tail"
    );
    let stored = store
        .load_current_messages(&ConversationId::new("tail-conversation"))
        .await
        .unwrap();
    assert!(
        stored
            .iter()
            .all(|message| message.message_id != "runtime:continue"),
        "the continuation tail is provider-visible only and never persisted"
    );
}

#[tokio::test]
async fn automatic_compaction_replays_the_newest_turn_verbatim() {
    // Compaction keeps the newest turn word for word and summarizes the older
    // one away. The retained turn is larger than the 20% target share, so it
    // survives on the hard limit: it carries the output the model needs to
    // continue, and a summary cannot stand in for it.
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&windowed_model("retain-model", Some(200_000)))
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response(
        &format!("{} NOISE-ALPHA", "y".repeat(560_000)),
        60_000,
        1_000,
    ));
    provider.push(text_response(
        &format!("{} TAIL-BETA", "z".repeat(200_000)),
        30_000,
        1_000,
    ));
    provider.push(text_response("hard state summary", 170_000, 20));
    provider.push(text_response("third answer", 20_000, 12));
    provider.push(text_response("fourth answer", 20_000, 12));
    let registry = registry(store, &provider);

    let first = run(
        &registry,
        "retain-first",
        user_request(
            "retain-conversation",
            "retain-user-1",
            "read the big report",
            &model.model_hash,
            None,
        ),
    )
    .await;
    assert_eq!(first.summary_started, 0, "the first turn fits the window");

    let second = run(
        &registry,
        "retain-second",
        user_request(
            "retain-conversation",
            "retain-user-2",
            "KEEPME-BETA continue",
            &model.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert_eq!(second.summary_started, 0, "the second turn still fits");

    let third = run(
        &registry,
        "retain-third",
        user_request(
            "retain-conversation",
            "retain-user-3",
            "what next?",
            &model.model_hash,
            second.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert_eq!(third.summary_started, 1);
    assert_eq!(third.summary_completed, 1);

    let requests = provider.requests();
    assert_eq!(requests.len(), 4, "two turns, summary call, continued turn");
    assert!(requests[2].prompt.tools.is_empty());

    let history = &requests[3].history;
    let retained =
        text_message(history, "KEEPME-BETA").expect("the newest turn is replayed verbatim");
    let original = text_message(&requests[1].history, "KEEPME-BETA")
        .expect("the newest turn was sent before the compaction");
    assert_eq!(
        retained, original,
        "a retained turn is the same provider message, byte for byte"
    );
    let position = history
        .iter()
        .position(|message| message.message_id == retained.message_id)
        .expect("the retained turn is in the compacted history");
    let slot = message_text(
        history[..position]
            .last()
            .expect("a slot precedes the turn"),
    )
    .expect("the summary slot is text");
    assert!(slot.contains("<conversation_summary>"));
    assert!(slot.contains("hard state summary"));
    assert!(
        history
            .iter()
            .any(|message| assistant_text(message).is_some_and(|text| text.contains("TAIL-BETA"))),
        "the assistant half of the retained turn comes with it"
    );
    assert!(
        history.iter().all(|message| !assistant_text(message)
            .unwrap_or_default()
            .contains("NOISE-ALPHA")),
        "the older turn is summarized away instead of being replayed"
    );

    let state = third.checkpoints.last().unwrap();
    let archive = archive_of(&third, state);
    assert!(
        state
            .root_prompt_messages_json
            .contains(&archive.summary_message),
        "the archive points at a root of the compacted state"
    );
    assert_ne!(
        Some(&archive.summary_message),
        state.root_prompt_messages_json.last(),
        "the archive does not simply point at the last root"
    );
    assert!(
        archive.window_tail >= 1,
        "the retained turn is reported as a kept window tail"
    );
    let previous_roots = &second.checkpoints.last().unwrap().root_prompt_messages_json;
    assert_eq!(
        archive.summarized_messages.len() + archive.window_tail as usize,
        previous_roots.len() - 1,
        "every root outside the summary is either summarized or kept"
    );

    // 压缩后的下一轮：整段 provider 历史必须是新请求的前缀。单条消息逐字节一致
    // 还不够——任何位置的改写都会让跨轮前缀缓存归零。
    let fourth = run(
        &registry,
        "retain-fourth",
        user_request(
            "retain-conversation",
            "retain-user-4",
            "and then?",
            &model.model_hash,
            third.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert_eq!(fourth.summary_started, 0, "the compacted history fits");
    let requests = provider.requests();
    assert_eq!(requests.len(), 5, "four turns and one summary call");
    let compacted = &requests[3].history;
    let continued = &requests[4].history;
    assert!(
        continued.len() > compacted.len(),
        "a new turn appends messages"
    );
    for (index, message) in compacted.iter().enumerate() {
        assert_eq!(
            serde_json::to_string(message).unwrap(),
            serde_json::to_string(&continued[index]).unwrap(),
            "provider message {index} must stay byte-identical across the turn boundary"
        );
    }
}

#[tokio::test]
async fn explicit_summarize_keeps_recent_turns_and_points_the_archive_at_the_summary() {
    // `/summarize` runs the same replacement assembly as the preflight, so the
    // Cursor state has to describe it: the summary message is located by
    // identity, the retained turns are reported as the window tail, and only
    // the dropped roots count as summarized.
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&windowed_model("summarize-model", Some(200_000)))
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response("first answer", 60_000, 12));
    provider.push(text_response("second answer", 61_000, 12));
    provider.push(text_response("Durable summary", 62_000, 20));
    provider.push(text_response("after compaction", 40_000, 12));
    let registry = registry(store, &provider);

    let first = run(
        &registry,
        "summarize-first",
        user_request(
            "summarize-conversation",
            "summarize-user-1",
            &format!("{} KEEPME-OLD", "x".repeat(240_000)),
            &model.model_hash,
            None,
        ),
    )
    .await;
    let second = run(
        &registry,
        "summarize-second",
        user_request(
            "summarize-conversation",
            "summarize-user-2",
            "keep me KEEPME-NEW",
            &model.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert_eq!(second.summary_started, 0);

    let compacted = run(
        &registry,
        "summarize-command",
        summary_request(
            "summarize-conversation",
            &model.model_hash,
            second.checkpoints.last().unwrap().clone(),
        ),
    )
    .await;
    assert_eq!(compacted.summary, "Durable summary");
    let state = compacted.checkpoints.last().unwrap();
    let archive = archive_of(&compacted, state);
    assert!(
        state
            .root_prompt_messages_json
            .contains(&archive.summary_message),
        "the archive points at a root of the compacted state"
    );
    assert_ne!(
        Some(&archive.summary_message),
        state.root_prompt_messages_json.last(),
        "the archive must not point at whatever root happens to be last"
    );
    assert!(
        archive.window_tail >= 1,
        "the newest turn is kept verbatim, so it is a window tail"
    );
    let previous_roots = &second.checkpoints.last().unwrap().root_prompt_messages_json;
    assert_eq!(
        archive.summarized_messages.len() + archive.window_tail as usize,
        previous_roots.len() - 1,
        "every root outside the summary is either summarized or kept"
    );

    let after = run(
        &registry,
        "summarize-after",
        user_request(
            "summarize-conversation",
            "summarize-user-3",
            "what next?",
            &model.model_hash,
            Some(state.clone()),
        ),
    )
    .await;
    assert_eq!(
        after.summary_started, 0,
        "the compacted prompt has room again"
    );
    let requests = provider.requests();
    let history = &requests[3].history;
    let slot = message_text(&history[0]).expect("the compacted history opens with the slot");
    assert!(slot.contains("Durable summary"));
    let retained = text_message(history, "KEEPME-NEW").expect("the newest turn survives");
    let original = text_message(&requests[1].history, "KEEPME-NEW")
        .expect("the newest turn was sent before the compaction");
    assert_eq!(
        retained, original,
        "the compacted prefix stays byte-stable for the turns that follow"
    );
    assert!(text_message(history, "KEEPME-OLD").is_none());
}

#[tokio::test]
async fn failed_summary_slot_states_the_compaction_and_keeps_the_turns() {
    // A summarizer failure must not shrink the prompt to a fragment of the
    // serialized history, and it must not pretend to summarize: the slot says
    // what happened and the newest turn is still replayed verbatim.
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&windowed_model("fallback-model", Some(200_000)))
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response(
        &format!("{} NOISE-ALPHA", "y".repeat(560_000)),
        60_000,
        1_000,
    ));
    provider.push(text_response(
        &format!("{} TAIL-BETA", "z".repeat(200_000)),
        30_000,
        1_000,
    ));
    provider.push_error(cursor_server::Error::Provider(
        "Anthropic 500 Internal Server Error: upstream unavailable".into(),
    ));
    provider.push_error(cursor_server::Error::Provider(
        "Anthropic 500 Internal Server Error: upstream unavailable".into(),
    ));
    provider.push(text_response("answer after fallback", 20_000, 12));
    let registry = registry(store, &provider);

    let first = run(
        &registry,
        "fallback-first",
        user_request(
            "fallback-conversation",
            "fallback-user-1",
            "read the big report",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let second = run(
        &registry,
        "fallback-second",
        user_request(
            "fallback-conversation",
            "fallback-user-2",
            "KEEPME-FALLBACK continue",
            &model.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert_eq!(second.summary_started, 0);
    let third = run(
        &registry,
        "fallback-third",
        user_request(
            "fallback-conversation",
            "fallback-user-3",
            "what next?",
            &model.model_hash,
            second.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert_eq!(third.summary_started, 1);
    assert_eq!(third.summary_completed, 1, "the compaction still completes");

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        5,
        "two turns, two failed summary attempts, continued turn"
    );
    assert!(requests[2].prompt.tools.is_empty());
    assert!(requests[3].prompt.tools.is_empty());
    let history = &requests[4].history;
    assert!(
        history.iter().all(|message| !message_text(message)
            .unwrap_or_default()
            .contains("Durable recent conversation state:")),
        "the removed fallback pasted a truncated JSON tail"
    );
    let slot = history
        .iter()
        .find_map(|message| {
            message_text(message).filter(|text| text.contains("<conversation_summary>"))
        })
        .expect("the summary slot is in the compacted history");
    assert!(
        slot.contains("summary call failed"),
        "the slot states the failure instead of pretending to summarize"
    );
    assert!(
        slot.contains("Deterministic digest of the dropped history"),
        "the fallback keeps a deterministic digest instead of a bare sentence"
    );
    assert!(
        slot.contains("TAIL-BETA"),
        "the digest keeps the newest assistant marker from the dropped history"
    );
    // The summary digest also quotes the marker, so select the retained user
    // turn itself rather than the first text match in the replacement.
    let retained = history
        .iter()
        .find_map(|message| {
            message_text(message).filter(|text| {
                !text.contains("<conversation_summary>") && text.contains("KEEPME-FALLBACK")
            })
        })
        .expect("the newest turn is replayed verbatim");
    let original = requests[1]
        .history
        .iter()
        .find_map(|message| message_text(message).filter(|text| text.contains("KEEPME-FALLBACK")))
        .expect("the newest turn was sent before the compaction");
    assert_eq!(
        retained, original,
        "a retained turn is the same provider message, byte for byte"
    );
    assert!(
        history
            .iter()
            .any(|message| assistant_text(message).is_some_and(|text| text.contains("TAIL-BETA"))),
        "the assistant half of the retained turn comes with it"
    );
    assert!(
        history.iter().all(|message| !assistant_text(message)
            .unwrap_or_default()
            .contains("NOISE-ALPHA")),
        "history the failed summary dropped stays dropped"
    );
}

/// 压缩把状态结构重置到压缩后的基线。
///
/// 客户端带上来的 `file_states` / `file_states_v2` 描述的是**已经被摘要掉的**那段历史
/// （blob 里存的是压缩前的文件内容），原样写回去，Cursor 侧会按这份基线把文件显示成压缩前
/// 的样子（审计 05/P2-1）。这条同时钉住另一半：请求带来的状态结构必须被 builder 接管
/// （`adopt` 是一次移动，不再整份克隆）——没有接管的话，未压缩那一轮的 checkpoint 就会
/// 直接丢掉文件状态。
#[tokio::test]
async fn a_compaction_resets_the_file_state_baseline() {
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&windowed_model("baseline-model", Some(1_000_000)))
        .await
        .unwrap();

    // 一轮正常提交（不压缩）：基线原样带过去。
    let quiet = fake_provider::FakeProvider::default();
    quiet.push(text_response("first answer", 4_000, 12));
    quiet.push(text_response("second answer", 4_100, 12));
    let quiet_registry = registry(store.clone(), &quiet);
    let first = run(
        &quiet_registry,
        "baseline-quiet-1",
        user_request(
            "baseline-quiet-conversation",
            "baseline-quiet-user-1",
            "start",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let mut state = first.checkpoints.last().cloned().unwrap();
    inject_file_states(&mut state);
    let second = run(
        &quiet_registry,
        "baseline-quiet-2",
        user_request(
            "baseline-quiet-conversation",
            "baseline-quiet-user-2",
            "continue",
            &model.model_hash,
            Some(state),
        ),
    )
    .await;
    assert_eq!(second.summary_started, 0);
    let settled = second.checkpoints.last().unwrap();
    assert!(
        settled.file_states_v2.contains_key("src/lib.rs"),
        "没有压缩时,请求带来的文件状态必须原样带回去"
    );
    assert!(
        settled.file_states.contains_key("src/lib.rs"),
        "旧版 file_states 同样要带上"
    );

    // 同一份基线遇到压缩：压缩之后的 checkpoint 必须清空，压缩之前的保持不动。
    let compacting = fake_provider::FakeProvider::default();
    compacting.push(text_response("first answer", 4_000, 12));
    compacting.push_error(cursor_server::Error::Provider(
        "Anthropic 400 Bad Request: {\"type\":\"error\",\"error\":{\"type\":\
         \"invalid_request_error\",\"message\":\"prompt is too long: 1002148 tokens > \
         1000000 maximum\"}}"
            .into(),
    ));
    compacting.push(text_response("durable summary", 3_000, 20));
    compacting.push(text_response("answer after compaction", 500, 20));
    let compacting_registry = registry(store.clone(), &compacting);
    let first = run(
        &compacting_registry,
        "baseline-compact-1",
        user_request(
            "baseline-compact-conversation",
            "baseline-compact-user-1",
            "start",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let mut state = first.checkpoints.last().cloned().unwrap();
    inject_file_states(&mut state);
    let second = run(
        &compacting_registry,
        "baseline-compact-2",
        user_request(
            "baseline-compact-conversation",
            "baseline-compact-user-2",
            "continue",
            &model.model_hash,
            Some(state),
        ),
    )
    .await;
    assert_eq!(second.summary_started, 1);
    for checkpoint in second
        .checkpoints
        .iter()
        .filter(|state| state.summary.is_none())
    {
        assert!(
            checkpoint.file_states_v2.contains_key("src/lib.rs"),
            "压缩之前推给客户端的状态仍然带着文件状态"
        );
    }
    let compacted = second
        .checkpoints
        .iter()
        .find(|state| state.summary.is_some())
        .expect("压缩之后的 checkpoint");
    assert!(
        compacted.file_states_v2.is_empty() && compacted.file_states.is_empty(),
        "压缩后的基线里不能残留压缩前的文件状态"
    );
}

/// 给状态结构塞一份"压缩前的文件状态"（值只是 32 字节 blob id，测试不读内容）。
fn inject_file_states(state: &mut pb::ConversationStateStructure) {
    state.file_states.insert("src/lib.rs".into(), vec![7u8; 32]);
    state.file_states_v2.insert(
        "src/lib.rs".into(),
        pb::FileStateStructure {
            content: Some(vec![9u8; 32]),
            initial_content: None,
        },
    );
}

fn registry(
    store: cursor_server::store::Store,
    provider: &fake_provider::FakeProvider,
) -> TransportRegistry {
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    )
}

fn message_text(message: &ProjectedMessage) -> Option<String> {
    let ProjectedContent::Parts(parts) = &message.content else {
        return None;
    };
    Some(
        parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn text_message<'a>(history: &'a [ProjectedMessage], needle: &str) -> Option<&'a ProjectedMessage> {
    history
        .iter()
        .find(|message| message_text(message).is_some_and(|text| text.contains(needle)))
}

fn assistant_text(message: &ProjectedMessage) -> Option<&str> {
    match &message.content {
        ProjectedContent::Assistant { text, .. } => Some(text.as_str()),
        _ => None,
    }
}

fn archive_of(
    output: &Output,
    state: &pb::ConversationStateStructure,
) -> pb::ConversationSummaryArchive {
    let archive_id = state
        .summary_archive
        .as_ref()
        .expect("compaction records a summary archive");
    pb::ConversationSummaryArchive::decode(output.blobs[archive_id].as_slice()).unwrap()
}

#[tokio::test]
async fn a_second_overflow_refusal_reuses_the_summary_and_drops_the_recent_turns() {
    // The ladder that keeps the worst case equal to 1.0.22: the retained
    // compaction is attempted first, and if the provider still refuses because
    // the real prompt is larger than the estimator thought, the next compaction
    // reuses the existing summary, drops the retained turns, and does not pay
    // for a second summarize call over nearly the same history.
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&windowed_model("ladder-model", Some(200_000)))
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response(
        &format!("{} BIG-TAIL", "y".repeat(400_000)),
        150_000,
        1_000,
    ));
    provider.push_error(cursor_server::Error::Provider(
        "Anthropic 400 Bad Request: {\"type\":\"error\",\"error\":{\"type\":\
         \"invalid_request_error\",\"message\":\"prompt is too long: 210000 tokens > \
         200000 maximum\"}}"
            .into(),
    ));
    provider.push(text_response("first summary", 100_000, 20));
    provider.push_error(cursor_server::Error::Provider(
        "Anthropic 400 Bad Request: {\"type\":\"error\",\"error\":{\"type\":\
         \"invalid_request_error\",\"message\":\"prompt is too long: 205000 tokens > \
         200000 maximum\"}}"
            .into(),
    ));
    provider.push(text_response("answer after compacting twice", 20_000, 12));
    let registry = registry(store, &provider);

    let first = run(
        &registry,
        "ladder-first",
        user_request(
            "ladder-conversation",
            "ladder-user-1",
            "read the big report",
            &model.model_hash,
            None,
        ),
    )
    .await;
    assert_eq!(first.summary_started, 0);

    let second = run(
        &registry,
        "ladder-second",
        user_request(
            "ladder-conversation",
            "ladder-user-2",
            "continue",
            &model.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert_eq!(second.summary_started, 2, "two compactions in one run");
    assert_eq!(second.summary_completed, 2);
    assert_eq!(second.turn_ended, 1);

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        5,
        "turn, refused retry, retained compaction, refused retry, answer"
    );
    assert!(requests[2].prompt.tools.is_empty(), "retained compaction");
    assert!(
        requests[3]
            .history
            .iter()
            .any(|message| assistant_text(message).is_some_and(|text| text.contains("BIG-TAIL"))),
        "the first compaction keeps the newest turn verbatim"
    );
    assert!(
        requests[4]
            .history
            .iter()
            .all(|message| !assistant_text(message)
                .unwrap_or_default()
                .contains("BIG-TAIL")),
        "the summary-only step drops the retained turns"
    );
    let summary_slot = requests[4]
        .history
        .iter()
        .filter_map(message_text)
        .find(|text| text.contains("<conversation_summary>"))
        .expect("the reused summary slot is in the compacted history");
    assert!(
        summary_slot.contains("first summary"),
        "the existing summary is reused instead of being re-generated"
    );
    assert!(
        summary_slot.contains("BIG-TAIL"),
        "the digest keeps the newest marker from the dropped retained turns"
    );
    assert!(
        !summary_slot.contains("second summary"),
        "no second summarize call ran"
    );
    assert!(text_message(&requests[4].history, "continue").is_some());
}

#[tokio::test]
async fn a_preflight_compaction_advances_the_overflow_ladder() {
    // A preflight compaction already wrote a summary slot. When the provider
    // still refuses the compacted prompt, the ladder must reuse that summary
    // instead of paying for a second summarize call over nearly the same
    // history, and the reused slot must point at the archive of the span this
    // step drops.
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&windowed_model("preflight-ladder-model", Some(100_000)))
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    // The first turn's huge assistant output makes the next run's estimate
    // exceed the window, so it preflights a compaction before dispatching.
    provider.push(text_response(&"x".repeat(400_000), 150_000, 1_000));
    // Preflight summarize call.
    provider.push(text_response("preflight summary", 20_000, 20));
    // The compacted prompt is still refused. No number in the prose, so the
    // learned window stays the configured one.
    provider.push_error(cursor_server::Error::Provider(
        "Anthropic 400 Bad Request: prompt is too long".into(),
    ));
    provider.push(text_response("answer after reuse", 20_000, 12));
    let registry = registry(store, &provider);

    let first = run(
        &registry,
        "preflight-ladder-first",
        user_request(
            "preflight-ladder-conversation",
            "preflight-ladder-user-1",
            "start",
            &model.model_hash,
            None,
        ),
    )
    .await;
    assert_eq!(first.summary_started, 0);

    let second = run(
        &registry,
        "preflight-ladder-second",
        user_request(
            "preflight-ladder-conversation",
            "preflight-ladder-user-2",
            "continue",
            &model.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert_eq!(second.turn_ended, 1);
    assert_eq!(
        second.summary_started, 2,
        "preflight compaction plus the summary-only step"
    );

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        4,
        "first turn, preflight summary, refused call, answer"
    );
    let summarize_requests = requests
        .iter()
        .filter(|request| request.prompt.tools.is_empty())
        .count();
    assert_eq!(
        summarize_requests, 1,
        "the summary-only step after a preflight compaction must reuse the slot, not summarize again"
    );
    let summary_slot = requests[3]
        .history
        .iter()
        .filter_map(message_text)
        .find(|text| text.contains("<conversation_summary>"))
        .expect("the reused summary slot is in the compacted history");
    assert!(summary_slot.contains("preflight summary"));
    assert!(
        summary_slot.contains("- Additional archived span:"),
        "the reused summary also references the archive written for this step: {summary_slot}"
    );
}

#[derive(Default)]
struct Output {
    checkpoints: Vec<pb::ConversationStateStructure>,
    blobs: HashMap<Vec<u8>, Vec<u8>>,
    summary: String,
    summary_started: usize,
    summary_completed: usize,
    turn_ended: usize,
    token_delta: usize,
    interaction_events: Vec<String>,
}

async fn run(
    registry: &TransportRegistry,
    request_id: &str,
    request: pb::AgentClientMessage,
) -> Output {
    let handle = registry.get_or_create(request_id).await.unwrap();
    let mut receiver = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(request),
        })
        .await
        .unwrap();
    let mut append_seqno = 1;
    let mut output = Output::default();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            return output;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                if let Some(pb::kv_server_message::Message::SetBlobArgs(set)) = kv.message {
                    output.blobs.insert(set.blob_id, set.blob_data);
                }
                handle
                    .command(TransportCommand::Append {
                        seqno: append_seqno,
                        message: Box::new(kv_ack(kv.id)),
                    })
                    .await
                    .unwrap();
                append_seqno += 1;
            }
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) => {
                output.checkpoints.push(state)
            }
            Some(pb::agent_server_message::Message::InteractionUpdate(update)) => {
                match update.message {
                    Some(pb::interaction_update::Message::SummaryStarted(_)) => {
                        output.summary_started += 1;
                        output.interaction_events.push("summary_started".into());
                    }
                    Some(pb::interaction_update::Message::Summary(delta)) => {
                        output.summary.push_str(&delta.summary)
                    }
                    Some(pb::interaction_update::Message::SummaryCompleted(_)) => {
                        output.summary_completed += 1;
                        output.interaction_events.push("summary_completed".into());
                    }
                    Some(pb::interaction_update::Message::TurnEnded(_)) => output.turn_ended += 1,
                    Some(pb::interaction_update::Message::TokenDelta(delta)) => {
                        output.token_delta += 1;
                        output
                            .interaction_events
                            .push(format!("token_delta:{}", delta.tokens));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn text_response(text: &str, input: u64, output: u64) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: format!("call-{text}"),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta(text.into()),
        ModelEvent::TextEnd,
        ModelEvent::Usage(Usage {
            input_tokens: Some(input),
            context_input_tokens: Some(input),
            output_tokens: Some(output),
            total_tokens: Some(input + output),
            ..Default::default()
        }),
        ModelEvent::Done(FinishReason::Stop),
    ]
}

fn user_request(
    conversation_id: &str,
    message_id: &str,
    text: &str,
    model_id: &str,
    state: Option<pb::ConversationStateStructure>,
) -> pb::AgentClientMessage {
    let user = pb::UserMessage {
        text: text.into(),
        message_id: message_id.into(),
        mode: pb::AgentMode::Agent as i32,
        ..Default::default()
    };
    request(
        conversation_id,
        model_id,
        state,
        pb::conversation_action::Action::UserMessageAction(pb::UserMessageAction {
            user_message: Some(user),
            request_context: Some(pb::RequestContext::default()),
            ..Default::default()
        }),
    )
}

fn summary_request(
    conversation_id: &str,
    model_id: &str,
    state: pb::ConversationStateStructure,
) -> pb::AgentClientMessage {
    let user = pb::UserMessage {
        text: "/summarize".into(),
        message_id: "summary-command".into(),
        mode: pb::AgentMode::Agent as i32,
        ..Default::default()
    };
    request(
        conversation_id,
        model_id,
        Some(state),
        pb::conversation_action::Action::UserMessageAction(pb::UserMessageAction {
            user_message: Some(user),
            request_context: Some(pb::RequestContext::default()),
            ..Default::default()
        }),
    )
}

fn request(
    conversation_id: &str,
    model_id: &str,
    state: Option<pb::ConversationStateStructure>,
    action: pb::conversation_action::Action,
) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                requested_model: Some(pb::RequestedModel {
                    model_id: model_id.into(),
                    ..Default::default()
                }),
                action: Some(pb::ConversationAction {
                    action: Some(action),
                    ..Default::default()
                }),
                conversation_id: Some(conversation_id.into()),
                conversation_state: state,
                run_id: Some("reusable-wire-run-id".into()),
                ..Default::default()
            },
        )),
    }
}

fn kv_ack(id: u32) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::KvClientMessage(
            pb::KvClientMessage {
                id,
                message: Some(pb::kv_client_message::Message::SetBlobResult(
                    pb::SetBlobResult { error: None },
                )),
            },
        )),
    }
}
/// 预留比例是真正的策略开关：同一段历史，默认 10% 放行，调到 50% 就必须压缩。
#[tokio::test]
async fn raising_the_reserve_percent_makes_the_same_history_compact() {
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&windowed_model("reserve-model", Some(100_000)))
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response(
        &format!("{} BIG-TURN", "q".repeat(240_000)),
        20_000,
        1_000,
    ));
    provider.push(text_response("second answer", 60_000, 20));
    provider.push(text_response("reserve summary", 40_000, 20));
    provider.push(text_response("third answer", 20_000, 20));
    let registry = registry(store.clone(), &provider);

    let first = run(
        &registry,
        "reserve-first",
        user_request(
            "reserve-conversation",
            "reserve-user-1",
            "write the big report",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let second = run(
        &registry,
        "reserve-second",
        user_request(
            "reserve-conversation",
            "reserve-user-2",
            "continue",
            &model.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert_eq!(
        second.summary_started, 0,
        "the default 10% reserve still fits this history"
    );

    store
        .set_compaction_settings(cursor_server::store::CompactionSettings {
            auto: true,
            reserve_percent: Some(50),
            ..Default::default()
        })
        .await
        .unwrap();

    let third = run(
        &registry,
        "reserve-third",
        user_request(
            "reserve-conversation",
            "reserve-user-3",
            "continue again",
            &model.model_hash,
            second.checkpoints.last().cloned(),
        ),
    )
    .await;
    assert!(
        third.summary_started >= 1,
        "a 50% reserve compacts the same history"
    );
}
