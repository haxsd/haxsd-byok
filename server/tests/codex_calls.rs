//! 调用来源是显式数据，不通过 ID 前缀猜测新调用。
#[path = "support/fixtures.rs"]
mod fixtures;
use cursor_server::model::{CallClient, CallPurpose, NewLlmCall, ProviderType};

#[tokio::test]
async fn call_source_round_trips_even_when_its_id_looks_like_another_client() {
    let (_dir, store) = fixtures::temp_store().await;
    store
        .start_llm_call(&NewLlmCall {
            purpose: CallPurpose::Chat,
            client_source: CallClient::Codex,
            call_id: "devin:misleading-id".into(),
            run_id: "r".into(),
            conversation_id: "c".into(),
            provider_call_index: 0,
            model_hash: "m".into(),
            provider_type: ProviderType::OpenAiChat,
            provider_url: "https://example.com".into(),
            request_type: ProviderType::OpenAiChat,
            request_url: "https://example.com/chat".into(),
            model_id: "model".into(),
            display_name: "Model".into(),
            reasoning_effort: None,
            fast: false,
            message_count: 1,
            tool_count: 0,
            detailed: false,
        })
        .await
        .unwrap();
    let call = store
        .llm_call("devin:misleading-id")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(call).unwrap()["client_source"],
        "codex"
    );
    assert_eq!(store.devin_call_count().await.unwrap(), 0);
}
