//! Verifies that a background subagent wakes its parent turn.
#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::sync::Arc;

use cursor_server::{
    cursor::{
        prompting::{PromptAssets, PromptCompiler},
        protocol::connect,
        protocol::proto::agent::v1 as pb,
        TransportCommand, TransportParent, TransportRegistry,
    },
    model::{ContentPart, MessageContent, ProjectedContent},
    provider::{FinishReason, ModelEvent},
};
use prost::Message;

const PARENT_CONVERSATION: &str = "wakeup-parent-conversation";
const CHILD_CONVERSATION: &str = "wakeup-child-conversation";
const TASK_CALL: &str = "call-task";

/// 父轮第一轮：派一个后台子代理。
fn task_call_events() -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: "parent-call-1".into(),
        },
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: TASK_CALL.into(),
            name: "Task".into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: format!(
                r#"{{"description":"Analyze game logic","prompt":"Inspect the game","run_in_background":true,"subagent_type":"generalPurpose","tool_call_id":"{TASK_CALL}"}}"#
            ),
        },
        ModelEvent::ToolCallEnd { index: 0 },
        ModelEvent::Done(FinishReason::ToolUse),
    ]
}

fn text_events(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: "call".into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta(text.into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(FinishReason::Stop),
    ]
}

fn run_request(
    conversation_id: &str,
    request_id: &str,
    message_id: &str,
    subagent: bool,
) -> pb::AgentClientMessage {
    let user = pb::UserMessage {
        text: "go".into(),
        message_id: message_id.into(),
        mode: pb::AgentMode::Agent as i32,
        ..Default::default()
    };
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(pb::conversation_action::Action::UserMessageAction(
                        pb::UserMessageAction {
                            user_message: Some(user),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }),
                conversation_id: Some(conversation_id.into()),
                run_id: Some(request_id.into()),
                subagent_type_name: subagent.then(|| "generalPurpose".to_owned()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "test-model".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )),
    }
}

/// 后台子代理的 Task 结果：只回"已后台化"，最终答复由子代理会话自己跑。
fn task_result(exec_id: u32) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id: exec_id,
                message: Some(pb::exec_client_message::Message::SubagentResult(
                    pb::SubagentResult {
                        result: Some(pb::subagent_result::Result::Success(pb::SubagentSuccess {
                            agent_id: CHILD_CONVERSATION.into(),
                            background_reason: pb::SubagentBackgroundReason::AgentRequest as i32,
                            ..Default::default()
                        })),
                    },
                )),
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

fn frame_role(frame: &[u8]) -> Option<pb::AgentServerMessage> {
    let (flags, payload) = connect::decode_frames(frame).unwrap().pop().unwrap();
    if flags & connect::END_STREAM_FLAG != 0 {
        return None;
    }
    Some(pb::AgentServerMessage::decode(payload).unwrap())
}

#[tokio::test]
async fn a_background_subagent_wakes_the_parent_turn() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("cursor_server=debug")
        .with_test_writer()
        .try_init();
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    // 按会话分发，顺序才确定：父轮先派子代理、再说一句收尾话进入等待；
    // 子代理那一次挂住，等父轮真的停下来之后再放行；放行后父轮被唤醒续写。
    provider.push_for(PARENT_CONVERSATION, task_call_events());
    provider.push_for(PARENT_CONVERSATION, text_events("waiting for the subagent"));
    let finish_child = provider.push_gated_for(CHILD_CONVERSATION, text_events("child finished"));
    provider.push_for(PARENT_CONVERSATION, text_events("followed up"));
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

    let parent = registry.get_or_create("parent-request").await.unwrap();
    let mut parent_output = parent.subscribe();
    parent
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                PARENT_CONVERSATION,
                "parent-request",
                "parent-user",
                false,
            )),
        })
        .await
        .unwrap();

    // 父轮的帧循环：只在收到 Task 的 exec 请求时动手——先把子代理跑起来（此刻父轮
    // 还在运行，血缘才查得到父运行），再把"已后台化"的结果回给父轮。
    let mut parent_seqno = 1;
    let mut child = None;
    while child.is_none() {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), parent_output.recv())
            .await
            .expect("父轮没有派出后台子代理")
            .expect("父轮的流提前关闭");
        let Some(server) = frame_role(&frame) else {
            panic!("父轮在子代理结果回来之前就收尾了");
        };
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                parent
                    .command(TransportCommand::Append {
                        seqno: parent_seqno,
                        message: Box::new(kv_ack(kv.id)),
                    })
                    .await
                    .unwrap();
                parent_seqno += 1;
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec)) => {
                let handle = registry.get_or_create("child-request").await.unwrap();
                handle
                    .set_parent(TransportParent {
                        request_id: Some("parent-request".into()),
                        tool_call_id: Some(TASK_CALL.into()),
                    })
                    .unwrap();
                handle
                    .command(TransportCommand::Append {
                        seqno: 0,
                        message: Box::new(run_request(
                            CHILD_CONVERSATION,
                            "child-request",
                            "child-user",
                            true,
                        )),
                    })
                    .await
                    .unwrap();
                child = Some(handle.clone());
                // 子代理那条流也得有人读：没人读它就会堵在输出上，永远收不了尾。
                let child_output = handle.subscribe();
                tokio::spawn(async move {
                    let mut child_output = child_output;
                    let mut seqno = 1;
                    while let Some(frame) = child_output.recv().await {
                        let Some(server) = frame_role(&frame) else {
                            break;
                        };
                        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) =
                            server.message
                        {
                            let _ = handle
                                .command(TransportCommand::Append {
                                    seqno,
                                    message: Box::new(kv_ack(kv.id)),
                                })
                                .await;
                            seqno += 1;
                        }
                    }
                });
                parent
                    .command(TransportCommand::Append {
                        seqno: parent_seqno,
                        message: Box::new(task_result(exec.id)),
                    })
                    .await
                    .unwrap();
                parent_seqno += 1;
            }
            _ => {}
        }
    }

    // 等到父轮真的"停下来了"：一段时间内既没有帧、也没有收尾——这就是等待态。
    // 修改前父轮会在第 2 次调用之后直接收尾，这里会读到 END_STREAM 而失败。
    let quiet_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < quiet_deadline,
            "父轮始终没有安静下来"
        );
        match tokio::time::timeout(std::time::Duration::from_millis(300), parent_output.recv())
            .await
        {
            Err(_) => break,
            Ok(None) => panic!("父轮的流提前关闭"),
            Ok(Some(frame)) => {
                let Some(server) = frame_role(&frame) else {
                    panic!("父轮在后台子代理还在跑的时候就收尾了（等待逻辑没生效）");
                };
                if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = server.message
                {
                    parent
                        .command(TransportCommand::Append {
                            seqno: parent_seqno,
                            message: Box::new(kv_ack(kv.id)),
                        })
                        .await
                        .unwrap();
                    parent_seqno += 1;
                }
            }
        }
    }

    // 子代理结束 → 父轮应当被唤醒，带着完成消息再问一次模型并收尾。
    // 用 notify_one：万一流还没被轮询到，通知会留成许可，不会丢。
    finish_child.notify_one();
    for _ in 0..100 {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM runs WHERE cursor_request_id = 'child-request'")
                .fetch_optional(store.pool())
                .await
                .unwrap();
        if status.as_deref().is_some_and(|status| status != "running") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let child_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM runs WHERE cursor_request_id = 'child-request'")
            .fetch_optional(store.pool())
            .await
            .unwrap();
    assert_eq!(
        child_status.as_deref(),
        Some("completed"),
        "子代理没有正常收尾"
    );
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), parent_output.recv())
            .await
            .expect("父轮没有被唤醒")
            .expect("父轮的流提前关闭");
        let Some(server) = frame_role(&frame) else {
            break;
        };
        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = server.message {
            parent
                .command(TransportCommand::Append {
                    seqno: parent_seqno,
                    message: Box::new(kv_ack(kv.id)),
                })
                .await
                .unwrap();
            parent_seqno += 1;
        }
    }

    // 父轮问了三次模型：派子代理、收尾发言、被唤醒之后的续写。
    let requests = provider.requests();
    let calls: Vec<(String, String, Option<i64>)> =
        sqlx::query_as("SELECT call_id, status, completed_at_ms FROM tool_round_calls")
            .fetch_all(store.pool())
            .await
            .unwrap();
    let runs: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT run_id, status, parent_run_id FROM runs")
            .fetch_all(store.pool())
            .await
            .unwrap();
    println!(
        "DIAG tool_round_calls = {calls:?}\nDIAG runs = {runs:?}\nDIAG requests = {}",
        requests.len()
    );
    assert_eq!(requests.len(), 4, "父轮 3 次 + 子代理 1 次");
    let follow_up = &requests[3];
    let completion = follow_up
        .history
        .iter()
        .filter_map(|message| match &message.content {
            ProjectedContent::Parts(parts) => parts.iter().find_map(|part| match part {
                ContentPart::Text { text } if text.contains("kind: subagent") => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .next()
        .expect("唤醒后的父轮历史里必须有子代理完成消息");
    assert!(completion.contains("child finished"), "{completion}");
    assert!(completion.contains(TASK_CALL), "{completion}");

    // 血缘：子代理这一轮记着父运行，并且已经收尾。
    let child_run: (String, Option<String>, String) = sqlx::query_as(
        "SELECT run_kind, parent_run_id, status FROM runs WHERE cursor_request_id = 'child-request'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(child_run.0, "subagent");
    assert!(child_run.1.is_some(), "子代理必须记下父运行");
    assert_eq!(child_run.2, "completed");

    // 父会话最后的状态里，任务气泡该被标成完成。
    let messages = store
        .load_current_messages(&cursor_server::model::ConversationId::new(
            PARENT_CONVERSATION,
        ))
        .await
        .unwrap();
    assert!(messages.iter().any(|message| {
        message.runtime_event_id.as_deref()
            == Some(format!("background-completed:BACKGROUND_TASK_KIND_SUBAGENT:{CHILD_CONVERSATION}:{TASK_CALL}").as_str())
            && matches!(&message.content, MessageContent::Parts { parts } if !parts.is_empty())
    }));
}
