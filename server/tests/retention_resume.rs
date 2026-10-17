//! 会话数据被保留期回收之后，继续使用旧会话会发生什么。
//!
//! 结论（理由见 `server/src/store/retention.rs` 里 `CONVERSATION_RETENTION_MS` 的注释）：
//! 服务端不持有旧会话的权威副本，Cursor 客户端才是；继续使用旧对话时客户端把 checkpoint
//! 原样带回来，服务端先查本地、再按 BlobID 向客户端要，然后重新水合。这个文件用一个假
//! 客户端把两件事钉住：
//!
//! 1. 本地数据被回收后继续使用旧会话：按需取回 → 正常跑完，而且新一轮发给模型的完整历史
//!    以上一轮的历史为前缀——前缀缓存不失效，不是"退化到重放全部上下文"；
//! 2. 客户端也没有那份数据时：明确报错（`missing message Blob`），而不是静默地少一段历史。

#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::{collections::HashMap, sync::Arc, time::Duration};

use bytes::Bytes;
use cursor_server::{
    cursor::{
        prompting::{PromptAssets, PromptCompiler},
        protocol::{connect, proto::agent::v1 as pb},
        TransportCommand, TransportHandle, TransportRegistry,
    },
    model::ModelRequest,
    provider::{FinishReason, ModelEvent},
    store::BlobId,
};
use prost::Message;
use serde_json::{json, Value};
use tokio::sync::mpsc::Receiver;

const CONVERSATION: &str = "conversation";
/// 假客户端等一帧输出的上限：超时说明服务端卡住了，比断言失败更难查，所以要先报出来。
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// 假 Cursor 客户端：收下服务端发布的每一个 blob（这正是真实客户端存的副本），
/// 服务端按 BlobID 来要时再送回去。
///
/// `serve_blobs = false` 模拟"客户端自己也没有那份数据"（用户在另一台机器上打开、或本地
/// 历史被清过）：服务端应当明确报错，而不是带着缺一段的历史继续跑。
struct FakeCursorClient {
    blobs: HashMap<Vec<u8>, Vec<u8>>,
    seqno: i64,
    serve_blobs: bool,
    gets_served: usize,
    served_missing: usize,
}

impl FakeCursorClient {
    fn new() -> Self {
        Self {
            blobs: HashMap::new(),
            seqno: 0,
            serve_blobs: true,
            gets_served: 0,
            served_missing: 0,
        }
    }

    fn forget_everything() -> Self {
        Self {
            serve_blobs: false,
            ..Self::new()
        }
    }

    /// 每个传输（每个 request id）有自己的 append 序号：换一轮就是换传输，序号从 0 重来。
    fn begin_request(&mut self) {
        self.seqno = 0;
    }

    async fn append(&mut self, handle: &TransportHandle, message: pb::AgentClientMessage) {
        handle
            .command(TransportCommand::Append {
                seqno: self.seqno,
                message: Box::new(message),
            })
            .await
            .unwrap();
        self.seqno += 1;
    }

    async fn acknowledge(&mut self, handle: &TransportHandle, id: u32) {
        self.append(
            handle,
            pb::AgentClientMessage {
                message: Some(pb::agent_client_message::Message::KvClientMessage(
                    pb::KvClientMessage {
                        id,
                        message: Some(pb::kv_client_message::Message::SetBlobResult(
                            pb::SetBlobResult { error: None },
                        )),
                    },
                )),
            },
        )
        .await;
    }

    async fn serve_get(&mut self, handle: &TransportHandle, id: u32, blob_id: &[u8]) {
        self.gets_served += 1;
        let data = self
            .serve_blobs
            .then(|| self.blobs.get(blob_id).cloned())
            .flatten();
        if data.is_none() {
            self.served_missing += 1;
        }
        self.append(
            handle,
            pb::AgentClientMessage {
                message: Some(pb::agent_client_message::Message::KvClientMessage(
                    pb::KvClientMessage {
                        id,
                        message: Some(pb::kv_client_message::Message::GetBlobResult(
                            pb::GetBlobResult {
                                blob_data: data,
                                error: None,
                            },
                        )),
                    },
                )),
            },
        )
        .await;
    }

    /// 把这一轮跑到流结束：收下发布的 blob、按需回送 blob、记住最后一次会话状态。
    /// 返回（最后一次会话状态，结束帧的 JSON：成功时是 `{}`，失败时带 `error`）。
    async fn run_turn(
        &mut self,
        handle: &TransportHandle,
        output: &mut Receiver<Bytes>,
    ) -> (Option<pb::ConversationStateStructure>, Value) {
        let mut state = None;
        loop {
            let frame = tokio::time::timeout(FRAME_TIMEOUT, output.recv())
                .await
                .expect("the run must make progress")
                .expect("the stream must end with an EndStream frame");
            let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
            if flags & connect::END_STREAM_FLAG != 0 {
                return (state, serde_json::from_slice(&payload).unwrap());
            }
            let message = pb::AgentServerMessage::decode(payload)
                .expect("every non-terminal frame is an AgentServerMessage");
            match message.message {
                Some(pb::agent_server_message::Message::KvServerMessage(kv)) => match kv.message {
                    Some(pb::kv_server_message::Message::SetBlobArgs(args)) => {
                        self.blobs
                            .insert(args.blob_id.clone(), args.blob_data.clone());
                        self.acknowledge(handle, kv.id).await;
                    }
                    Some(pb::kv_server_message::Message::GetBlobArgs(args)) => {
                        self.serve_get(handle, kv.id, &args.blob_id).await;
                    }
                    None => {}
                },
                Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(next)) => {
                    state = Some(next);
                }
                Some(pb::agent_server_message::Message::ExecServerMessage(_)) => {
                    panic!("a plain text turn must not ask the client to execute a tool");
                }
                _ => {}
            }
        }
    }
}

fn text_response(model_call_id: &str, text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: model_call_id.into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta(text.into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(FinishReason::Stop),
    ]
}

fn user_request(text: &str, request_id: &str) -> pb::AgentClientMessage {
    user_request_with_state(text, request_id, None)
}

fn user_request_with_state(
    text: &str,
    request_id: &str,
    state: Option<pb::ConversationStateStructure>,
) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(pb::conversation_action::Action::UserMessageAction(
                        pb::UserMessageAction {
                            user_message: Some(pb::UserMessage {
                                text: text.into(),
                                message_id: format!("user-{text}"),
                                mode: pb::AgentMode::Agent as i32,
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }),
                conversation_state: state,
                conversation_id: Some(CONVERSATION.into()),
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

fn registry(
    store: &cursor_server::store::Store,
    provider: &fake_provider::FakeProvider,
) -> TransportRegistry {
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    TransportRegistry::new(
        store.clone(),
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    )
}

/// 跑一轮会话，返回（会话状态，服务端这一轮写给模型的请求）。
async fn run_text_turn(
    registry: &TransportRegistry,
    client: &mut FakeCursorClient,
    request_id: &str,
    text: &str,
    state: Option<pb::ConversationStateStructure>,
) -> (Option<pb::ConversationStateStructure>, Value) {
    let handle = registry.get_or_create(request_id).await.unwrap();
    let mut output = handle.subscribe();
    client.begin_request();
    client
        .append(&handle, user_request_with_state(text, request_id, state))
        .await;
    let (state, end) = client.run_turn(&handle, &mut output).await;
    handle.disconnect().await;
    (state, end)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// 把这份"旧会话"变成过期的：会话时间拨回窗口之前，blob 的"最近使用"也一起拨回去。
async fn age_conversation_past_the_window(store: &cursor_server::store::Store) {
    sqlx::query("UPDATE conversations SET updated_at_ms = 1")
        .execute(store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
        .execute(store.pool())
        .await
        .unwrap();
}

/// 回收一轮，并断言这次回收真的把本地会话数据清掉了（否则后面的用例证明不了任何事）。
async fn recycle_and_assert_local_copy_is_gone(
    store: &cursor_server::store::Store,
    root_ids: &[Vec<u8>],
) {
    store.prune_inactive_storage(now_ms()).await.unwrap();
    let conversations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(conversations, 0, "the idle conversation must be recycled");
    for raw in root_ids {
        let id = BlobId::from_bytes(raw).unwrap();
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE blob_id = ?")
            .bind(id.as_bytes().as_slice())
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            stored, 0,
            "the checkpoint blobs must be gone from the store"
        );
    }
}

fn history_of(requests: &[ModelRequest], index: usize) -> Vec<String> {
    requests[index]
        .history
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect()
}

/// 本地数据被回收之后，旧会话照常继续：客户端把 checkpoint 带回来，服务端按 BlobID
/// 取回缺的内容块并重新水合。第二轮发给模型的完整历史以第一轮为前缀。
#[tokio::test]
async fn a_recycled_conversation_resumes_from_the_client_copy_without_losing_the_prefix() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response("model-1", "first answer"));
    provider.push(text_response("model-2", "second answer"));
    let registry = registry(&store, &provider);
    let mut client = FakeCursorClient::new();

    let (published, end) = run_text_turn(&registry, &mut client, "turn-1", "hello", None).await;
    assert_eq!(end, json!({}), "the first turn must finish cleanly");

    let published = published.expect("the server publishes a checkpoint every turn");
    let root_ids = published.root_prompt_messages_json.clone();
    assert!(
        !root_ids.is_empty(),
        "a published checkpoint names the blobs the client must keep"
    );

    // 一个闲置超过窗口、又没有运行在跑的会话：回收把它从本地清掉。
    age_conversation_past_the_window(&store).await;
    recycle_and_assert_local_copy_is_gone(&store, &root_ids).await;
    assert_eq!(provider.requests().len(), 1);

    // 用户重新打开这个旧对话继续提问：带的还是同一份 checkpoint，服务端只能向客户端要。
    let (_state, end) = run_text_turn(
        &registry,
        &mut client,
        "turn-2",
        "again",
        Some(published.clone()),
    )
    .await;
    assert_eq!(
        end,
        json!({}),
        "resuming a recycled conversation must not fail"
    );
    assert!(
        client.gets_served > 0,
        "the server must have asked the client for the recycled blobs"
    );
    assert_eq!(
        client.served_missing, 0,
        "the client had every requested blob"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one model call per turn");
    let first = history_of(&requests, 0);
    let second = history_of(&requests, 1);
    assert!(
        second.len() > first.len(),
        "the second turn appends to the same history instead of starting over"
    );
    assert_eq!(
        second[..first.len()],
        first[..],
        "the previous provider history must remain an exact prefix (prefix cache) "
    );
}

/// 客户端也没有那份副本时：明确报错，而不是带着缺了一段的历史继续跑。
#[tokio::test]
async fn a_recycled_conversation_without_the_client_copy_reports_a_missing_blob() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_response("model-1", "first answer"));
    provider.push(text_response("model-2", "second answer"));
    let registry = registry(&store, &provider);
    let mut client = FakeCursorClient::new();

    let (published, end) = run_text_turn(&registry, &mut client, "turn-1", "hello", None).await;
    assert_eq!(end, json!({}));
    let published = published.expect("the server publishes a checkpoint every turn");
    let root_ids = published.root_prompt_messages_json.clone();
    age_conversation_past_the_window(&store).await;
    recycle_and_assert_local_copy_is_gone(&store, &root_ids).await;

    // 换一个"什么都不记得"的客户端：服务端要不到 BlobID 指向的内容块。
    let mut forgetful = FakeCursorClient::forget_everything();
    let (_state, end) = run_text_turn(
        &registry,
        &mut forgetful,
        "turn-2",
        "again",
        Some(published),
    )
    .await;
    assert!(forgetful.served_missing > 0);
    assert_eq!(
        end["error"]["code"], "invalid_argument",
        "a missing blob is a clear, user-visible request error: {end}"
    );
    let message = end["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("missing message Blob"),
        "the error must name the missing blob instead of failing later: {message}"
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "no model call may be issued from a history that lost its base"
    );
}

/// 有运行在跑的会话（这一轮还没结束）回收一律不碰：`runs.status = 'running'` 与
/// `conversations.active_run_id` 是数据库这一侧的硬线，上层（
/// `TransportRegistry::maintain_storage_if_idle`）还先检查"没有活跃传输"。
#[tokio::test]
async fn maintenance_leaves_a_turn_in_flight_alone() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    let gate = provider.push_gated(text_response("model-1", "late answer"));
    let registry = registry(&store, &provider);
    let mut client = FakeCursorClient::new();

    let handle = registry.get_or_create("in-flight").await.unwrap();
    let mut output = handle.subscribe();
    client.begin_request();
    client
        .append(&handle, user_request("working", "in-flight"))
        .await;
    // 等到服务端已经把这个会话跑起来：运行行落库，模型那一轮还挂在闸门上。
    // （不要在闸门打开前抢收帧——那样会把等待确认的 blob 写丢了，回收反而成了替罪羊。）
    let mut run_in_flight = false;
    for _ in 0..200 {
        let running: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE status = 'running'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        if running > 0 {
            run_in_flight = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(run_in_flight, "the run must be in flight before aging it");

    age_conversation_past_the_window(&store).await;
    store.prune_inactive_storage(now_ms()).await.unwrap();

    let conversations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(conversations, 1, "a live turn pins its conversation");
    assert!(messages > 0, "a live turn keeps its messages");

    // 放行模型，让这一轮正常收尾（否则取消路径会留下 running 行）。
    gate.notify_one();
    let (state, end) = client.run_turn(&handle, &mut output).await;
    assert!(state.is_some());
    assert_eq!(end, json!({}));
}
