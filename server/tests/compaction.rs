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
    assert_eq!(summary.summary, "Durable summary");
    let archive_id = compacted_state.summary_archive.as_ref().unwrap();
    let archive =
        pb::ConversationSummaryArchive::decode(compacted.blobs[archive_id].as_slice()).unwrap();
    assert_eq!(archive.summary, "Durable summary");
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
                if text == "<conversation_summary>\nDurable summary\n</conversation_summary>")
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
        4,
        "two turns, failed summary call, continued turn"
    );
    assert!(requests[2].prompt.tools.is_empty());
    let history = &requests[3].history;
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
    let retained =
        text_message(history, "KEEPME-FALLBACK").expect("the newest turn is replayed verbatim");
    let original = text_message(&requests[1].history, "KEEPME-FALLBACK")
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
