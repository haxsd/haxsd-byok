//! Maps conversation IDs to active conversation runtimes.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::{mpsc, Mutex, Notify};

use crate::{
    cursor::{prompting::PromptCompiler, transport::TransportHandle},
    model::{ConversationId, RunId},
    provider::Provider,
    run::{BackgroundWaitPolicy, CommandResult, RunHandle},
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
    /// 父轮等待后台子代理的参数；生产用默认值，测试用短值。
    pub background_wait: BackgroundWaitPolicy,
}

struct RegistryInner {
    current: Mutex<HashMap<ConversationId, ActiveRun>>,
    /// 活跃 run 结束后到达的消息：按会话排队，等下一次 run 开头吃进去。
    /// 时间戳是"这个会话最早一条待投递消息"的入队时刻，用于超上限时丢最久的会话。
    pending: Mutex<HashMap<ConversationId, (Instant, PendingMessages)>>,
    /// 用户按停止（客户端断开）结束过运行、此后还没有新运行的会话 → 记下的时刻。
    ///
    /// 停止只取消在跑的那一轮，它派出去的后台子代理还在跑；子代理随后完成时，完成
    /// 通知不能把已停止的会话重新拉起来（实测里"停了自己醒"就是这么来的）。新运行
    /// 一开始就说明会话又被用户打开了，这一标记随之清掉；超过 `STOPPED_TTL` 也失效，
    /// 免得长驻进程里按会话累积。
    stopped: Mutex<HashMap<ConversationId, Instant>>,
    changed: Notify,
    pub dependencies: ConversationDependencies,
}

/// `pending` 里最多同时挂多少个会话：超上限丢挂得最久的那个。
///
/// 排队等待的是"下一次 run"——客户端再也不开那个会话时，谁也不会来取。单个会话内的
/// 条数上限在 `PendingMessages` 里。
const MAX_PENDING_CONVERSATIONS: usize = 64;

/// "用户停止过"这条标记的有效期。
///
/// 后台子代理可以跑很久，但标记本身没有天然终点。窗口内保持严格抑制；超过窗口的完成
/// 通知允许重新唤醒（用户早就不在这个会话上了），代价是一次多余唤醒，换来的是这张表
/// 有界。
const STOPPED_TTL: Duration = Duration::from_secs(24 * 60 * 60);

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
        Self::with_background_wait(
            store,
            provider,
            compiler,
            web_cache,
            local_rules_dir,
            BackgroundWaitPolicy::default(),
        )
    }

    pub(crate) fn with_background_wait(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
        local_rules_dir: Option<std::path::PathBuf>,
        background_wait: BackgroundWaitPolicy,
    ) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                current: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                stopped: Mutex::new(HashMap::new()),
                changed: Notify::new(),
                dependencies: ConversationDependencies {
                    store,
                    provider,
                    compiler,
                    web_cache,
                    local_rules_dir,
                    background_wait,
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
        let now = Instant::now();
        let mut stopped = self.inner.stopped.lock().await;
        stopped.retain(|_, at| now.duration_since(*at) < STOPPED_TTL);
        stopped.insert(conversation_id.clone(), now);
    }

    /// 客户端又在这个会话里开口了：停止标记失效，唤醒恢复。
    pub(crate) async fn clear_stopped(&self, conversation_id: &ConversationId) {
        self.inner.stopped.lock().await.remove(conversation_id);
    }

    /// 这个会话是不是"用户停过、窗口内还没开口"。
    ///
    /// 过期的标记顺手删掉：判断本身就是回收点，不需要额外的清扫任务。
    async fn is_stopped(&self, conversation_id: &ConversationId) -> bool {
        let now = Instant::now();
        let mut stopped = self.inner.stopped.lock().await;
        match stopped.get(conversation_id) {
            Some(at) if now.duration_since(*at) < STOPPED_TTL => true,
            Some(_) => {
                stopped.remove(conversation_id);
                false
            }
            None => false,
        }
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
        if self.is_stopped(conversation_id).await {
            return CommandResult::Suppressed;
        }
        self.deliver(conversation_id, compiled).await
    }

    /// 把消息排进"等下一次 run"的队列，顺带维持会话数上限。
    async fn queue_pending(&self, conversation_id: &ConversationId, compiled: CompiledMessages) {
        let mut pending = self.inner.pending.lock().await;
        if !pending.contains_key(conversation_id) && pending.len() >= MAX_PENDING_CONVERSATIONS {
            if let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, (queued_at, _))| *queued_at)
                .map(|(conversation_id, _)| conversation_id.clone())
            {
                pending.remove(&oldest);
                tracing::warn!(
                    conversation_id = oldest.as_str(),
                    limit = MAX_PENDING_CONVERSATIONS,
                    "dropping the conversation that has waited longest for its next Run"
                );
            }
        }
        pending
            .entry(conversation_id.clone())
            .or_insert_with(|| (Instant::now(), PendingMessages::default()))
            .1
            .push(compiled);
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
            self.queue_pending(conversation_id, compiled).await;
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
            self.queue_pending(conversation_id, pending).await;
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
            .map(|(_, mut pending)| pending.drain().collect())
            .unwrap_or_default()
    }

    pub async fn shutdown(&self) {
        let current = std::mem::take(&mut *self.inner.current.lock().await);
        for active in current.into_values() {
            active.handle.cancel();
        }
    }
}
