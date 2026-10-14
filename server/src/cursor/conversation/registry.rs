//! Maps conversation IDs to active conversation runtimes.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use tokio::sync::{mpsc, Mutex, Notify};

use crate::{
    cursor::{prompting::PromptCompiler, transport::TransportHandle},
    model::{ConversationId, RunId},
    provider::Provider,
    run::{CommandResult, RunHandle},
    search::WebCache,
    store::Store,
};

use super::{CompiledMessages, MessageDelivery, PendingMessages, TransportCommand};

#[derive(Clone)]
pub struct ConversationRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Clone)]
pub(crate) struct ConversationDependencies {
    pub store: Store,
    pub provider: Arc<dyn Provider>,
    pub compiler: PromptCompiler,
    pub web_cache: WebCache,
    /// 本地 rules 服务的 md 存储目录;编译请求上下文时合并其中的规则。
    pub local_rules_dir: Option<std::path::PathBuf>,
}

struct RegistryInner {
    current: Mutex<HashMap<ConversationId, ActiveRun>>,
    pending: Mutex<HashMap<ConversationId, PendingMessages>>,
    /// 用户按停止（客户端断开）结束过运行、此后还没有新运行的会话。
    ///
    /// 停止只取消在跑的那一轮，它派出去的后台子代理还在跑；子代理随后完成时，完成
    /// 通知不能把已停止的会话重新拉起来（实测里"停了自己醒"就是这么来的）。新运行
    /// 一开始就说明会话又被用户打开了，这一标记随之清掉。
    stopped: Mutex<HashSet<ConversationId>>,
    changed: Notify,
    pub dependencies: ConversationDependencies,
}

#[derive(Clone)]
struct ActiveRun {
    run_id: RunId,
    handle: RunHandle,
}

impl ConversationRegistry {
    pub(crate) async fn has_active_runs(&self) -> bool {
        !self.inner.current.lock().await.is_empty()
    }

    pub fn new(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
        local_rules_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                current: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                stopped: Mutex::new(HashSet::new()),
                changed: Notify::new(),
                dependencies: ConversationDependencies {
                    store,
                    provider,
                    compiler,
                    web_cache,
                    local_rules_dir,
                },
            }),
        }
    }

    pub(crate) fn dependencies(&self) -> &ConversationDependencies {
        &self.inner.dependencies
    }

    pub(crate) fn bind_transport(
        &self,
        handle: TransportHandle,
        receiver: mpsc::Receiver<TransportCommand>,
    ) {
        super::ConversationRuntime::spawn(self.clone(), handle, receiver);
    }

    pub(crate) async fn activate(
        &self,
        conversation_id: ConversationId,
        run_id: RunId,
        handle: RunHandle,
    ) {
        let previous = self.inner.current.lock().await.insert(
            conversation_id,
            ActiveRun {
                run_id: run_id.clone(),
                handle,
            },
        );
        if let Some(previous) = previous.filter(|previous| previous.run_id != run_id) {
            previous.handle.cancel();
        }
    }

    /// 用户按停止结束了这个会话的运行：记下来，好让后台子代理的完成通知不再把它唤醒。
    ///
    /// 只有"断开时这一轮还在跑"才调用（见 runtime 的断开分支）：断开本身不等于停止，
    /// 轮次已收尾时的流断开不能被记成停止，否则合法的完成通知会被丢掉。
    pub(crate) async fn mark_stopped(&self, conversation_id: &ConversationId) {
        self.inner
            .stopped
            .lock()
            .await
            .insert(conversation_id.clone());
    }

    /// 客户端又在这个会话里开口了：停止标记失效，唤醒恢复。
    pub(crate) async fn clear_stopped(&self, conversation_id: &ConversationId) {
        self.inner.stopped.lock().await.remove(conversation_id);
    }

    /// 后台子代理的完成通知：用户明确停止过的会话不再被唤醒。
    ///
    /// 停止只取消在跑的那一轮，它派出去的后台子代理还在跑；子代理完成时若照旧投递，
    /// 用户下次开口就会带着 follow-up 指令把它当成一轮的前置消息吃进去——实测里
    /// "停了自己醒"就是这么来的。这里对这类会话直接丢弃：子代理的最终答复仍留在子会话
    /// 历史里，客户端自己的完成通知负责把它带回父会话。
    pub async fn deliver_wake(
        &self,
        conversation_id: &ConversationId,
        compiled: CompiledMessages,
    ) -> CommandResult {
        if self.inner.stopped.lock().await.contains(conversation_id) {
            return CommandResult::Suppressed;
        }
        self.deliver(conversation_id, compiled).await
    }

    pub async fn deliver(
        &self,
        conversation_id: &ConversationId,
        compiled: CompiledMessages,
    ) -> CommandResult {
        if compiled.delivery == MessageDelivery::Ignore {
            return CommandResult::Applied;
        }
        let active = self
            .inner
            .current
            .lock()
            .await
            .get(conversation_id)
            .cloned();
        let Some(active) = active else {
            self.inner
                .pending
                .lock()
                .await
                .entry(conversation_id.clone())
                .or_default()
                .push(compiled);
            return CommandResult::RunEnded;
        };
        if compiled
            .target_run_id
            .as_ref()
            .is_some_and(|target| target != &active.run_id)
        {
            return CommandResult::StaleTarget;
        }
        let pending = compiled.clone();
        let result = match compiled.delivery {
            MessageDelivery::Ignore => CommandResult::Applied,
            MessageDelivery::InsertMessages => {
                active
                    .handle
                    .insert_messages(compiled.event_id, compiled.messages)
                    .await
            }
            MessageDelivery::BreakMessages => {
                active
                    .handle
                    .break_messages(compiled.event_id, compiled.messages)
                    .await
            }
        };
        if matches!(result, CommandResult::RunClosing | CommandResult::RunEnded) {
            self.inner
                .pending
                .lock()
                .await
                .entry(conversation_id.clone())
                .or_default()
                .push(pending);
        }
        result
    }

    pub(crate) async fn release(&self, conversation_id: &ConversationId, run_id: &RunId) {
        let mut current = self.inner.current.lock().await;
        if current
            .get(conversation_id)
            .is_some_and(|run| &run.run_id == run_id)
        {
            current.remove(conversation_id);
            self.inner.changed.notify_waiters();
        }
    }

    pub(crate) async fn wait_until_idle(&self, conversation_id: &ConversationId) {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self
                .inner
                .current
                .lock()
                .await
                .contains_key(conversation_id)
            {
                return;
            }
            changed.await;
        }
    }

    pub(crate) async fn take_pending(
        &self,
        conversation_id: &ConversationId,
    ) -> Vec<CompiledMessages> {
        self.inner
            .pending
            .lock()
            .await
            .remove(conversation_id)
            .map(|mut pending| pending.drain().collect())
            .unwrap_or_default()
    }

    pub async fn shutdown(&self) {
        let current = std::mem::take(&mut *self.inner.current.lock().await);
        for active in current.into_values() {
            active.handle.cancel();
        }
    }
}
