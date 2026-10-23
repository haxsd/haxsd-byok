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
    run::BackgroundWaitPolicy,
};
use prost::Message;

const PARENT_CONVERSATION: &str = "wakeup-parent-conversation";
const CHILD_CONVERSATION: &str = "wakeup-child-conversation";
const TASK_CALL: &str = "call-task";

/// 「用户停止」那条用例自己的子代理身份：子代理完成通知的"已通知"集合是**进程级**的
/// （`subagents.rs` 的静态集合），两个用例共用 id 会互相去重，所以这里各用一套。
const STOPPED_TASK_CALL: &str = "call-stopped-task";
const STOPPED_CHILD_CONVERSATION: &str = "stopped-child-conversation";

/// 「活动度」那条用例自己的身份，理由同上。
const ACTIVE_TASK_CALL: &str = "call-active-task";
const ACTIVE_PARENT_CONVERSATION: &str = "active-parent-conversation";
const ACTIVE_CHILD_CONVERSATION: &str = "active-child-conversation";

/// 父轮第一轮：派一个后台子代理。
fn task_call_events(call_id: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: "parent-call-1".into(),
        },
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: call_id.into(),
            name: "Task".into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: format!(
                r#"{{"description":"Analyze game logic","prompt":"Inspect the game","run_in_background":true,"subagent_type":"generalPurpose","tool_call_id":"{call_id}"}}"#
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
fn task_result(exec_id: u32, child_conversation: &str) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id: exec_id,
                message: Some(pb::exec_client_message::Message::SubagentResult(
                    pb::SubagentResult {
                        result: Some(pb::subagent_result::Result::Success(pb::SubagentSuccess {
                            agent_id: child_conversation.into(),
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
    provider.push_for(PARENT_CONVERSATION, task_call_events(TASK_CALL));
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
                        message: Box::new(task_result(exec.id, CHILD_CONVERSATION)),
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

/// 等某个 run 落到期望状态；超时就失败，避免用例挂死。
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
    panic!("run {request_id} 没有在超时内变成 {expected}");
}

/// 等父轮把 Task 调用的结果提交掉——只有提交过，服务端才认它是"后台子代理"，
/// 才会在子代理完成时通知父会话（判据见 `Store::background_task`）。
async fn wait_for_tool_call_completed(store: &cursor_server::store::Store, call_id: &str) {
    for _ in 0..400 {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM tool_round_calls WHERE call_id = ?")
                .bind(call_id)
                .fetch_optional(store.pool())
                .await
                .unwrap();
        if status.as_deref() == Some("completed") {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("Task 调用 {call_id} 没有在超时内完成");
}

/// 等已经发出过这么多次模型调用。用来确认"挂住的那次调用已经在飞"，这样后面的
/// 停止动作才真的是打断一轮活着的运行（否则那次调用会把后面重开时的脚本吃掉）。
async fn wait_for_model_calls(provider: &fake_provider::FakeProvider, expected: usize) {
    for _ in 0..400 {
        if provider.requests().len() >= expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("模型调用数没有达到 {expected}");
}

/// 用户按停止之后，后台子代理跑完不再把已停止的会话拉起来。
///
/// 实测时序（本机 2026-10-14 17:47）：父轮派出后台子代理 → 用户按停止（客户端断开，
/// 在跑的那一轮被取消，父子代理还在跑）→ 子代理随后完成。旧行为把完成通知排进
/// pending，用户下次开口时会被当成这一轮的前置消息吃进去（带 follow-up 指令），于是
/// "停了的会话又自己动起来"。这里断言：重新开口时，模型请求与落库历史里都不含那条完成通知。
#[tokio::test]
async fn a_stopped_conversation_is_not_woken_by_a_finished_subagent() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("cursor_server=debug")
        .with_test_writer()
        .try_init();
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push_for(PARENT_CONVERSATION, task_call_events(STOPPED_TASK_CALL));
    // 父轮第二次调用挂住不放行：用户就是在"这一轮还活着"的时候按的停止。
    provider.push_gated_for(PARENT_CONVERSATION, text_events("waiting for the subagent"));
    let finish_child =
        provider.push_gated_for(STOPPED_CHILD_CONVERSATION, text_events("child finished"));
    provider.push_for(PARENT_CONVERSATION, text_events("resumed by the user"));
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

    // 驱动父轮直到它派出后台子代理：收到 exec 请求就起子代理那条流，并把"已后台化"
    // 的结果回给父轮。子代理的流也要有人读，否则它堵在输出上永远收不了尾。
    let mut parent_seqno = 1;
    let mut child = None;
    while child.is_none() {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), parent_output.recv())
            .await
            .expect("父轮没有派出后台子代理")
            .expect("父轮的流提前关闭");
        let Some(server) = frame_role(&frame) else {
            panic!("父轮在派子代理之前就收尾了");
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
                        tool_call_id: Some(STOPPED_TASK_CALL.into()),
                    })
                    .unwrap();
                handle
                    .command(TransportCommand::Append {
                        seqno: 0,
                        message: Box::new(run_request(
                            STOPPED_CHILD_CONVERSATION,
                            "child-request",
                            "child-user",
                            true,
                        )),
                    })
                    .await
                    .unwrap();
                let child_handle = handle.clone();
                let child_output = child_handle.subscribe();
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
                parent
                    .command(TransportCommand::Append {
                        seqno: parent_seqno,
                        message: Box::new(task_result(exec.id, STOPPED_CHILD_CONVERSATION)),
                    })
                    .await
                    .unwrap();
                parent_seqno += 1;
                child = Some(handle.clone());
            }
            _ => {}
        }
    }

    // 父轮的帧还得有人继续应答（KV 回执/后续 exec），否则它卡在 blob 同步上，
    // 永远走不到"带着挂住的模型调用在跑"的状态。
    tokio::spawn({
        let parent = parent.clone();
        let mut parent_output = parent_output;
        let mut seqno = parent_seqno;
        async move {
            while let Some(frame) = parent_output.recv().await {
                let Some(server) = frame_role(&frame) else {
                    break;
                };
                if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = server.message
                {
                    let _ = parent
                        .command(TransportCommand::Append {
                            seqno,
                            message: Box::new(kv_ack(kv.id)),
                        })
                        .await;
                    seqno += 1;
                }
            }
        }
    });

    // 父轮把 Task 的结果提交掉之后，它才算派出了一个"后台子代理"；此时它已经带着
    // 那次"挂住"的模型调用在跑。
    wait_for_tool_call_completed(&store, STOPPED_TASK_CALL).await;
    wait_for_model_calls(&provider, 3).await;

    // 用户按停止：客户端断开，父轮那一轮被取消。
    parent.command(TransportCommand::Disconnect).await.unwrap();
    wait_for_run_status(&store, "parent-request", "cancelled").await;

    // 子代理随后才跑完。
    finish_child.notify_one();
    wait_for_run_status(&store, "child-request", "completed").await;
    // 完成通知的处理紧跟在子代理收尾之后；给它落地的时间，断言本身才是重点。
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 用户重新开口：新的请求 id（Cursor 停止之后下一次是新的流）。
    let resumed = registry.get_or_create("resumed-request").await.unwrap();
    let mut resumed_output = resumed.subscribe();
    resumed
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                PARENT_CONVERSATION,
                "resumed-request",
                "resumed-user",
                false,
            )),
        })
        .await
        .unwrap();
    let mut resumed_seqno = 1;
    loop {
        let frame =
            match tokio::time::timeout(std::time::Duration::from_secs(10), resumed_output.recv())
                .await
            {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => panic!("重新开口的那一轮没有收尾"),
            };
        let Some(server) = frame_role(&frame) else {
            break;
        };
        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = server.message {
            resumed
                .command(TransportCommand::Append {
                    seqno: resumed_seqno,
                    message: Box::new(kv_ack(kv.id)),
                })
                .await
                .unwrap();
            resumed_seqno += 1;
        }
    }

    let history = serde_json::to_string(
        &provider
            .requests()
            .last()
            .expect("重新开口的那一轮没有问模型")
            .history,
    )
    .unwrap();
    assert!(
        !history.contains("kind: subagent"),
        "被停止的会话又被后台子代理的完成通知唤醒了：{history}"
    );
    let wake_marker = format!(
        "background-completed:BACKGROUND_TASK_KIND_SUBAGENT:{STOPPED_CHILD_CONVERSATION}:{STOPPED_TASK_CALL}"
    );
    let messages = store
        .load_current_messages(&cursor_server::model::ConversationId::new(
            PARENT_CONVERSATION,
        ))
        .await
        .unwrap();
    assert!(
        !messages
            .iter()
            .any(|message| message.runtime_event_id.as_deref() == Some(wake_marker.as_str())),
        "被停止的会话的历史里仍然出现了后台子代理的完成通知"
    );
}

/// 子代理还在持续工作时，父轮必须继续等：活动时间戳不断推进，父轮就不能收尾。
///
/// 阈值通过 `BackgroundWaitPolicy` 缩到秒级：空闲 400ms。旧实现按固定截止时间收尾
/// （与活动无关），或者只看父轮已经等了多久，都会在等待 400ms 后结束这一轮。这里先
/// 把子代理的 `updated_at_ms` 设成一小时前，再在等待期间持续刷新——只有真正读取活动
/// 时间戳的实现才能一直等到子代理结束。
#[tokio::test]
async fn a_background_subagent_that_keeps_working_is_not_cut_off_by_a_fixed_deadline() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("cursor_server=debug")
        .with_test_writer()
        .try_init();
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push_for(
        ACTIVE_PARENT_CONVERSATION,
        task_call_events(ACTIVE_TASK_CALL),
    );
    provider.push_for(
        ACTIVE_PARENT_CONVERSATION,
        text_events("waiting for the active subagent"),
    );
    let finish_child =
        provider.push_gated_for(ACTIVE_CHILD_CONVERSATION, text_events("child finished"));
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::with_background_wait(
        store.clone(),
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
        BackgroundWaitPolicy {
            poll: std::time::Duration::from_millis(20),
            completion_grace: std::time::Duration::from_millis(50),
            idle_timeout: std::time::Duration::from_millis(400),
            ceiling: std::time::Duration::from_secs(10),
            heartbeat: std::time::Duration::from_millis(100),
            progress_log: std::time::Duration::from_secs(60),
        },
    );

    let parent = registry
        .get_or_create("active-parent-request")
        .await
        .unwrap();
    let mut parent_output = parent.subscribe();
    parent
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                ACTIVE_PARENT_CONVERSATION,
                "active-parent-request",
                "active-parent-user",
                false,
            )),
        })
        .await
        .unwrap();

    // 驱动父轮派出后台子代理；子代理的流也要有人读，否则它收不了尾。
    let mut parent_seqno = 1;
    let mut child_started = false;
    while !child_started {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), parent_output.recv())
            .await
            .expect("父轮没有派出后台子代理")
            .expect("父轮的流提前关闭");
        let Some(server) = frame_role(&frame) else {
            panic!("父轮在派子代理之前就收尾了");
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
                let handle = registry
                    .get_or_create("active-child-request")
                    .await
                    .unwrap();
                handle
                    .set_parent(TransportParent {
                        request_id: Some("active-parent-request".into()),
                        tool_call_id: Some(ACTIVE_TASK_CALL.into()),
                    })
                    .unwrap();
                handle
                    .command(TransportCommand::Append {
                        seqno: 0,
                        message: Box::new(run_request(
                            ACTIVE_CHILD_CONVERSATION,
                            "active-child-request",
                            "active-child-user",
                            true,
                        )),
                    })
                    .await
                    .unwrap();
                let child_handle = handle.clone();
                let mut child_output = handle.subscribe();
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
                parent
                    .command(TransportCommand::Append {
                        seqno: parent_seqno,
                        message: Box::new(task_result(exec.id, ACTIVE_CHILD_CONVERSATION)),
                    })
                    .await
                    .unwrap();
                parent_seqno += 1;
                child_started = true;
            }
            _ => {}
        }
    }
    wait_for_tool_call_completed(&store, ACTIVE_TASK_CALL).await;
    // 父轮发起第二次模型调用之前还要回完 blob 的 KV 回执；不读帧它就会停在那里，
    // 等不到"进入等待"的那一刻。
    let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while provider.requests().len() < 3 {
        assert!(
            tokio::time::Instant::now() < ready_deadline,
            "父轮没有发起第二次模型调用"
        );
        match tokio::time::timeout(std::time::Duration::from_millis(50), parent_output.recv()).await
        {
            Ok(Some(frame)) => {
                let Some(server) = frame_role(&frame) else {
                    panic!("父轮在进入等待之前就收尾了");
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
            Ok(None) => panic!("父轮在进入等待之前就关闭了流"),
            Err(_) => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // 先造出"子代理上次活动是一小时前"：不读活动时间戳的实现会把它当成已经空闲。
    sqlx::query(
        "UPDATE runs
            SET updated_at_ms = CAST(unixepoch('subsec') * 1000 AS INTEGER) - 3600000
          WHERE cursor_request_id = 'active-child-request'",
    )
    .execute(store.pool())
    .await
    .unwrap();

    // 之后持续刷新活动，跑过 3 个空闲阈值：父轮必须一直保持等待，并且有保活帧。
    let mut saw_heartbeat = false;
    let refresh_deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1_500);
    while tokio::time::Instant::now() < refresh_deadline {
        sqlx::query(
            "UPDATE runs
                SET updated_at_ms = CAST(unixepoch('subsec') * 1000 AS INTEGER)
              WHERE cursor_request_id = 'active-child-request'",
        )
        .execute(store.pool())
        .await
        .unwrap();
        match tokio::time::timeout(std::time::Duration::from_millis(30), parent_output.recv()).await
        {
            Ok(Some(frame)) => match frame_role(&frame) {
                Some(server) => match server.message {
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
                    Some(pb::agent_server_message::Message::InteractionUpdate(update)) => {
                        if matches!(
                            update.message,
                            Some(pb::interaction_update::Message::Heartbeat(_))
                        ) {
                            saw_heartbeat = true;
                        }
                    }
                    _ => {}
                },
                None => panic!("父轮在子代理仍在活动时就收尾了"),
            },
            Ok(None) => panic!("父轮在子代理仍在活动时关闭了流"),
            Err(_) => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }

    let parent_status: String = sqlx::query_scalar(
        "SELECT status FROM runs WHERE cursor_request_id = 'active-parent-request'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(parent_status, "running", "子代理持续有活动，父轮却已经收尾");
    assert_eq!(
        provider.requests().len(),
        3,
        "父轮 2 次 + 子代理那条挂住的调用；等待期间父轮不该再问模型，也不该提前收尾"
    );
    assert!(saw_heartbeat, "长等待期间没有发出保活帧");

    // 停止刷新：子代理真的空闲了，父轮应在空闲阈值后收尾，而不是永久挂住。
    let mut ended = false;
    let end_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < end_deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(500), parent_output.recv())
            .await
        {
            Ok(Some(frame)) => match frame_role(&frame) {
                Some(server) => {
                    if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) =
                        server.message
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
                None => {
                    ended = true;
                    break;
                }
            },
            Ok(None) => {
                ended = true;
                break;
            }
            Err(_) => {}
        }
    }
    assert!(ended, "子代理已经没有活动，父轮却仍未收尾");
    wait_for_run_status(&store, "active-parent-request", "completed").await;
    assert_eq!(provider.requests().len(), 3, "空闲收尾不该再问一次模型");

    // 让子代理正常结束，避免测试结束时还留着一条挂住的运行。
    finish_child.notify_one();
    wait_for_run_status(&store, "active-child-request", "completed").await;
}

// 边界说明：为什么这里没有"服务端已自行收尾之后客户端才断开"这条用例——
// 引擎会在后台子代理跑完之前一直 hold 住父轮的运行（日志里每 5 秒一条
// "parent turn is waiting for background subagent"），所以"父轮已收尾 + 子代理仍在跑"
// 这个组合在真实流程里构造不出来，除非父轮以失败收尾。断开判据本身（只有"断开时这一轮
// 还在跑"才记成用户停止）落在 runtime.rs 的 Disconnect 分支里，行为由上面两条用例
// 从两侧夹住：活着的轮次被打断 → 不唤醒；轮次正常跑完 → 照旧唤醒。
