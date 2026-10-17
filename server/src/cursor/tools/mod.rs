//! Exposes the extensible Cursor Tool system.
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use tokio::sync::Mutex;

pub mod codec;
pub(crate) mod compat;
pub(crate) mod edit;
pub(crate) mod registry;
pub mod runtime;
mod schedule;
pub(crate) mod stream;
mod tool_call_dispatch;
pub(crate) mod tool_call_result;
pub(crate) mod turn_steps;

use crate::{
    model::ToolCall,
    search::{WebCache, WebFetch, WebSearch},
    store::Store,
    Error, Result,
};

use self::schedule::{DeferredEdit, EditSchedule};
use self::tool_call_result::{ToolCompletion, ToolResultSender};
use super::protocol::proto::agent::v1 as pb;
use runtime::{CursorToolRuntime, ExecContext};

#[derive(Clone)]
pub struct ToolDispatcher {
    runtime: CursorToolRuntime,
    results: ToolResultSender,
    search: SearchSource,
    fetch: WebFetch,
    store: Option<Store>,
    edit_schedule: Arc<Mutex<EditSchedule>>,
}

/// 搜索用的 `WebSearch`：按出网路由代次决定要不要换实例。
///
/// `WebSearch` 自己按实例记客户端——一次 run 内 23 个引擎共用一份连接池，这个记忆值得留。
/// 但它记的是**建实例那一刻**的代理设置，而用户完全可能在一次 run 中途换代理；那之后
/// 实例内的记忆不会自己更新，下一次搜索还会走旧路由。代次（`network::route_generation`）
/// 在每次"按新配置失效"时 +1，这里拿它当判据：没变就复用实例（连接池继续用），变了就
/// 重建——下一次搜索按新路由建客户端。
#[derive(Clone)]
enum SearchSource {
    /// 没有 store 的调用方（`ToolDispatcher::new`）：内建引擎配内建客户端，与代理设置无关。
    BuiltIn(Arc<WebSearch>),
    Managed {
        store: Store,
        current: Arc<parking_lot::Mutex<(u64, Arc<WebSearch>)>>,
    },
}

impl SearchSource {
    fn built_in() -> Self {
        Self::BuiltIn(Arc::new(WebSearch::built_in()))
    }

    fn managed(store: Store) -> Self {
        let current = Arc::new(parking_lot::Mutex::new((
            crate::network::route_generation(),
            Arc::new(WebSearch::managed(store.clone())),
        )));
        Self::Managed { store, current }
    }

    /// 当前该用的搜索实例。
    fn get(&self) -> Arc<WebSearch> {
        match self {
            Self::BuiltIn(search) => search.clone(),
            Self::Managed { store, current } => {
                let generation = crate::network::route_generation();
                let mut current = current.lock();
                if current.0 != generation {
                    current.0 = generation;
                    current.1 = Arc::new(WebSearch::managed(store.clone()));
                }
                current.1.clone()
            }
        }
    }
}

pub struct DispatchedTool {
    pub messages: Vec<pb::AgentServerMessage>,
    pub completion: Option<ToolCompletion>,
}

pub struct ToolBatchState<'a> {
    pub completed: &'a HashSet<String>,
    pub response_text: &'a str,
    pub response_thinking: &'a str,
}

pub enum ClientToolEvent {
    Completed(Box<ToolCompletion>),
    Pending,
}

impl ToolDispatcher {
    pub fn new(runtime: CursorToolRuntime) -> Self {
        let (results, _) = tool_call_result::tool_result_channel();
        Self {
            runtime,
            results,
            search: SearchSource::built_in(),
            fetch: WebFetch::built_in(),
            store: None,
            edit_schedule: Arc::new(Mutex::new(EditSchedule::default())),
        }
    }

    pub fn with_results(
        runtime: CursorToolRuntime,
        results: ToolResultSender,
        store: Store,
        web_cache: WebCache,
    ) -> Self {
        Self {
            runtime,
            results,
            search: SearchSource::managed(store.clone()),
            fetch: WebFetch::managed(store.clone(), web_cache),
            store: Some(store),
            edit_schedule: Arc::new(Mutex::new(EditSchedule::default())),
        }
    }

    /// 派发一批工具调用。
    ///
    /// `turn_steps` 是**当前 turn 已经提交的 step 数**(见 `turn_steps::TurnSteps`):调用方
    /// 在形状可能变化时按历史数一次并记住,之后每个工具轮只往前推进,不必再把整段历史
    /// 装载回来(`06/P3-5`)。
    pub async fn start_batch(
        &self,
        calls: &[ToolCall],
        state: ToolBatchState<'_>,
        turn_steps: usize,
        dynamic_mcp: &BTreeMap<String, pb::McpToolDefinition>,
        context: &ExecContext,
    ) -> Result<Vec<DispatchedTool>> {
        let first_tool_index =
            turn_steps::first_tool_index(turn_steps, state.response_thinking, state.response_text);
        let mut dispatched = Vec::new();
        for (position, call) in calls.iter().enumerate() {
            if state.completed.contains(&call.call_id) {
                continue;
            }
            let message_index = first_tool_index + position;
            if let Some(error) = &call.argument_error {
                dispatched.push(validation_failure(call, error.clone()));
                continue;
            }
            let edit_path = if dynamic_mcp.contains_key(&call.name) {
                None
            } else {
                match edit::execution_path(call, &context.workspace_paths) {
                    Ok(path) => path,
                    Err(error) => {
                        dispatched.push(recover_validation_failure(call, error)?);
                        continue;
                    }
                }
            };
            if let Some(path) = edit_path {
                let next = self.edit_schedule.lock().await.start_or_defer(
                    path,
                    DeferredEdit {
                        call: call.clone(),
                        message_index,
                        context: context.clone(),
                    },
                );
                let Some(next) = next else {
                    continue;
                };
                let started = self
                    .start(&next.call, next.message_index, dynamic_mcp, &next.context)
                    .await;
                dispatched.push(match started {
                    Ok(started) => started,
                    Err(error) => recover_validation_failure(&next.call, error)?,
                });
                continue;
            }
            let started = self.start(call, message_index, dynamic_mcp, context).await;
            dispatched.push(match started {
                Ok(started) => started,
                Err(error) => recover_validation_failure(call, error)?,
            });
        }
        Ok(dispatched)
    }

    pub(crate) async fn continue_after(&self, call_id: &str) -> Result<Option<DispatchedTool>> {
        let next = self.edit_schedule.lock().await.complete(call_id)?;
        let Some(next) = next else {
            return Ok(None);
        };
        match self
            .start(
                &next.call,
                next.message_index,
                &BTreeMap::new(),
                &next.context,
            )
            .await
        {
            Ok(started) => Ok(Some(started)),
            Err(error) => recover_validation_failure(&next.call, error).map(Some),
        }
    }

    pub async fn interrupt_for_message(&self) -> Vec<u32> {
        self.edit_schedule.lock().await.clear();
        self.runtime.interrupt_for_message().await
    }

    /// 起一个工具调用。
    ///
    /// `tool_started` 每次都发:调用点传来的"已发布集合"在生产里恒为空(没有任何地方往里
    /// 写过东西),所以"跳过"那条路径从来没有生效过;留着一个永远为真的开关只会让人以为
    /// 重放时不会重复发卡片。轮内重放由 `completed` 挡住,不需要第二份集合。
    async fn start(
        &self,
        call: &ToolCall,
        message_index: usize,
        dynamic_mcp: &BTreeMap<String, pb::McpToolDefinition>,
        context: &ExecContext,
    ) -> Result<DispatchedTool> {
        let call = context.prepare_call(call)?;
        let mut messages = vec![codec::tool_started(&call, dynamic_mcp.get(&call.name))?];
        let started = tool_call_dispatch::start(
            &self.runtime,
            &self.results,
            &call,
            message_index,
            dynamic_mcp,
            context,
            self.store.as_ref(),
        )
        .await?;
        messages.extend(started.messages);
        Ok(DispatchedTool {
            messages,
            completion: started.completion,
        })
    }

    pub async fn interaction_response(
        &self,
        response: &pb::InteractionResponse,
    ) -> Result<ClientToolEvent> {
        if self.runtime.is_interrupted(response.id).await {
            return Ok(ClientToolEvent::Pending);
        }
        let pending = match self.runtime.take_interaction(response.id).await {
            Some(pending) => pending,
            None if self.runtime.completed_call(response.id).await.is_some() => {
                tracing::warn!(
                    id = response.id,
                    "ignoring duplicate terminal interaction response"
                );
                return Ok(ClientToolEvent::Pending);
            }
            None => {
                tracing::warn!(
                    id = response.id,
                    "ignoring response for unknown interaction"
                );
                return Ok(ClientToolEvent::Pending);
            }
        };
        let call = pending.call.clone();
        let search = self.search.get();
        let continuation = match tool_call_dispatch::resume_interaction(
            &self.runtime,
            &self.results,
            &search,
            &self.fetch,
            pending,
            response,
        )
        .await
        {
            Ok(continuation) => continuation,
            Err(Error::Protocol(message)) => {
                return Ok(ClientToolEvent::Completed(Box::new(
                    compat::failure_with_message(&call, message),
                )));
            }
            Err(Error::Json(error)) => {
                return Ok(ClientToolEvent::Completed(Box::new(
                    compat::failure_with_message(&call, error.to_string()),
                )));
            }
            Err(error) => return Err(error),
        };
        Ok(match continuation {
            tool_call_dispatch::InteractionContinuation::Completed(completion) => {
                ClientToolEvent::Completed(completion)
            }
            tool_call_dispatch::InteractionContinuation::Pending => ClientToolEvent::Pending,
        })
    }
}

fn validation_failure(call: &ToolCall, message: String) -> DispatchedTool {
    DispatchedTool {
        messages: Vec::new(),
        completion: Some(compat::failure_with_message(call, message)),
    }
}

fn recover_validation_failure(call: &ToolCall, error: Error) -> Result<DispatchedTool> {
    match error {
        Error::Protocol(message) => Ok(validation_failure(call, message)),
        Error::Json(error) => Ok(validation_failure(call, error.to_string())),
        error => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::cursor::tools::codec::{client_event, ClientExecEvent};
    use crate::cursor::tools::runtime::CursorToolRuntime;

    /// 搜索那份客户端必须跟着出网路由走：代次没变就复用同一实例（一次 run 内 23 个引擎
    /// 共用一份连接池），代次一变（保存设置 / 系统代理变化 / 直连兜底，都经由
    /// `NetworkClients::invalidate`）就换实例——下一次搜索按新路由建客户端。
    ///
    /// 恒复用（改动前的行为）会让"必须换实例"那半条变红。
    ///
    /// 代次是进程级的，同进程并行跑的其他用例也可能推进它，所以"必须复用"那半条先确认
    /// 窗口内没人推进过；"必须换实例"那半条由本用例自己推进，不受别人影响。
    #[tokio::test]
    async fn the_search_instance_follows_the_outbound_route_generation() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let source = SearchSource::managed(store.clone());

        let generation = crate::network::route_generation();
        let first = source.get();
        if crate::network::route_generation() == generation {
            assert!(
                Arc::ptr_eq(&first, &source.get()),
                "代次没变时必须复用同一实例（连接池）"
            );
        }

        let before = source.get();
        let generation = crate::network::route_generation();
        crate::network::NetworkClients::new(store)
            .invalidate()
            .await;
        assert!(
            crate::network::route_generation() > generation,
            "失效必须推进代次"
        );
        assert!(
            !Arc::ptr_eq(&before, &source.get()),
            "代次变了必须换实例，否则换了代理也不会生效"
        );
    }

    /// 每个被派发的调用都必须先发 `tool_started` 卡片。
    ///
    /// 之前这由调用点传进来的"已发布集合"决定,而那个集合在生产里恒为空 —— 等于一个永远
    /// 为真的开关,却让人以为重放时不会重复发卡片(见 P3-4)。字段删掉之后这条契约由本用例
    /// 钉住:任何"跳过 start 卡片"的改动都会先在 `messages[0]` 上变红。
    #[tokio::test]
    async fn every_dispatched_call_publishes_its_start_card() {
        let runtime = CursorToolRuntime::default();
        let dispatcher = ToolDispatcher::new(runtime.clone());
        let completed = HashSet::new();
        let calls = vec![read_call("call-read", "src/a.ts")];

        let dispatched = dispatcher
            .start_batch(
                &calls,
                batch_state(&completed),
                0,
                &BTreeMap::new(),
                &ExecContext::default(),
            )
            .await
            .unwrap();

        let Some(pb::agent_server_message::Message::InteractionUpdate(update)) =
            dispatched[0].messages[0].message.as_ref()
        else {
            panic!("第一批消息必须是 tool-start 交互")
        };
        assert!(matches!(
            update.message.as_ref(),
            Some(pb::interaction_update::Message::ToolCallStarted(started))
                if started.tool_call.as_ref().is_some_and(|call| call.started_at_ms.is_some())
        ));
    }

    fn read_call(call_id: &str, path: &str) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: call_id.into(),
            model_call_id: format!("model-{call_id}"),
            name: "Read".into(),
            arguments_text: String::new(),
            arguments: json!({ "path": path }),
            argument_error: None,
        }
    }

    fn write_call(call_id: &str, path: &str, contents: &str) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: call_id.into(),
            model_call_id: format!("model-{call_id}"),
            name: "Write".into(),
            arguments_text: String::new(),
            arguments: json!({"path": path, "contents": contents}),
            argument_error: None,
        }
    }

    fn batch_state(completed: &HashSet<String>) -> ToolBatchState<'_> {
        ToolBatchState {
            completed,
            response_text: "",
            response_thinking: "",
        }
    }

    fn read_request_id(messages: &[pb::AgentServerMessage], path: &str) -> u32 {
        for message in messages {
            let Some(pb::agent_server_message::Message::ExecServerMessage(exec)) =
                message.message.as_ref()
            else {
                continue;
            };
            let Some(pb::exec_server_message::Message::ReadArgs(args)) = exec.message.as_ref()
            else {
                continue;
            };
            if args.path == path {
                return exec.id;
            }
        }
        panic!("no ReadArgs request for {path}");
    }

    fn write_args_text(event: ClientExecEvent) -> String {
        let ClientExecEvent::Message(message) = event else {
            panic!("expected a terminal encoding message");
        };
        let Some(pb::agent_server_message::Message::ExecServerMessage(exec)) = message.message
        else {
            panic!("expected an ExecServerMessage");
        };
        let Some(pb::exec_server_message::Message::WriteArgs(args)) = exec.message else {
            panic!("expected WriteArgs");
        };
        args.file_text
    }

    fn read_result(id: u32, path: &str, content: &str) -> pb::ExecClientMessage {
        pb::ExecClientMessage {
            id,
            message: Some(pb::exec_client_message::Message::ReadResult(
                pb::ReadResult {
                    result: Some(pb::read_result::Result::Success(pb::ReadSuccess {
                        path: path.into(),
                        output: Some(pb::read_success::Output::Content(content.into())),
                        ..Default::default()
                    })),
                },
            )),
            ..Default::default()
        }
    }

    /// 编辑串行化的整条链路:同一批里对同一个文件(两种写法)的两个编辑,第二个先挂起,
    /// 第一个结算后才拿到自己的 ReadArgs;两次写各自独立,不会并行算 after。
    #[tokio::test]
    async fn two_edits_of_one_file_in_a_batch_are_serialized() {
        let runtime = CursorToolRuntime::default();
        let dispatcher = ToolDispatcher::new(runtime.clone());
        let context = ExecContext::default();
        let completed = HashSet::new();
        let calls = vec![
            write_call("call-1", "src/a.ts", "first"),
            // 同一个文件的不同写法:归一到同一条队列
            write_call("call-2", "src\\a.ts", "second"),
        ];

        let dispatched = dispatcher
            .start_batch(
                &calls,
                batch_state(&completed),
                0,
                &BTreeMap::new(),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(dispatched.len(), 1, "第二个编辑必须挂起而不是并行执行");
        let first_read = read_request_id(&dispatched[0].messages, "src/a.ts");

        // 第一段:客户端回读内容 → 服务端算 after 并下发 WriteArgs
        let write = client_event(&read_result(first_read, "src/a.ts", "alpha\n"), &runtime)
            .await
            .unwrap();
        assert_eq!(write_args_text(write), "first");

        // 第一个编辑结算(生产路径由 output.rs 在拿到工具结果后调用)才放行第二个
        let next = dispatcher
            .continue_after("call-1")
            .await
            .unwrap()
            .expect("第一个编辑结算后第二个必须被派发");
        let second_read = read_request_id(&next.messages, "src\\a.ts");
        assert_ne!(
            second_read, first_read,
            "两次编辑必须是两次独立的 Exec 往返"
        );

        let write = client_event(&read_result(second_read, "src\\a.ts", "first\n"), &runtime)
            .await
            .unwrap();
        assert_eq!(write_args_text(write), "second");
    }
}
