//! Verifies BreakMessages, cancellation, shutdown, and Finalizing races.
#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::sync::Arc;

use bytes::Bytes;
use cursor_server::{
    cursor::prompting::{PromptAssets, PromptCompiler},
    cursor::protocol::{connect, proto::agent::v1 as pb},
    cursor::{TransportCommand, TransportParent, TransportRegistry},
    model::{
        CanonicalMessage, ConversationId, MessageContent, ModelConfigInput, ModelSpec, ModelType,
        Origin, PreparedRun, ProjectedContent, ProjectedMessage, PromptSpec, Role, RunAction,
        RunId, RunKind, ToolResultContent, Usage, OPENAI_CHAT_ENDPOINT,
    },
    provider::{FinishReason, ModelEvent},
    run::{self, CommandResult, CommitCause, RunEngine, RunEvent, RunOutcome, RunPhase},
    store::RunStatus,
};
use prost::Message;

#[tokio::test]
async fn finalizing_rejects_late_messages_for_the_next_run() {
    let (_directory, store) = fixtures::temp_store().await;
    let conversation_id = ConversationId::new("finalizing-conversation");
    let base_checkpoint_id = store.ensure_conversation(&conversation_id).await.unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "final-call".into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta("done".into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(FinishReason::Stop),
    ]);
    let prepared = PreparedRun {
        run_id: RunId::new("finalizing-run"),
        cursor_request_id: None,
        conversation_id,
        kind: RunKind::Root,
        model: ModelSpec::new("model"),
        prompt: PromptSpec {
            instructions: String::new(),
            tools: Vec::new(),
        },
        initial_messages: Vec::new(),
        action: RunAction::Resume {
            pending_tool_round: None,
        },
        base_checkpoint_id,
    };
    let (port, mut session, handle) = run::channel(prepared.run_id.clone(), 32);
    let cancellation = handle.cancellation();
    let task = tokio::spawn(async move {
        RunEngine::new(store, Arc::new(provider))
            .run(prepared, port, cancellation)
            .await
    });

    loop {
        match session.events.recv().await.unwrap() {
            RunEvent::MessagesCommitted(committed) if committed.cause == CommitCause::FinalTurn => {
                assert_eq!(handle.phase(), RunPhase::Finalizing);
                assert_eq!(
                    handle
                        .insert_messages(
                            "late-event".into(),
                            vec![CanonicalMessage::text(
                                "late-message",
                                Role::User,
                                Origin::Runtime,
                                "late",
                            )],
                        )
                        .await,
                    CommandResult::RunClosing
                );
                committed.barrier.complete(Ok(()));
            }
            RunEvent::Ended(outcome) => {
                assert_eq!(outcome, RunOutcome::Completed);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(task.await.unwrap(), RunOutcome::Completed);
}

#[tokio::test]
async fn break_messages_emits_one_cycle_boundary_before_the_runtime_commit() {
    let (_directory, store) = fixtures::temp_store().await;
    let conversation_id = ConversationId::new("cycle-boundary-conversation");
    let base_checkpoint_id = store.ensure_conversation(&conversation_id).await.unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push_pending();
    provider.push(text_response("continued"));
    let prepared = PreparedRun {
        run_id: RunId::new("cycle-boundary-run"),
        cursor_request_id: None,
        conversation_id,
        kind: RunKind::Root,
        model: ModelSpec::new("model"),
        prompt: PromptSpec {
            instructions: String::new(),
            tools: Vec::new(),
        },
        initial_messages: Vec::new(),
        action: RunAction::Resume {
            pending_tool_round: None,
        },
        base_checkpoint_id,
    };
    let (port, mut session, handle) = run::channel(prepared.run_id.clone(), 32);
    let cancellation = handle.cancellation();
    let engine_store = store.clone();
    let engine_provider = provider.clone();
    let engine = tokio::spawn(async move {
        RunEngine::new(engine_store, Arc::new(engine_provider))
            .run(prepared, port, cancellation)
            .await
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    while provider.requests().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "first model cycle did not start"
        );
        tokio::task::yield_now().await;
    }
    let mut message = CanonicalMessage::text(
        "cycle-boundary-message",
        Role::User,
        Origin::Runtime,
        "new information",
    );
    message.runtime_event_id = Some("cycle-boundary-event".into());
    let command = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .break_messages("cycle-boundary-event".into(), vec![message])
                .await
        }
    });

    let mut lifecycle = Vec::new();
    loop {
        match session.events.recv().await.unwrap() {
            RunEvent::CycleInterrupted => lifecycle.push("interrupted"),
            RunEvent::MessagesCommitted(committed) => {
                if matches!(committed.cause, CommitCause::RuntimeEvent { .. }) {
                    lifecycle.push("runtime-committed");
                }
                committed.barrier.complete(Ok(()));
            }
            RunEvent::Ended(outcome) => {
                assert_eq!(outcome, RunOutcome::Completed);
                break;
            }
            _ => {}
        }
    }

    assert_eq!(lifecycle, ["interrupted", "runtime-committed"]);
    assert_eq!(command.await.unwrap(), CommandResult::Applied);
    assert_eq!(engine.await.unwrap(), RunOutcome::Completed);
}

#[tokio::test]
async fn a_replaced_run_cannot_overwrite_its_cancelled_status() {
    let (_directory, store) = fixtures::temp_store().await;
    let conversation_id = ConversationId::new("conversation");
    let base_checkpoint_id = store.ensure_conversation(&conversation_id).await.unwrap();
    let prepared = |run_id: &str| PreparedRun {
        run_id: RunId::new(run_id),
        cursor_request_id: None,
        conversation_id: conversation_id.clone(),
        kind: RunKind::Root,
        model: ModelSpec::new("model"),
        prompt: PromptSpec {
            instructions: String::new(),
            tools: Vec::new(),
        },
        initial_messages: Vec::new(),
        action: RunAction::Resume {
            pending_tool_round: None,
        },
        base_checkpoint_id,
    };
    let first = prepared("first");
    let second = prepared("second");

    store.claim_run(&first).await.unwrap();
    sqlx::query(
        "INSERT INTO llm_calls(
            call_id, run_id, conversation_id, provider_call_index,
            provider_type, provider_url, request_type, request_url,
            model_id, display_name, status,
            created_at_ms, message_count, tool_count, detailed
         ) VALUES (
            'first:0', 'first', 'conversation', 0,
            'openai-chat', 'https://example.com/v1',
            'openai-chat', 'https://example.com/v1/chat/completions',
            'model', 'Model', 'running',
            unixepoch('subsec') * 1000, 1, 0, 0
         )",
    )
    .execute(store.pool())
    .await
    .unwrap();
    store.claim_run(&second).await.unwrap();
    assert!(!store
        .finish_run(&first.run_id, RunStatus::Completed, None, None,)
        .await
        .unwrap());

    let status: String = sqlx::query_scalar("SELECT status FROM runs WHERE run_id = 'first'")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let active: Option<String> = sqlx::query_scalar(
        "SELECT active_run_id FROM conversations WHERE conversation_id = 'conversation'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(status, "cancelled");
    assert_eq!(active.as_deref(), Some("second"));
    let call: (String, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT status, finished_at_ms, duration_ms FROM llm_calls WHERE call_id = 'first:0'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(call.0, "cancelled");
    assert!(call.1.is_some());
    assert!(call.2.is_some());
}

#[tokio::test]
async fn registry_shutdown_cancels_runs_and_closes_run_sse_outputs() {
    let (_directory, store) = fixtures::temp_store().await;
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(fake_provider::FakeProvider::default()),
        PromptCompiler::new(assets),
    );
    let handle = registry.get_or_create("active-run").await.unwrap();
    let mut output = handle.subscribe();

    registry.shutdown().await;

    let terminal = output.recv().await.expect("canceled EndStream");
    let (flags, payload) = connect::decode_frames(&terminal).unwrap().pop().unwrap();
    assert_eq!(flags, connect::END_STREAM_FLAG);
    let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(payload["error"]["code"], "canceled");
    assert_eq!(output.recv().await, None);
}

#[tokio::test]
async fn client_heartbeat_returns_a_server_protocol_heartbeat() {
    let (_directory, store) = fixtures::temp_store().await;
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(fake_provider::FakeProvider::default()),
        PromptCompiler::new(assets),
    );
    let handle = registry.get_or_create("heartbeat-run").await.unwrap();
    let mut output = handle.subscribe();

    cursor_server::api::cursor::bidi::append(
        &registry,
        cursor_server::api::cursor::bidi::DecodedAppend {
            request_id: "heartbeat-run".into(),
            // A transport heartbeat must not wait for missing application messages.
            seqno: 1,
            message: pb::AgentClientMessage {
                message: Some(pb::agent_client_message::Message::ClientHeartbeat(
                    pb::ClientHeartbeat {},
                )),
            },
        },
        None,
    )
    .await
    .unwrap();

    let frame = tokio::time::timeout(std::time::Duration::from_secs(1), output.recv())
        .await
        .unwrap()
        .unwrap();
    let (_, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
    let message = pb::AgentServerMessage::decode(payload).unwrap();
    assert!(matches!(
        message.message,
        Some(pb::agent_server_message::Message::InteractionUpdate(
            pb::InteractionUpdate {
                message: Some(pb::interaction_update::Message::Heartbeat(_)),
            }
        ))
    ));

    registry.shutdown().await;
}

#[tokio::test]
async fn runtime_cancel_action_aborts_active_exec_before_canceled_end_stream() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "ignored".into(),
        },
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: "call-1".into(),
            name: "Read".into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: "{\"path\":\"/tmp/a\"}".into(),
        },
        ModelEvent::ToolCallEnd { index: 0 },
        ModelEvent::Done(FinishReason::ToolUse),
    ]);
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(store, Arc::new(provider), PromptCompiler::new(assets));
    let handle = registry.get_or_create("cancel-request").await.unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run()),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let exec_id = loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .unwrap();
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        assert_eq!(
            flags & connect::END_STREAM_FLAG,
            0,
            "Run ended before Exec: {}",
            String::from_utf8_lossy(&payload)
        );
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                handle
                    .command(TransportCommand::Append {
                        seqno: append_seqno,
                        message: Box::new(kv_ack(kv.id)),
                    })
                    .await
                    .unwrap();
                append_seqno += 1;
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec)) => break exec.id,
            _ => {}
        }
    };

    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_cancel_action()),
        })
        .await
        .unwrap();
    let mut saw_abort = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before canceled EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(json["error"]["code"], "canceled");
            assert!(saw_abort, "ExecServerAbort must precede canceled EndStream");
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::ExecServerControlMessage(control)) =
            server.message
        {
            let Some(pb::exec_server_control_message::Message::Abort(abort)) = control.message
            else {
                panic!("expected ExecServerAbort")
            };
            assert_eq!(abort.id, exec_id);
            saw_abort = true;
        }
    }
    assert_eq!(output.recv().await, None);
}

#[tokio::test]
async fn queued_user_message_after_turn_ended_starts_the_next_turn() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response("first turn"));
    provider.push(text_response("queued turn"));
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
    let handle = registry.get_or_create("queued-after-turn").await.unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "queued-after-turn",
                "queued-after-turn-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    wait_for_turn_ended(&handle, &mut output, &mut append_seqno).await;
    assert_transport_remains_open(&handle, &mut output, &mut append_seqno).await;

    cursor_server::api::cursor::bidi::append(
        &registry,
        cursor_server::api::cursor::bidi::DecodedAppend {
            request_id: "queued-after-turn".into(),
            seqno: append_seqno,
            message: runtime_user_message(),
        },
        None,
    )
    .await
    .unwrap();
    append_seqno += 1;

    let mut text = String::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("queued turn closed without EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(payload.as_ref(), b"{}");
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::InteractionUpdate(update)) = server.message {
            if let Some(pb::interaction_update::Message::TextDelta(delta)) = update.message {
                text.push_str(&delta.text);
            }
        }
        acknowledge_kv(&handle, &mut append_seqno, &frame).await;
    }

    assert!(text.contains("queued turn"));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        &requests[1].history[..requests[0].history.len()],
        requests[0].history.as_slice(),
        "queued continuation must preserve the first provider request as a prefix"
    );
    let history = serde_json::to_string(&requests[1].history).unwrap();
    assert!(history.contains("queued follow-up"));
}

#[tokio::test]
async fn runtime_user_message_action_interrupts_and_continues_with_new_message() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push_pending();
    provider.push(text_response("continued after user interruption"));
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
    let handle = registry
        .get_or_create("user-message-request")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "user-message-request",
                "user-message-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while provider.requests().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "provider did not start"
        );
        if let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), output.recv()).await
        {
            let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
            if flags & connect::END_STREAM_FLAG != 0 {
                panic!("initial run ended: {}", String::from_utf8_lossy(&payload));
            }
            acknowledge_kv(&handle, &mut append_seqno, &frame).await;
        }
    }
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_user_message()),
        })
        .await
        .unwrap();

    let mut saw_continued = false;
    let mut append_seqno = append_seqno + 1;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before successful EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(payload.as_ref(), b"{}");
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::InteractionUpdate(update)) = server.message {
            if let Some(pb::interaction_update::Message::TextDelta(delta)) = update.message {
                saw_continued |= delta.text.contains("continued after user interruption");
            }
        }
        acknowledge_kv(&handle, &mut append_seqno, &frame).await;
    }
    assert!(saw_continued);
    assert_eq!(provider.requests().len(), 2);
    let history = serde_json::to_string(&provider.requests()[1].history).unwrap();
    assert!(history.contains("queued follow-up"));
}

#[tokio::test]
async fn runtime_user_message_reports_delivered_and_appended() {
    // A runtime user message is queued into `pending_injections` under a
    // `user-message:{id}` key, but the commit correlation only handled the
    // `inject-context:` prefix, so the entry was never cleared: the client
    // never saw Delivered/UserMessageAppended and every later tool round was
    // detached (a hang). This asserts the full delivery sequence.
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push_pending();
    provider.push(text_response("continued after user interruption"));
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
    let handle = registry
        .get_or_create("user-message-events-request")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "user-message-events-request",
                "user-message-events-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while provider.requests().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "provider did not start"
        );
        if let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), output.recv()).await
        {
            acknowledge_kv(&handle, &mut append_seqno, &frame).await;
        }
    }
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_user_message()),
        })
        .await
        .unwrap();
    append_seqno += 1;

    let mut protocol_events = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before successful EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(payload.as_ref(), b"{}");
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::InteractionUpdate(update)) = server.message {
            match update.message {
                Some(pb::interaction_update::Message::ContextInjectionState(update)) => {
                    assert_eq!(update.injection_id, "user-message:queued-user");
                    match update.state.and_then(|state| state.state) {
                        Some(pb::context_injection_state::State::Queued(_)) => {
                            protocol_events.push("queued")
                        }
                        Some(pb::context_injection_state::State::Delivered(delivered)) => {
                            assert!(!delivered.delivery_batch_id.is_empty());
                            assert!(delivered.delivered_at_ms > 0);
                            protocol_events.push("delivered");
                        }
                        _ => {}
                    }
                }
                Some(pb::interaction_update::Message::UserMessageAppended(update)) => {
                    let user = update.user_message.expect("appended user message");
                    assert_eq!(user.message_id, "queued-user");
                    assert_eq!(user.text, "queued follow-up");
                    protocol_events.push("user_message_appended");
                }
                Some(pb::interaction_update::Message::TextDelta(update))
                    if update.text.contains("continued after user interruption") =>
                {
                    protocol_events.push("continued_output");
                }
                _ => {}
            }
        }
        acknowledge_kv(&handle, &mut append_seqno, &frame).await;
    }

    assert_eq!(
        protocol_events,
        [
            "queued",
            "delivered",
            "user_message_appended",
            "continued_output"
        ]
    );
}

#[tokio::test]
async fn tool_call_with_empty_arguments_does_not_fail_the_run() {
    // A tool call that carries no arguments streams no argument text. Parsing it
    // as JSON must yield an empty object (as the model cycle already does), not
    // fail the run with `EOF while parsing a value`.
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(tool_response("call-1", "UpdateCurrentStep", ""));
    provider.push(text_response("done after empty-argument tool"));
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
    let handle = registry.get_or_create("empty-args-request").await.unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "empty-args-request",
                "empty-args-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let mut saw_done = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before successful EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(
                payload.as_ref(),
                b"{}",
                "run failed: {}",
                String::from_utf8_lossy(&payload)
            );
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::InteractionUpdate(update)) = server.message {
            if let Some(pb::interaction_update::Message::TextDelta(delta)) = update.message {
                saw_done |= delta.text.contains("done after empty-argument tool");
            }
        }
        acknowledge_kv(&handle, &mut append_seqno, &frame).await;
    }
    assert!(saw_done);
    assert_eq!(provider.requests().len(), 2);
}

/// 同一轮多个工具的结果几乎同时到达时，整轮仍要正常收尾。
///
/// 收尾与否由每次提交自己的 `settled` 决定，不能在 output 层事后读实时轮状态：
/// 并行结果可能已经全部落库，把"非收尾提交"误判成收尾就会要求一个不属于它的
/// completion barrier（`settled tool round has no completion barrier`）。
#[tokio::test]
async fn parallel_tool_results_complete_the_round_without_a_protocol_error() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(parallel_tool_response(&[
        ("call-1", "Read", "{\"path\":\"/tmp/a\"}"),
        ("call-2", "Read", "{\"path\":\"/tmp/b\"}"),
    ]));
    provider.push(text_response("done after parallel tools"));
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
    let handle = registry
        .get_or_create("parallel-tools-request")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "parallel-tools-request",
                "parallel-tools-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let first = wait_for_exec(&handle, &mut output, &mut append_seqno, "Read").await;
    let second = wait_for_exec(&handle, &mut output, &mut append_seqno, "Read").await;
    assert_ne!(first, second, "两个调用必须各自拿到一个 exec id");
    // 两个结果背靠背送达，中间不给 output 层处理第一个结果的机会。
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(read_success(first)),
        })
        .await
        .unwrap();
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno + 1,
            message: Box::new(read_success(second)),
        })
        .await
        .unwrap();
    append_seqno += 2;

    drain_successfully(&handle, &mut output, &mut append_seqno).await;
}

#[tokio::test]
async fn injected_user_context_restarts_only_the_active_model_cycle() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push_pending();
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "continued".into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta("continued after injection".into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(FinishReason::Stop),
    ]);
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
    let handle = registry.get_or_create("inject-request").await.unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for("inject-request", "inject-conversation")),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while provider.requests().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "provider did not start"
        );
        if let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), output.recv()).await
        {
            acknowledge_kv(&handle, &mut append_seqno, &frame).await;
        }
    }
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_injection()),
        })
        .await
        .unwrap();
    append_seqno += 1;

    let mut protocol_events = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before successful EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(payload.as_ref(), b"{}");
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::InteractionUpdate(update)) = server.message {
            match update.message {
                Some(pb::interaction_update::Message::ContextInjectionState(update)) => {
                    assert_eq!(update.injection_id, "injection-1");
                    match update.state.and_then(|state| state.state) {
                        Some(pb::context_injection_state::State::Queued(_)) => {
                            protocol_events.push("queued")
                        }
                        Some(pb::context_injection_state::State::Delivered(delivered)) => {
                            assert!(!delivered.delivery_batch_id.is_empty());
                            assert!(delivered.delivered_at_ms > 0);
                            protocol_events.push("delivered");
                        }
                        _ => {}
                    }
                }
                Some(pb::interaction_update::Message::UserMessageAppended(update)) => {
                    let user = update.user_message.expect("appended user message");
                    assert_eq!(user.message_id, "injected-user");
                    assert_eq!(user.text, "injected follow-up");
                    protocol_events.push("user_message_appended");
                }
                Some(pb::interaction_update::Message::TextDelta(update))
                    if update.text.contains("continued after injection") =>
                {
                    protocol_events.push("continued_output");
                }
                _ => {}
            }
        }
        acknowledge_kv(&handle, &mut append_seqno, &frame).await;
    }

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let continued_history = serde_json::to_string(&requests[1].history).unwrap();
    assert!(continued_history.contains("injected follow-up"));
    assert_eq!(
        protocol_events,
        [
            "queued",
            "delivered",
            "user_message_appended",
            "continued_output"
        ]
    );
}

#[tokio::test]
async fn injected_user_context_aborts_pending_tools_and_ignores_late_results() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(tool_response("call-1", "Read", "{\"path\":\"/tmp/a\"}"));
    let release = provider.push_gated(text_response("continued after tool interruption"));
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
    let handle = registry
        .get_or_create("interrupt-tool-request")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "interrupt-tool-request",
                "interrupt-tool-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let exec_id = wait_for_exec(&handle, &mut output, &mut append_seqno, "Read").await;
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_injection_for(
                "tool-injection",
                "interrupt-tool-request",
            )),
        })
        .await
        .unwrap();
    append_seqno += 1;

    let mut saw_abort = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while provider.requests().len() < 2 || !saw_abort {
        assert!(
            tokio::time::Instant::now() < deadline,
            "root model did not restart after tool interruption"
        );
        if let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), output.recv()).await
        {
            let (_, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
            let server = pb::AgentServerMessage::decode(payload).unwrap();
            if let Some(pb::agent_server_message::Message::ExecServerControlMessage(control)) =
                server.message
            {
                if let Some(pb::exec_server_control_message::Message::Abort(abort)) =
                    control.message
                {
                    assert_eq!(abort.id, exec_id);
                    saw_abort = true;
                }
            }
            acknowledge_kv(&handle, &mut append_seqno, &frame).await;
        }
    }

    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(read_success(exec_id)),
        })
        .await
        .unwrap();
    append_seqno += 1;
    release.notify_one();

    drain_successfully(&handle, &mut output, &mut append_seqno).await;

    let requests = provider.requests();
    assert_eq!(
        requests[0].history,
        requests[1].history[..requests[0].history.len()]
    );
    let history = serde_json::to_string(&requests[1].history).unwrap();
    let interrupted = history
        .find("Tool execution was interrupted by a newer user message.")
        .expect("interrupted tool result missing from provider history");
    let injected = history
        .find("injected follow-up")
        .expect("injected message missing from provider history");
    assert!(interrupted < injected);
}

/// 回归:被打断的 call_id 只在轮内有效。上一轮的打断不能让这一轮同名调用的结果
/// 被丢弃——那样 tool_round 会一直等一个永远不会被接受的结果(那条等待没有超时)。
#[tokio::test]
async fn a_later_round_may_reuse_an_interrupted_call_id() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(tool_response("call-1", "Read", "{\"path\":\"/tmp/a\"}"));
    // 上游不给调用 id 时引擎按序号合成 call-0/call-1…,跨轮同名是常态。
    provider.push(tool_response("call-1", "Read", "{\"path\":\"/tmp/b\"}"));
    provider.push(text_response("done"));
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
    let handle = registry
        .get_or_create("interrupt-reuse-request")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "interrupt-reuse-request",
                "interrupt-reuse-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let interrupted_exec = wait_for_exec(&handle, &mut output, &mut append_seqno, "Read").await;
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_injection_for(
                "reuse-injection",
                "interrupt-reuse-request",
            )),
        })
        .await
        .unwrap();
    append_seqno += 1;

    // 注入打断第一轮;模型重启后第二轮又请求同一个 call_id,但拿到新的 exec id。
    let reused_exec = wait_for_exec(&handle, &mut output, &mut append_seqno, "Read").await;
    assert_ne!(
        interrupted_exec, reused_exec,
        "第二轮必须是一次新的 exec,才能区分迟到结果与新结果"
    );
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(read_success(reused_exec)),
        })
        .await
        .unwrap();
    append_seqno += 1;

    drain_successfully(&handle, &mut output, &mut append_seqno).await;

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        3,
        "第二轮的工具结果必须被接受并触发第三次模型调用,否则这一轮永远等不到结果"
    );
}

/// 回归(C-03):上一轮已经收过尾的 call_id 被客户端补发到下一轮工具轮时,应当忽略。
///
/// 30 分钟工具轮超时会补一条 interrupted 结果（run/tool_round.rs 的收尾路径），注释明确
/// 预期客户端之后仍会把真正的结果送上来；如果它落在**下一轮**工具轮里，旧实现会让
/// store 报 `unknown tool result call_id`，工具轮失败不经过重试，整轮 run 被判死。
#[tokio::test]
async fn a_late_tool_result_from_an_earlier_round_is_ignored() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(tool_response("call-1", "Read", "{\"path\":\"/tmp/a\"}"));
    provider.push(tool_response("call-2", "Read", "{\"path\":\"/tmp/b\"}"));
    provider.push(text_response("done"));
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
    let handle = registry.get_or_create("late-result-request").await.unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "late-result-request",
                "late-result-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let first_exec = wait_for_exec(&handle, &mut output, &mut append_seqno, "Read").await;
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(read_success(first_exec)),
        })
        .await
        .unwrap();
    append_seqno += 1;

    // 第二轮工具轮开始后，补发上一轮的结果：必须忽略，而不是把本轮判死。
    let second_exec = wait_for_exec(&handle, &mut output, &mut append_seqno, "Read").await;
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(read_success(first_exec)),
        })
        .await
        .unwrap();
    append_seqno += 1;

    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(read_success(second_exec)),
        })
        .await
        .unwrap();
    append_seqno += 1;

    drain_successfully(&handle, &mut output, &mut append_seqno).await;

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        3,
        "迟到的旧结果不能让工具轮失败;本轮结果仍要触发下一次模型调用"
    );
}

#[tokio::test]
async fn injected_user_context_detaches_subagents_without_cancelling_them() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(tool_response(
        "task-call",
        "Task",
        &serde_json::json!({
            "description": "Inspect protocol",
            "prompt": "Inspect the protocol",
            "subagent_type": "generalPurpose",
            "run_in_background": false
        })
        .to_string(),
    ));
    let release = provider.push_gated(text_response("continued while subagent runs"));
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
    let handle = registry
        .get_or_create("detach-subagent-request")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "detach-subagent-request",
                "detach-subagent-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let exec_id = wait_for_exec(&handle, &mut output, &mut append_seqno, "Task").await;
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_injection_for(
                "subagent-injection",
                "detach-subagent-request",
            )),
        })
        .await
        .unwrap();
    append_seqno += 1;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while provider.requests().len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "root model did not restart while subagent remained active"
        );
        if let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), output.recv()).await
        {
            let (_, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
            let server = pb::AgentServerMessage::decode(payload).unwrap();
            if let Some(pb::agent_server_message::Message::ExecServerControlMessage(control)) =
                server.message
            {
                if let Some(pb::exec_server_control_message::Message::Abort(abort)) =
                    control.message
                {
                    assert_ne!(abort.id, exec_id, "Task must not be aborted by injection");
                }
            }
            acknowledge_kv(&handle, &mut append_seqno, &frame).await;
        }
    }

    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(subagent_success(exec_id)),
        })
        .await
        .unwrap();
    append_seqno += 1;
    release.notify_one();

    drain_successfully(&handle, &mut output, &mut append_seqno).await;

    let history = serde_json::to_string(&provider.requests()[1].history).unwrap();
    assert!(history.contains("Tool execution was interrupted by a newer user message."));
    assert!(history.contains("injected follow-up"));
}

#[tokio::test]
async fn injected_user_context_interrupts_automatic_compaction() {
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
            context_window_tokens: Some(100_000),
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        })
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    let seed_answer = "x".repeat(400_000);
    provider.push(text_response(&seed_answer));
    provider.push_pending();
    provider.push(text_response("continued after compacting injection"));
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

    run_to_end(
        &registry,
        "seed-request",
        client_run_for_model(
            "seed-request",
            "compaction-injection-conversation",
            &model.model_hash,
        ),
    )
    .await;

    let handle = registry
        .get_or_create("inject-during-compaction")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    let mut compacting_request = client_run_for_model_with_state(
        "inject-during-compaction",
        "compaction-injection-conversation",
        &model.model_hash,
        None,
    );
    let Some(pb::agent_client_message::Message::RunRequest(request)) =
        compacting_request.message.as_mut()
    else {
        panic!("expected RunRequest")
    };
    request.requested_model.as_mut().unwrap().parameters.push(
        pb::requested_model::ModelParameterValue {
            id: "context".into(),
            value: "100000".into(),
        },
    );
    let Some(pb::conversation_action::Action::UserMessageAction(action)) = request
        .action
        .as_mut()
        .and_then(|action| action.action.as_mut())
    else {
        panic!("expected UserMessageAction")
    };
    action.user_message.as_mut().unwrap().message_id = "compaction-user".into();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(compacting_request),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while provider.requests().len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "automatic compaction did not start"
        );
        if let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), output.recv()).await
        {
            acknowledge_kv(&handle, &mut append_seqno, &frame).await;
        }
    }
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_injection_for(
                "compaction-injection",
                "inject-during-compaction",
            )),
        })
        .await
        .unwrap();
    append_seqno += 1;

    let mut saw_continued = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before successful EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(payload.as_ref(), b"{}");
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::InteractionUpdate(update)) = server.message {
            if let Some(pb::interaction_update::Message::TextDelta(delta)) = update.message {
                saw_continued |= delta.text.contains("continued after compacting injection");
            }
        }
        acknowledge_kv(&handle, &mut append_seqno, &frame).await;
    }

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]
            .prompt
            .instructions
            .starts_with("Summarize this conversation as a complete engineering handover"),
        "second request was not compaction: instructions={:?}, history={:?}",
        requests[1].prompt.instructions,
        requests[1].history
    );
    assert!(!serde_json::to_string(&requests[1].history)
        .unwrap()
        .contains("injected follow-up"));
    assert!(serde_json::to_string(&requests[2].history)
        .unwrap()
        .contains("injected follow-up"));
    assert!(saw_continued);
}

#[tokio::test]
async fn stale_context_injection_is_rejected_without_failing_the_active_run() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    let release = provider.push_gated(vec![
        ModelEvent::Start {
            model_call_id: "active-cycle".into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta("active run completed".into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(FinishReason::Stop),
    ]);
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
    let handle = registry.get_or_create("active-request").await.unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "active-request",
                "stale-injection-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while provider.requests().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "provider did not start"
        );
        if let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), output.recv()).await
        {
            acknowledge_kv(&handle, &mut append_seqno, &frame).await;
        }
    }
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_injection_for("stale-injection", "replaced-request")),
        })
        .await
        .unwrap();
    append_seqno += 1;
    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_injection_for("stale-injection", "replaced-request")),
        })
        .await
        .unwrap();
    append_seqno += 1;

    let mut rejection_count = 0;
    let mut released = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before successful EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(payload.as_ref(), b"{}");
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        let rejected = match server.message {
            Some(pb::agent_server_message::Message::InteractionUpdate(pb::InteractionUpdate {
                message:
                    Some(pb::interaction_update::Message::ContextInjectionState(
                        pb::ContextInjectionStateUpdate {
                            injection_id,
                            state:
                                Some(pb::ContextInjectionState {
                                    state:
                                        Some(pb::context_injection_state::State::Rejected(rejected)),
                                }),
                        },
                    )),
                ..
            })) if injection_id == "stale-injection" => {
                assert_eq!(
                    rejected.reason,
                    "InjectContextAction expected run replaced-request, active run is active-request"
                );
                true
            }
            _ => false,
        };
        acknowledge_kv(&handle, &mut append_seqno, &frame).await;
        if rejected {
            rejection_count += 1;
            if !released {
                released = true;
                release.notify_one();
            }
        }
    }

    assert!(released, "stale injection was not rejected");
    assert_eq!(rejection_count, 1);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn unsupported_runtime_action_is_ignored_without_failing_the_active_run() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    let release = provider.push_gated(text_response("active run completed"));
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
    let handle = registry
        .get_or_create("unsupported-action-request")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "unsupported-action-request",
                "unsupported-action-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while provider.requests().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "provider did not start"
        );
        if let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), output.recv()).await
        {
            acknowledge_kv(&handle, &mut append_seqno, &frame).await;
        }
    }

    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_unsupported_action()),
        })
        .await
        .unwrap();
    append_seqno += 1;
    tokio::task::yield_now().await;
    release.notify_one();

    drain_successfully(&handle, &mut output, &mut append_seqno).await;
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn cancel_subagent_action_aborts_the_target_task_and_keeps_the_parent_running() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "task-cycle".into(),
        },
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: "task-call".into(),
            name: "Task".into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: serde_json::json!({
                "description": "Inspect protocol",
                "prompt": "Inspect the protocol",
                "subagent_type": "generalPurpose",
                "run_in_background": false
            })
            .to_string(),
        },
        ModelEvent::ToolCallEnd { index: 0 },
        ModelEvent::Done(FinishReason::ToolUse),
    ]);
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "continued".into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta("continued after subagent cancellation".into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(FinishReason::Stop),
    ]);
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
    let handle = registry
        .get_or_create("cancel-subagent-request")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "cancel-subagent-request",
                "cancel-subagent-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let exec_id = loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before Task exec");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        assert_eq!(flags & connect::END_STREAM_FLAG, 0);
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                handle
                    .command(TransportCommand::Append {
                        seqno: append_seqno,
                        message: Box::new(kv_ack(kv.id)),
                    })
                    .await
                    .unwrap();
                append_seqno += 1;
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec)) => {
                let Some(pb::exec_server_message::Message::SubagentArgs(args)) = exec.message
                else {
                    continue;
                };
                assert_eq!(args.tool_call_id, "task-call");
                break exec.id;
            }
            _ => {}
        }
    };

    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_cancel_subagent("task-call")),
        })
        .await
        .unwrap();
    append_seqno += 1;

    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before Task abort");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        assert_eq!(flags & connect::END_STREAM_FLAG, 0);
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::ExecServerControlMessage(control)) => {
                let Some(pb::exec_server_control_message::Message::Abort(abort)) = control.message
                else {
                    continue;
                };
                assert_eq!(abort.id, exec_id);
                break;
            }
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                handle
                    .command(TransportCommand::Append {
                        seqno: append_seqno,
                        message: Box::new(kv_ack(kv.id)),
                    })
                    .await
                    .unwrap();
                append_seqno += 1;
            }
            _ => {}
        }
    }

    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(subagent_aborted(exec_id)),
        })
        .await
        .unwrap();
    append_seqno += 1;

    let mut saw_continued = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before successful EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(payload.as_ref(), b"{}");
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                handle
                    .command(TransportCommand::Append {
                        seqno: append_seqno,
                        message: Box::new(kv_ack(kv.id)),
                    })
                    .await
                    .unwrap();
                append_seqno += 1;
            }
            Some(pb::agent_server_message::Message::InteractionUpdate(update)) => {
                if let Some(pb::interaction_update::Message::TextDelta(delta)) = update.message {
                    saw_continued |= delta.text.contains("continued after subagent cancellation");
                }
            }
            _ => {}
        }
    }

    assert!(saw_continued);
    assert_eq!(provider.requests().len(), 2);
}

/// C02：工具轮活跃时，客户端在插话动作里给出被中断调用的真实 shell 结果。服务端必须
/// 用它完成这次调用，而不是再生成一条 generic interrupted 结果把真实输出顶掉。
#[tokio::test]
async fn a_user_interrupt_uses_the_client_supplied_shell_resolution() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(tool_response(
        "shell-interrupt",
        "Shell",
        r#"{"command":"echo actual","block_until_ms":1000}"#,
    ));
    provider.push(text_response("superseded by the follow-up"));
    provider.push(text_response("continued after the resolution"));
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
    let handle = registry
        .get_or_create("resolution-user-request")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "resolution-user-request",
                "resolution-user-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before the shell exec");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        assert_eq!(flags & connect::END_STREAM_FLAG, 0);
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(_)) => {
                acknowledge_kv(&handle, &mut append_seqno, &frame).await;
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec))
                if matches!(
                    exec.message.as_ref(),
                    Some(pb::exec_server_message::Message::ShellStreamArgs(_))
                ) =>
            {
                break;
            }
            _ => {}
        }
    }

    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_user_message_with_shell_resolution(
                "shell-interrupt",
            )),
        })
        .await
        .unwrap();
    append_seqno += 1;

    drain_successfully(&handle, &mut output, &mut append_seqno).await;
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        3,
        "the resolved round runs once, then the queued follow-up interrupts it"
    );
    let final_history = &requests.last().unwrap().history;
    let result = projected_tool_result(final_history, "shell-interrupt")
        .expect("the client-supplied shell result must reach history");
    assert!(
        result.content.contains("client-side shell output"),
        "the actual shell result must be committed, got: {}",
        result.content
    );
    assert!(
        !result
            .content
            .contains("interrupted by a newer user message"),
        "the generic interruption must not replace the actual result: {}",
        result.content
    );
    assert!(!result.is_error);
}

/// C02：CancelAction 携带真实 shell 结果时，先把结果收进历史再取消这一轮；取消仍然
/// 生效（客户端收到 canceled EndStream），但不能把唯一的结果载体丢掉。
#[tokio::test]
async fn a_cancel_action_persists_the_client_supplied_shell_resolution_before_cancelling() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("cursor_server=debug")
        .with_test_writer()
        .try_init();
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(tool_response(
        "shell-cancel",
        "Shell",
        r#"{"command":"echo cancel","block_until_ms":1000}"#,
    ));
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store.clone(),
        Arc::new(provider),
        PromptCompiler::new(assets),
    );
    let handle = registry
        .get_or_create("resolution-cancel-request")
        .await
        .unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "resolution-cancel-request",
                "resolution-cancel-conversation",
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before the shell exec");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        assert_eq!(flags & connect::END_STREAM_FLAG, 0);
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(_)) => {
                acknowledge_kv(&handle, &mut append_seqno, &frame).await;
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec))
                if matches!(
                    exec.message.as_ref(),
                    Some(pb::exec_server_message::Message::ShellStreamArgs(_))
                ) =>
            {
                break;
            }
            _ => {}
        }
    }

    handle
        .command(TransportCommand::Append {
            seqno: append_seqno,
            message: Box::new(runtime_cancel_action_with_shell_resolution("shell-cancel")),
        })
        .await
        .unwrap();
    append_seqno += 1;

    // 先看到取消收尾，再检查历史：结果必须在取消之前提交。
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before canceled EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(json["error"]["code"], "canceled");
            break;
        }
        acknowledge_kv(&handle, &mut append_seqno, &frame).await;
    }

    let messages = store
        .load_current_messages(&ConversationId::new("resolution-cancel-conversation"))
        .await
        .unwrap();
    let result = messages
        .iter()
        .find_map(|message| match &message.content {
            MessageContent::ToolResult(result) if result.call_id == "shell-cancel" => Some(result),
            _ => None,
        })
        .expect("the cancelled run must keep the client-supplied shell result");
    assert!(
        result.content.contains("client-side cancel output"),
        "got: {}",
        result.content
    );
    assert!(!result.is_error);
}

/// C02：上一轮被取消、checkpoint 里留下 pending 工具调用时，新用户动作携带的真实
/// shell / Task 结果必须被收进历史（包括 Task 的 agent 关联），而不是被 Start 静默清掉。
#[tokio::test]
async fn a_new_user_action_resolves_pending_tool_calls_from_checkpoint() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(parallel_tool_response(&[
        (
            "pending-shell",
            "Shell",
            r#"{"command":"echo pending","block_until_ms":1000}"#,
        ),
        (
            "pending-task",
            "Task",
            r#"{"description":"pending task","prompt":"do work","subagent_type":"generalPurpose","run_in_background":true}"#,
        ),
    ]));
    provider.push(text_response("resumed after resolving the pending calls"));
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

    let first = registry
        .get_or_create("pending-resolution-first")
        .await
        .unwrap();
    let mut first_output = first.subscribe();
    first
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "pending-resolution-first",
                "pending-resolution-conversation",
            )),
        })
        .await
        .unwrap();
    let mut first_seqno = 1;
    let staged = loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), first_output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before the staged checkpoint");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            panic!("the first run ended before the pending checkpoint")
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) =
            &server.message
        {
            if state.pending_tool_calls.len() == 1 {
                break state.clone();
            }
        }
        acknowledge_kv(&first, &mut first_seqno, &frame).await;
    };
    first
        .command(TransportCommand::Append {
            seqno: first_seqno,
            message: Box::new(runtime_cancel_action()),
        })
        .await
        .unwrap();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), first_output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before canceled EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(json["error"]["code"], "canceled");
            break;
        }
    }

    let resumed = registry
        .get_or_create("pending-resolution-second")
        .await
        .unwrap();
    let mut resumed_output = resumed.subscribe();
    resumed
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_with_pending_resolutions(
                "pending-resolution-second",
                "pending-resolution-conversation",
                staged,
            )),
        })
        .await
        .unwrap();
    let mut resumed_seqno = 1;
    let mut settled = None;
    loop {
        let frame =
            match tokio::time::timeout(std::time::Duration::from_secs(5), resumed_output.recv())
                .await
            {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => panic!("resumed run did not finish"),
            };
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(payload.as_ref(), b"{}");
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) =
            &server.message
        {
            if state.pending_tool_calls.is_empty() {
                settled = Some(state.clone());
            }
        }
        acknowledge_kv(&resumed, &mut resumed_seqno, &frame).await;
    }

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "the new action must continue with a model call"
    );
    let shell = projected_tool_result(&requests[1].history, "pending-shell")
        .expect("the pending shell result must reach history");
    assert!(
        shell.content.contains("client-side pending shell output"),
        "got: {}",
        shell.content
    );
    let task = projected_tool_result(&requests[1].history, "pending-task")
        .expect("the pending Task result must reach history");
    assert!(
        task.content.contains("resolved-pending-agent"),
        "{}",
        task.content
    );
    assert!(!task.is_error, "the resolved Task must stay a success");

    let settled = settled.expect("the resolved run must publish a settled checkpoint");
    let run = settled
        .subagent_runs_by_parent_tool_call_id
        .get("pending-task")
        .expect("the resolved Task must keep its parent-tool-call association");
    assert_eq!(run.subagent_id.as_deref(), Some("resolved-pending-agent"));
    assert_eq!(
        run.status,
        pb::SubagentRunStatus::Backgrounded as i32,
        "a resolved background Task must not be turned into an abort"
    );
}

/// C02/F02：活跃工具轮里通过 UserMessageAction 的 Task resolution 收尾 Task 后，
/// 同一 transport 立即按 agent id 取消必须命中当前 generation 的关联，而不是等下一次
/// RunRequest 的 checkpoint 种子。
#[tokio::test]
async fn an_active_task_resolution_updates_the_current_transport_agent_map_for_cancel() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("cursor_server=debug")
        .with_test_writer()
        .try_init();
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push_for(
        "active-resolution-conversation",
        tool_response(
            "active-resolution-task",
            "Task",
            r#"{"description":"active task","prompt":"do work","subagent_type":"generalPurpose","run_in_background":true}"#,
        ),
    );
    let _hold_parent = provider.push_gated_for(
        "active-resolution-conversation",
        text_response("parent stays active"),
    );
    let _hold_child = provider.push_gated_for(
        "active-resolution-child",
        text_response("child stays active"),
    );
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

    let parent = registry
        .get_or_create("active-resolution-parent")
        .await
        .unwrap();
    let mut parent_output = parent.subscribe();
    parent
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "active-resolution-parent",
                "active-resolution-conversation",
            )),
        })
        .await
        .unwrap();
    let mut parent_seqno = 1;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), parent_output.recv())
            .await
            .expect("parent did not dispatch the Task")
            .expect("parent stream closed");
        let Some(server) = frame_role(&frame) else {
            panic!("parent ended before the Task exec");
        };
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                acknowledge_kv(&parent, &mut parent_seqno, &frame).await;
                let _ = kv;
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec)) => {
                let Some(pb::exec_server_message::Message::SubagentArgs(args)) = exec.message
                else {
                    continue;
                };
                assert_eq!(args.tool_call_id, "active-resolution-task");
                let child = registry
                    .get_or_create("active-resolution-child-request")
                    .await
                    .unwrap();
                child
                    .set_parent(TransportParent {
                        request_id: Some("active-resolution-parent".into()),
                        tool_call_id: Some("active-resolution-task".into()),
                    })
                    .unwrap();
                child
                    .command(TransportCommand::Append {
                        seqno: 0,
                        message: Box::new(subagent_run_request(
                            "active-resolution-child-request",
                            "active-resolution-child",
                        )),
                    })
                    .await
                    .unwrap();
                let child_handle = child.clone();
                let mut child_output = child.subscribe();
                tokio::spawn(async move {
                    let mut seqno = 1;
                    while let Some(frame) = child_output.recv().await {
                        let Some(server) = frame_role(&frame) else {
                            break;
                        };
                        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) =
                            server.message
                        {
                            let _ = child_handle
                                .command(TransportCommand::Append {
                                    seqno,
                                    message: Box::new(kv_ack(kv.id)),
                                })
                                .await;
                            seqno += 1;
                        }
                    }
                });
                break;
            }
            _ => {}
        }
    }
    // 用 Task resolution 收尾活跃调用；结果会走正常 tool round 提交。
    parent
        .command(TransportCommand::Append {
            seqno: parent_seqno,
            message: Box::new(runtime_user_message_with_task_resolution(
                "active-resolution-task",
                "active-resolution-child",
            )),
        })
        .await
        .unwrap();
    wait_for_tool_call_status(&store, "active-resolution-task", "completed").await;
    wait_for_run_status(&store, "active-resolution-child-request", "running").await;

    parent
        .command(TransportCommand::Append {
            seqno: parent_seqno + 1,
            message: Box::new(runtime_cancel_subagent("active-resolution-child")),
        })
        .await
        .unwrap();
    wait_for_run_status(&store, "active-resolution-child-request", "cancelled").await;
    registry.shutdown().await;
}

/// C02/F02：旧 run 留下的 pending 工具轮由新动作补齐真实结果时，必须同时收尾原
/// `tool_round_calls`（background_task/call 候选依赖 completed）、更新当前 transport 的
/// agent→call 映射，并让真实 child 的完成唤醒与 agent-id 取消都成立。
#[tokio::test]
async fn a_pending_task_resolution_completes_the_old_round_and_supports_wake_and_cancel() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("cursor_server=debug")
        .with_test_writer()
        .try_init();
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push_for(
        "pending-round-conversation",
        parallel_tool_response(&[
            (
                "pending-shell",
                "Shell",
                r#"{"command":"echo pending","block_until_ms":1000}"#,
            ),
            (
                "pending-wake-task",
                "Task",
                r#"{"description":"wake task","prompt":"do work","subagent_type":"generalPurpose","run_in_background":true}"#,
            ),
            (
                "pending-cancel-task",
                "Task",
                r#"{"description":"cancel task","prompt":"do work","subagent_type":"generalPurpose","run_in_background":true}"#,
            ),
        ]),
    );
    let release_wake = provider.push_gated_for(
        "pending-wake-child",
        text_response("pending wake child finished"),
    );
    let _hold_cancel = provider.push_gated_for(
        "pending-cancel-child",
        text_response("pending cancel child stays active"),
    );
    provider.push_for(
        "pending-round-conversation",
        text_response("resumed after resolving the pending calls"),
    );
    provider.push_for(
        "pending-round-conversation",
        text_response("after the wake"),
    );
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

    let first = registry.get_or_create("pending-round-first").await.unwrap();
    let mut first_output = first.subscribe();
    first
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "pending-round-first",
                "pending-round-conversation",
            )),
        })
        .await
        .unwrap();
    let mut first_seqno = 1;
    let mut staged = None;
    let mut started_children = 0;
    while staged.is_none() || started_children < 2 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), first_output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before the staged checkpoint");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            panic!("the first run ended before the pending checkpoint")
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) =
            &server.message
        {
            if state.pending_tool_calls.len() == 1 {
                staged = Some(state.clone());
            }
        }
        if let Some(pb::agent_server_message::Message::ExecServerMessage(exec)) = &server.message {
            let Some(pb::exec_server_message::Message::SubagentArgs(args)) = &exec.message else {
                continue;
            };
            let (child_conversation, child_request) = match args.tool_call_id.as_str() {
                "pending-wake-task" => ("pending-wake-child", "pending-wake-child-request"),
                "pending-cancel-task" => ("pending-cancel-child", "pending-cancel-child-request"),
                _ => continue,
            };
            let child = registry.get_or_create(child_request).await.unwrap();
            child
                .set_parent(TransportParent {
                    request_id: Some("pending-round-first".into()),
                    tool_call_id: Some(args.tool_call_id.clone()),
                })
                .unwrap();
            child
                .command(TransportCommand::Append {
                    seqno: 0,
                    message: Box::new(subagent_run_request(child_request, child_conversation)),
                })
                .await
                .unwrap();
            let child_handle = child.clone();
            let mut child_output = child.subscribe();
            tokio::spawn(async move {
                let mut seqno = 1;
                while let Some(frame) = child_output.recv().await {
                    let Some(server) = frame_role(&frame) else {
                        break;
                    };
                    if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) =
                        server.message
                    {
                        let _ = child_handle
                            .command(TransportCommand::Append {
                                seqno,
                                message: Box::new(kv_ack(kv.id)),
                            })
                            .await;
                        seqno += 1;
                    }
                }
            });
            started_children += 1;
        }
        acknowledge_kv(&first, &mut first_seqno, &frame).await;
    }
    let staged = staged.expect("the first run did not publish a pending checkpoint");
    // 子 run 必须在取消父轮之前真正 active：后续的 agent-id 取消按“当前 running run”
    // 绑定目标，子 run 还没 claim 时只能安全拒绝。
    wait_for_run_status(&store, "pending-wake-child-request", "running").await;
    wait_for_run_status(&store, "pending-cancel-child-request", "running").await;
    first
        .command(TransportCommand::Append {
            seqno: first_seqno,
            message: Box::new(runtime_cancel_action()),
        })
        .await
        .unwrap();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), first_output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before canceled EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(json["error"]["code"], "canceled");
            break;
        }
    }

    let resumed = registry
        .get_or_create("pending-round-second")
        .await
        .unwrap();
    let mut resumed_output = resumed.subscribe();
    resumed
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_with_pending_task_resolutions(
                "pending-round-second",
                "pending-round-conversation",
                staged,
            )),
        })
        .await
        .unwrap();
    let mut resumed_seqno = 1;
    loop {
        let frame =
            match tokio::time::timeout(std::time::Duration::from_secs(5), resumed_output.recv())
                .await
            {
                Ok(Some(frame)) => frame,
                Ok(None) => panic!("resumed transport closed before turnEnded"),
                Err(_) => panic!("resumed run did not finish"),
            };
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        assert_eq!(
            flags & connect::END_STREAM_FLAG,
            0,
            "resumed run closed before turnEnded"
        );
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        let ended = matches!(
            server.message,
            Some(pb::agent_server_message::Message::InteractionUpdate(
                pb::InteractionUpdate {
                    message: Some(pb::interaction_update::Message::TurnEnded(_)),
                }
            ))
        );
        acknowledge_kv(&resumed, &mut resumed_seqno, &frame).await;
        if ended {
            break;
        }
    }

    // F2（pending）：解析出的 Task 身份必须进入当前 transport 的 map；立刻按 agent id 取消。
    resumed
        .command(TransportCommand::Append {
            seqno: resumed_seqno,
            message: Box::new(runtime_cancel_subagent("pending-cancel-child")),
        })
        .await
        .unwrap();

    // F1：旧 round 的三个调用都必须由 resolution 收敛为 completed，而不是继续 pending。
    let calls: Vec<(String, String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT call_id, status, result_content, result_is_error FROM tool_round_calls
          WHERE call_id IN ('pending-shell','pending-wake-task','pending-cancel-task')
          ORDER BY call_id",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(calls.len(), 3);
    for (call_id, status, content, is_error) in &calls {
        assert_eq!(status, "completed", "{call_id} must be completed");
        assert_eq!(*is_error, Some(0), "{call_id} must keep success");
        let content = content.as_deref().unwrap_or_default();
        match call_id.as_str() {
            "pending-shell" => assert!(content.contains("client-side pending shell output")),
            "pending-wake-task" => assert!(content.contains("pending-wake-child")),
            "pending-cancel-task" => assert!(content.contains("pending-cancel-child")),
            other => panic!("unexpected call {other}"),
        }
    }

    wait_for_run_status(&store, "pending-cancel-child-request", "running").await;
    wait_for_run_status(&store, "pending-cancel-child-request", "cancelled").await;

    // F1 的后台唤醒：释放 wake child，让 background_task 通过已 completed 的旧 call。
    wait_for_run_status(&store, "pending-wake-child-request", "running").await;
    release_wake.notify_one();
    wait_for_run_status(&store, "pending-wake-child-request", "completed").await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let third = registry.get_or_create("pending-round-third").await.unwrap();
    let mut third_output = third.subscribe();
    third
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run_for(
                "pending-round-third",
                "pending-round-conversation",
            )),
        })
        .await
        .unwrap();
    let mut third_seqno = 1;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), third_output.recv())
            .await
            .unwrap()
            .expect("third run closed before EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(payload.as_ref(), b"{}");
            break;
        }
        acknowledge_kv(&third, &mut third_seqno, &frame).await;
    }
    let history = serde_json::to_string(
        &provider
            .requests()
            .last()
            .expect("third run did not call the model")
            .history,
    )
    .unwrap();
    assert!(
        history.contains("pending-wake-child") && history.contains("pending-wake-task"),
        "the completed Task must wake the parent with its child/call identity: {history}"
    );
    registry.shutdown().await;
}

fn client_run() -> pb::AgentClientMessage {
    client_run_for("cancel-request", "cancel-conversation")
}

fn client_run_for(request_id: &str, conversation_id: &str) -> pb::AgentClientMessage {
    client_run_for_model(request_id, conversation_id, "test-model")
}

fn client_run_for_model(
    request_id: &str,
    conversation_id: &str,
    model_id: &str,
) -> pb::AgentClientMessage {
    client_run_for_model_with_state(request_id, conversation_id, model_id, None)
}

fn client_run_for_model_with_state(
    request_id: &str,
    conversation_id: &str,
    model_id: &str,
    state: Option<pb::ConversationStateStructure>,
) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(pb::conversation_action::Action::UserMessageAction(
                        pb::UserMessageAction {
                            user_message: Some(pb::UserMessage {
                                text: "read".into(),
                                message_id: "cancel-user".into(),
                                mode: pb::AgentMode::Agent as i32,
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }),
                conversation_id: Some(conversation_id.into()),
                run_id: Some(request_id.into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: model_id.into(),
                    ..Default::default()
                }),
                conversation_state: state,
                ..Default::default()
            },
        )),
    }
}

fn text_response(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: format!("call-{text}"),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta(text.into()),
        ModelEvent::TextEnd,
        ModelEvent::Usage(Usage {
            input_tokens: Some(1),
            context_input_tokens: Some(1),
            output_tokens: Some(1),
            total_tokens: Some(2),
            ..Default::default()
        }),
        ModelEvent::Done(FinishReason::Stop),
    ]
}

fn tool_response(call_id: &str, name: &str, arguments: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: format!("call-{call_id}"),
        },
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: call_id.into(),
            name: name.into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: arguments.into(),
        },
        ModelEvent::ToolCallEnd { index: 0 },
        ModelEvent::Done(FinishReason::ToolUse),
    ]
}

/// 同一轮里的并行调用，索引必须分开，否则第二轮会被当成重复的同一调用。
fn parallel_tool_response(calls: &[(&str, &str, &str)]) -> Vec<ModelEvent> {
    let mut events = vec![ModelEvent::Start {
        model_call_id: "parallel-tool-call".into(),
    }];
    for (index, (call_id, name, arguments)) in calls.iter().enumerate() {
        events.extend([
            ModelEvent::ToolCallStart {
                index,
                call_id: (*call_id).into(),
                name: (*name).into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index,
                delta: (*arguments).into(),
            },
            ModelEvent::ToolCallEnd { index },
        ]);
    }
    events.push(ModelEvent::Done(FinishReason::ToolUse));
    events
}

async fn wait_for_exec(
    handle: &cursor_server::cursor::TransportHandle,
    output: &mut tokio::sync::mpsc::Receiver<Bytes>,
    append_seqno: &mut i64,
    tool: &str,
) -> u32 {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before Exec");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        assert_eq!(flags & connect::END_STREAM_FLAG, 0);
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::ExecServerMessage(exec)) = server.message {
            let matches = match exec.message.as_ref() {
                Some(pb::exec_server_message::Message::ReadArgs(_)) => tool == "Read",
                Some(pb::exec_server_message::Message::SubagentArgs(_)) => tool == "Task",
                _ => false,
            };
            if matches {
                return exec.id;
            }
        }
        acknowledge_kv(handle, append_seqno, &frame).await;
    }
}

async fn drain_successfully(
    handle: &cursor_server::cursor::TransportHandle,
    output: &mut tokio::sync::mpsc::Receiver<Bytes>,
    append_seqno: &mut i64,
) {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before successful EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(payload.as_ref(), b"{}");
            return;
        }
        acknowledge_kv(handle, append_seqno, &frame).await;
    }
}

fn read_success(id: u32) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id,
                message: Some(pb::exec_client_message::Message::ReadResult(
                    pb::ReadResult {
                        result: Some(pb::read_result::Result::Success(pb::ReadSuccess {
                            path: "/tmp/a".into(),
                            total_lines: 1,
                            file_size: 1,
                            output: Some(pb::read_success::Output::Content("late".into())),
                            ..Default::default()
                        })),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

fn subagent_success(id: u32) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id,
                message: Some(pb::exec_client_message::Message::SubagentResult(
                    pb::SubagentResult {
                        result: Some(pb::subagent_result::Result::Success(pb::SubagentSuccess {
                            agent_id: "detached-child".into(),
                            ..Default::default()
                        })),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

async fn run_to_end(
    registry: &TransportRegistry,
    request_id: &str,
    request: pb::AgentClientMessage,
) -> pb::ConversationStateStructure {
    let handle = registry.get_or_create(request_id).await.unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(request),
        })
        .await
        .unwrap();
    let mut append_seqno = 1;
    let mut state = None;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before EndStream");
        let (flags, _) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            return state.expect("Run ended without a checkpoint");
        }
        let (_, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(update)) =
            server.message
        {
            state = Some(update);
        }
        acknowledge_kv(&handle, &mut append_seqno, &frame).await;
    }
}

async fn wait_for_turn_ended(
    handle: &cursor_server::cursor::TransportHandle,
    output: &mut tokio::sync::mpsc::Receiver<Bytes>,
    append_seqno: &mut i64,
) {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before turnEnded");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        assert_eq!(flags & connect::END_STREAM_FLAG, 0);
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        let turn_ended = matches!(
            server.message,
            Some(pb::agent_server_message::Message::InteractionUpdate(
                pb::InteractionUpdate {
                    message: Some(pb::interaction_update::Message::TurnEnded(_)),
                }
            ))
        );
        acknowledge_kv(handle, append_seqno, &frame).await;
        if turn_ended {
            return;
        }
    }
}

async fn assert_transport_remains_open(
    handle: &cursor_server::cursor::TransportHandle,
    output: &mut tokio::sync::mpsc::Receiver<Bytes>,
    append_seqno: &mut i64,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(frame)) = tokio::time::timeout(remaining, output.recv()).await else {
            return;
        };
        let (flags, _) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        assert_eq!(
            flags & connect::END_STREAM_FLAG,
            0,
            "turnEnded closed the transport before the queued action arrived"
        );
        acknowledge_kv(handle, append_seqno, &frame).await;
    }
}

async fn acknowledge_kv(
    handle: &cursor_server::cursor::TransportHandle,
    append_seqno: &mut i64,
    frame: &[u8],
) {
    let (flags, payload) = connect::decode_frames(frame).unwrap().pop().unwrap();
    if flags & connect::END_STREAM_FLAG != 0 {
        return;
    }
    let server = pb::AgentServerMessage::decode(payload).unwrap();
    if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = server.message {
        handle
            .command(TransportCommand::Append {
                seqno: *append_seqno,
                message: Box::new(kv_ack(kv.id)),
            })
            .await
            .unwrap();
        *append_seqno += 1;
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

fn runtime_cancel_action() -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ConversationAction(
            pb::ConversationAction {
                action: Some(pb::conversation_action::Action::CancelAction(
                    pb::CancelAction::default(),
                )),
                ..Default::default()
            },
        )),
    }
}

fn runtime_unsupported_action() -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ConversationAction(
            pb::ConversationAction {
                action: Some(pb::conversation_action::Action::ResumeAction(
                    pb::ResumeAction::default(),
                )),
                ..Default::default()
            },
        )),
    }
}

fn runtime_user_message() -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ConversationAction(
            pb::ConversationAction {
                action: Some(pb::conversation_action::Action::UserMessageAction(
                    pb::UserMessageAction {
                        user_message: Some(pb::UserMessage {
                            text: "queued follow-up".into(),
                            message_id: "queued-user".into(),
                            mode: pb::AgentMode::Agent as i32,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

fn runtime_injection() -> pb::AgentClientMessage {
    runtime_injection_for("injection-1", "inject-request")
}

fn runtime_injection_for(injection_id: &str, expected_run_id: &str) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ConversationAction(
            pb::ConversationAction {
                action: Some(pb::conversation_action::Action::InjectContextAction(
                    pb::InjectContextAction {
                        injection_id: injection_id.into(),
                        expected_run_id: expected_run_id.into(),
                        payload: Some(pb::inject_context_action::Payload::UserContext(
                            pb::UserContextInjection {
                                user_message: Some(pb::UserMessage {
                                    text: "injected follow-up".into(),
                                    message_id: "injected-user".into(),
                                    ..Default::default()
                                }),
                                request_context: Some(Default::default()),
                            },
                        )),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

fn runtime_cancel_subagent(tool_call_id: &str) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ConversationAction(
            pb::ConversationAction {
                action: Some(pb::conversation_action::Action::CancelSubagentAction(
                    pb::CancelSubagentAction {
                        subagent_id: tool_call_id.into(),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

fn shell_resolution(call_id: &str, stdout: &str) -> pb::InterruptedPendingToolCallResolution {
    pb::InterruptedPendingToolCallResolution {
        tool_call_id: call_id.into(),
        resolution: Some(
            pb::interrupted_pending_tool_call_resolution::Resolution::ShellResult(
                pb::ShellResult {
                    result: Some(pb::shell_result::Result::Success(pb::ShellSuccess {
                        stdout: stdout.into(),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            ),
        ),
    }
}

fn task_resolution(call_id: &str, agent_id: &str) -> pb::InterruptedPendingToolCallResolution {
    pb::InterruptedPendingToolCallResolution {
        tool_call_id: call_id.into(),
        resolution: Some(
            pb::interrupted_pending_tool_call_resolution::Resolution::TaskResult(pb::TaskResult {
                result: Some(pb::task_result::Result::Success(pb::TaskSuccess {
                    agent_id: Some(agent_id.into()),
                    is_background: true,
                    result_suffix: Some("resolved task output".into()),
                    background_reason: pb::SubagentBackgroundReason::AgentRequest as i32,
                    ..Default::default()
                })),
            }),
        ),
    }
}

fn runtime_user_message_with_shell_resolution(call_id: &str) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ConversationAction(
            pb::ConversationAction {
                action: Some(pb::conversation_action::Action::UserMessageAction(
                    pb::UserMessageAction {
                        user_message: Some(pb::UserMessage {
                            text: "continue".into(),
                            message_id: "resolution-user".into(),
                            mode: pb::AgentMode::Agent as i32,
                            ..Default::default()
                        }),
                        interrupted_pending_tool_call_resolutions: Some(
                            pb::InterruptedPendingToolCallResolutions {
                                resolutions: vec![shell_resolution(
                                    call_id,
                                    "client-side shell output",
                                )],
                            },
                        ),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

fn runtime_cancel_action_with_shell_resolution(call_id: &str) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ConversationAction(
            pb::ConversationAction {
                action: Some(pb::conversation_action::Action::CancelAction(
                    pb::CancelAction {
                        reason: "user cancelled".into(),
                        interrupted_pending_tool_call_resolutions: Some(
                            pb::InterruptedPendingToolCallResolutions {
                                resolutions: vec![shell_resolution(
                                    call_id,
                                    "client-side cancel output",
                                )],
                            },
                        ),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

fn client_run_with_pending_resolutions(
    request_id: &str,
    conversation_id: &str,
    state: pb::ConversationStateStructure,
) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(pb::conversation_action::Action::UserMessageAction(
                        pb::UserMessageAction {
                            user_message: Some(pb::UserMessage {
                                text: "continue after the interruption".into(),
                                message_id: "pending-resolution-user".into(),
                                mode: pb::AgentMode::Agent as i32,
                                ..Default::default()
                            }),
                            interrupted_pending_tool_call_resolutions: Some(
                                pb::InterruptedPendingToolCallResolutions {
                                    resolutions: vec![
                                        shell_resolution(
                                            "pending-shell",
                                            "client-side pending shell output",
                                        ),
                                        task_resolution("pending-task", "resolved-pending-agent"),
                                    ],
                                },
                            ),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }),
                conversation_id: Some(conversation_id.into()),
                conversation_state: Some(state),
                run_id: Some(request_id.into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "test-model".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )),
    }
}

fn client_run_with_pending_task_resolutions(
    request_id: &str,
    conversation_id: &str,
    state: pb::ConversationStateStructure,
) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(pb::conversation_action::Action::UserMessageAction(
                        pb::UserMessageAction {
                            user_message: Some(pb::UserMessage {
                                text: "continue after the interruption".into(),
                                message_id: "pending-task-resolution-user".into(),
                                mode: pb::AgentMode::Agent as i32,
                                ..Default::default()
                            }),
                            interrupted_pending_tool_call_resolutions: Some(
                                pb::InterruptedPendingToolCallResolutions {
                                    resolutions: vec![
                                        shell_resolution(
                                            "pending-shell",
                                            "client-side pending shell output",
                                        ),
                                        task_resolution("pending-wake-task", "pending-wake-child"),
                                        task_resolution(
                                            "pending-cancel-task",
                                            "pending-cancel-child",
                                        ),
                                    ],
                                },
                            ),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }),
                conversation_id: Some(conversation_id.into()),
                conversation_state: Some(state),
                run_id: Some(request_id.into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "test-model".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )),
    }
}

fn runtime_user_message_with_task_resolution(
    call_id: &str,
    agent_id: &str,
) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ConversationAction(
            pb::ConversationAction {
                action: Some(pb::conversation_action::Action::UserMessageAction(
                    pb::UserMessageAction {
                        user_message: Some(pb::UserMessage {
                            text: "continue".into(),
                            message_id: "task-resolution-user".into(),
                            mode: pb::AgentMode::Agent as i32,
                            ..Default::default()
                        }),
                        interrupted_pending_tool_call_resolutions: Some(
                            pb::InterruptedPendingToolCallResolutions {
                                resolutions: vec![task_resolution(call_id, agent_id)],
                            },
                        ),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

fn subagent_run_request(request_id: &str, conversation_id: &str) -> pb::AgentClientMessage {
    let mut request = client_run_for(request_id, conversation_id);
    if let Some(pb::agent_client_message::Message::RunRequest(run)) = &mut request.message {
        run.subagent_type_name = Some("generalPurpose".into());
    }
    request
}

async fn wait_for_tool_call_status(
    store: &cursor_server::store::Store,
    call_id: &str,
    expected: &str,
) {
    for _ in 0..400 {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM tool_round_calls WHERE call_id = ?")
                .bind(call_id)
                .fetch_optional(store.pool())
                .await
                .unwrap();
        if status.as_deref() == Some(expected) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("tool call {call_id} did not become {expected}");
}

async fn wait_for_run_status(
    store: &cursor_server::store::Store,
    request_id: &str,
    expected: &str,
) {
    for _ in 0..400 {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM runs WHERE cursor_request_id = ?")
                .bind(request_id)
                .fetch_optional(store.pool())
                .await
                .unwrap();
        if status.as_deref() == Some(expected) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("run {request_id} did not become {expected}");
}

fn frame_role(frame: &[u8]) -> Option<pb::AgentServerMessage> {
    let (flags, payload) = connect::decode_frames(frame).unwrap().pop().unwrap();
    if flags & connect::END_STREAM_FLAG != 0 {
        return None;
    }
    Some(pb::AgentServerMessage::decode(payload).unwrap())
}

fn projected_tool_result<'a>(
    history: &'a [ProjectedMessage],
    call_id: &str,
) -> Option<&'a ToolResultContent> {
    history.iter().find_map(|message| match &message.content {
        ProjectedContent::ToolResult(result) if result.call_id == call_id => Some(result),
        _ => None,
    })
}

fn subagent_aborted(id: u32) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id,
                message: Some(pb::exec_client_message::Message::SubagentResult(
                    pb::SubagentResult {
                        result: Some(pb::subagent_result::Result::Error(pb::SubagentError {
                            agent_id: None,
                            error: "Subagent was aborted by the user".into(),
                        })),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}
