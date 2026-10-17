//! Maps request IDs to active transport handles.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use tokio::sync::{mpsc, Mutex, Notify};

use crate::{
    cursor::{
        conversation::ConversationRegistry, prompting::PromptCompiler,
        services::observability::CursorTraceService,
    },
    plugin::PluginRegistry,
    provider::Provider,
    search::WebCache,
    store::Store,
    Error, Result,
};

use super::{OutputHub, TransportHandle};

/// RunSSE 等路由落地的上界。正常时序是 BidiAppend 先建好 transport、RunSSE 随后附着,
/// 几十秒足够;超过它说明这个 request_id 上不会再有人建 transport。
const ROUTE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// upstream 标记在多久没人碰过之后算残留。
///
/// 标记没有天然终点：它由带模型的 BidiAppend 建立，由 RunSSE 的响应体 guard 清理，而
/// "客户端从此不开 RunSSE"时两边都不会发生。读取即续期——`wait_route`（RunSSE 附着）
/// 与 `upstream`（没有模型的 append 续用路由，心跳与工具结果都走它）都会把时刻推后，
/// 所以只有真的被丢下的标记才会过期。取值与官方转发的空闲上界同量级：一条 run 静默
/// 半小时，客户端早就不在了。
const UPSTREAM_MARK_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone)]
pub struct TransportRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    local: Mutex<HashMap<String, LocalTransport>>,
    next_local_generation: AtomicU64,
    upstream: Mutex<HashMap<String, UpstreamMark>>,
    route_changed: Notify,
    store: Store,
    traces: CursorTraceService,
    web_cache: WebCache,
    plugins: Option<PluginRegistry>,
    conversations: ConversationRegistry,
}

/// 一条 request_id 被钉在官方上游的事实。
struct UpstreamMark {
    /// 每次重新标记自增；`finish_upstream` 只清掉自己那一代。
    generation: u64,
    /// 最后一次有人用它是什么时候，见 [`UPSTREAM_MARK_TTL`]。
    last_used: Instant,
}

#[derive(Clone)]
struct LocalTransport {
    generation: u64,
    handle: TransportHandle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportRoute {
    Local,
    Upstream(u64),
}

impl TransportRegistry {
    pub fn new(store: Store, provider: Arc<dyn Provider>, compiler: PromptCompiler) -> Self {
        Self::with_web_cache(store, provider, compiler, WebCache::default())
    }

    pub fn with_web_cache(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
    ) -> Self {
        Self::build(store, provider, compiler, web_cache, None, None)
    }

    /// 附带本地 rules 目录的构造;编译请求上下文时会合并该目录下的 md 规则。
    pub fn with_local_rules(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        local_rules_dir: std::path::PathBuf,
    ) -> Self {
        Self::build(
            store,
            provider,
            compiler,
            WebCache::default(),
            None,
            Some(local_rules_dir),
        )
    }

    pub fn with_plugins(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
        plugins: PluginRegistry,
        local_rules_dir: std::path::PathBuf,
    ) -> Self {
        Self::build(
            store,
            provider,
            compiler,
            web_cache,
            Some(plugins),
            Some(local_rules_dir),
        )
    }

    fn build(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
        plugins: Option<PluginRegistry>,
        local_rules_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                local: Mutex::new(HashMap::new()),
                next_local_generation: AtomicU64::new(1),
                upstream: Mutex::new(HashMap::new()),
                route_changed: Notify::new(),
                traces: CursorTraceService::new(store.clone()),
                conversations: ConversationRegistry::new(
                    store.clone(),
                    provider,
                    compiler,
                    web_cache.clone(),
                    local_rules_dir,
                ),
                store,
                web_cache,
                plugins,
            }),
        }
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub(crate) async fn maintain_storage_if_idle(&self) -> Result<()> {
        // 残留的 upstream 标记会永久挡住回收（见 `UPSTREAM_MARK_TTL`），先让过期的走人：
        // 这一拍就是回收的入口，判定也必须在这里做，否则没有任何路径会再检查它。
        self.expire_stale_upstream().await;
        // Check admission before collecting, then release the registry locks before DB work.
        let no_transports = {
            let local = self.inner.local.lock().await;
            let upstream = self.inner.upstream.lock().await;
            local.is_empty() && upstream.is_empty()
        };
        if no_transports && !self.inner.conversations.has_active_runs().await {
            self.inner
                .store
                .prune_inactive_storage(crate::store::now_ms())
                .await?;
        }
        Ok(())
    }

    pub fn trace(
        &self,
        request_id: &str,
    ) -> crate::cursor::services::observability::CursorTraceRecorder {
        self.inner.traces.recorder(request_id)
    }

    pub fn web_cache(&self) -> &WebCache {
        &self.inner.web_cache
    }

    pub fn plugins(&self) -> Option<&PluginRegistry> {
        self.inner.plugins.as_ref()
    }

    pub fn conversations(&self) -> &ConversationRegistry {
        &self.inner.conversations
    }

    pub async fn get_or_create(&self, request_id: &str) -> Result<TransportHandle> {
        self.get_or_create_for_append(request_id, false).await
    }

    pub(crate) async fn get_or_create_for_append(
        &self,
        request_id: &str,
        replace_closing: bool,
    ) -> Result<TransportHandle> {
        let mut local = self.inner.local.lock().await;
        if let Some(transport) = local.get(request_id) {
            if transport.handle.accepting_appends() || !replace_closing {
                return Ok(transport.handle.clone());
            }
        }
        local.remove(request_id);
        let (commands, receiver) = mpsc::channel(128);
        let output = Arc::new(OutputHub::default());
        let trace = self.inner.traces.recorder(request_id);
        trace.resume();
        let handle = TransportHandle::new(request_id.into(), commands, output, trace);
        let generation = self
            .inner
            .next_local_generation
            .fetch_add(1, Ordering::Relaxed);
        local.insert(
            request_id.into(),
            LocalTransport {
                generation,
                handle: handle.clone(),
            },
        );
        drop(local);
        self.inner.route_changed.notify_waiters();
        self.inner
            .conversations
            .bind_transport(handle.clone(), receiver);

        let registry = Arc::downgrade(&self.inner);
        let request_id = request_id.to_string();
        let lifecycle = handle.clone();
        tokio::spawn(async move {
            lifecycle.wait_transport_closed().await;
            if let Some(registry) = registry.upgrade() {
                let mut local = registry.local.lock().await;
                if local
                    .get(&request_id)
                    .is_some_and(|transport| transport.generation == generation)
                {
                    local.remove(&request_id);
                }
            }
        });
        Ok(handle)
    }

    pub async fn local(&self, request_id: &str) -> Option<TransportHandle> {
        self.inner
            .local
            .lock()
            .await
            .get(request_id)
            .map(|transport| transport.handle.clone())
    }

    /// 把这条 request_id 钉在官方上游，返回这一代标记的 generation。
    ///
    /// 关闭并移除同名的本地 transport 必须与标记在同一把锁里完成：路由判定先看 local
    /// 再看 upstream（[`wait_route`](Self::wait_route)），而"本地性"是每条 append 各自
    /// 按模型重算的。同一个 request_id 上"先走库内模型、随后又来一条库外模型"时，如果
    /// 本地 transport 还留在 map 里，这条 run 的 RunSSE 会绑到本地 hub，而它的 RunRequest
    /// 已经被转发给官方：客户端既收不到帧也收不到 END_STREAM，只能自己超时。
    ///
    /// 锁顺序与 `maintain_storage_if_idle` 一致：先 local 再 upstream。
    pub async fn mark_upstream(&self, request_id: &str) -> u64 {
        let (dropped, generation) = {
            let mut local = self.inner.local.lock().await;
            let mut upstream = self.inner.upstream.lock().await;
            let generation = upstream
                .get(request_id)
                .map_or(1, |mark| mark.generation + 1);
            upstream.insert(
                request_id.into(),
                UpstreamMark {
                    generation,
                    last_used: Instant::now(),
                },
            );
            let dropped = local.remove(request_id).map(|transport| transport.handle);
            (dropped, generation)
        };
        if let Some(handle) = dropped {
            tracing::warn!(
                request_id,
                "this request now runs on the Cursor upstream; dropping its local transport"
            );
            handle.close_transport();
            // 不 await：命令队列满时这里会等到 actor 腾出位置，而这是 BidiAppend 的
            // 处理路径。断开是收尾动作，不该拦住这次转发。
            tokio::spawn(async move {
                handle.disconnect().await;
            });
        }
        self.inner.route_changed.notify_waiters();
        generation
    }

    /// 这条 request_id 此刻是否被钉在官方上游。读取即续期：没有模型的 append（工具结果、
    /// 心跳、KV 应答）走这里延续路由，有 run 在跑的 id 因此不会过期。
    pub async fn upstream(&self, request_id: &str) -> bool {
        let mut upstream = self.inner.upstream.lock().await;
        match upstream.get_mut(request_id) {
            Some(mark) => {
                mark.last_used = Instant::now();
                true
            }
            None => false,
        }
    }

    /// 清掉没人再用的 upstream 标记，返回清掉的条数。
    ///
    /// 门禁 `upstream.is_empty()` 一旦被残留标记卡住，`prune_inactive_storage` 就永久
    /// 不再执行（数据库与 blob 无界增长），而标记没有天然终点，所以只能按"多久没人碰"
    /// 过期。清掉之后路由会退回本地判定：下一条带模型的 append 会重新钉一次。
    async fn expire_stale_upstream(&self) -> usize {
        self.expire_stale_upstream_at(Instant::now()).await
    }

    async fn expire_stale_upstream_at(&self, now: Instant) -> usize {
        let mut upstream = self.inner.upstream.lock().await;
        let stale: Vec<String> = upstream
            .iter()
            .filter(|(_, mark)| now.saturating_duration_since(mark.last_used) >= UPSTREAM_MARK_TTL)
            .map(|(request_id, _)| request_id.clone())
            .collect();
        for request_id in &stale {
            upstream.remove(request_id);
            tracing::warn!(
                request_id,
                ttl = ?UPSTREAM_MARK_TTL,
                "dropping a Cursor upstream route mark nobody used recently"
            );
        }
        stale.len()
    }

    /// 等这条 request_id 的路由落地。等待有上界:同一个 request_id 在上一轮用完之后
    /// 条目会被回收(`Closed` 后删除),客户端若在这个 id 上重发 RunSSE,就再没有任何
    /// 生产者会唤醒等待者——不设上界时这条连接永久挂着,连关服都要吃满 10 秒预算。
    pub async fn wait_route(&self, request_id: &str) -> Result<TransportRoute> {
        tokio::time::timeout(ROUTE_WAIT_TIMEOUT, async {
            loop {
                let changed = self.inner.route_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.inner.local.lock().await.contains_key(request_id) {
                    return TransportRoute::Local;
                }
                // 附着到官方路由也算"有人还在用这个标记"：续期，别让一条正在被消费的
                // 官方 run 因为静默而半路过期。
                {
                    let mut upstream = self.inner.upstream.lock().await;
                    if let Some(mark) = upstream.get_mut(request_id) {
                        mark.last_used = Instant::now();
                        return TransportRoute::Upstream(mark.generation);
                    }
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| {
            Error::RunNotFound(format!(
                "no transport for request {request_id} within {ROUTE_WAIT_TIMEOUT:?}"
            ))
        })
    }

    pub fn finish_upstream(&self, request_id: String, generation: u64) {
        let registry = self.clone();
        tokio::spawn(async move {
            let mut upstream = registry.inner.upstream.lock().await;
            if upstream
                .get(&request_id)
                .is_some_and(|mark| mark.generation == generation)
            {
                upstream.remove(&request_id);
            }
        });
    }

    pub async fn shutdown(&self) {
        self.inner.conversations.shutdown().await;
        let handles = std::mem::take(&mut *self.inner.local.lock().await);
        self.inner.upstream.lock().await.clear();
        for transport in handles.into_values() {
            transport.handle.disconnect().await;
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                transport.handle.wait_transport_closed(),
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::transport::test_registry;

    /// 同一条 request_id 上先有本地 transport、随后这条 append 选了库外模型：路由只能有
    /// 一个答案。以前 `mark_upstream` 不碰本地 transport，而 `wait_route` 先看 local，
    /// 于是这条 run 的 RunSSE 绑在一个永远收不到 RunRequest 的本地 hub 上——客户端既收
    /// 不到帧也收不到 END_STREAM，一直转圈到自身超时。
    #[tokio::test]
    async fn marking_a_route_upstream_retires_the_local_transport() {
        let (_directory, registry) = test_registry().await;
        let handle = registry.get_or_create("switched-model").await.unwrap();
        assert!(registry.local("switched-model").await.is_some());

        assert_eq!(registry.mark_upstream("switched-model").await, 1);
        assert!(
            registry.local("switched-model").await.is_none(),
            "本地 transport 必须让位,否则 RunSSE 会绑到它上面"
        );
        assert_eq!(
            registry.wait_route("switched-model").await.unwrap(),
            TransportRoute::Upstream(1),
            "两侧对同一条 request_id 的判定必须一致"
        );
        tokio::time::timeout(Duration::from_secs(5), handle.wait_transport_closed())
            .await
            .expect("被顶掉的本地 transport 必须收尾");
    }

    /// 残留的 upstream 标记不能永久挡住存储回收。标记由带模型的 BidiAppend 建立、由
    /// RunSSE 的响应体 guard 清理，"客户端从此不开 RunSSE"时两边都不发生，于是
    /// `maintain_storage_if_idle` 的 `upstream.is_empty()` 门禁永久失效——数据库与 blob
    /// 再也不会被回收。维护循环必须让没人用的标记过期。
    #[tokio::test]
    async fn a_stale_upstream_mark_is_expired_by_maintenance() {
        let (_directory, registry) = test_registry().await;
        assert_eq!(registry.mark_upstream("abandoned").await, 1);
        assert!(registry.upstream("abandoned").await, "刚标记的路由还在");
        assert_eq!(
            registry.expire_stale_upstream().await,
            0,
            "有人刚碰过的标记不许被清掉"
        );

        // 维护循环那一拍：把时钟推过 TTL（读取会续期,所以多推一秒）。
        let now = Instant::now() + UPSTREAM_MARK_TTL + Duration::from_secs(1);
        assert_eq!(registry.expire_stale_upstream_at(now).await, 1);
        assert!(!registry.upstream("abandoned").await);

        // 门禁不再被这个标记挡住：维护照常执行。
        registry.maintain_storage_if_idle().await.unwrap();
    }
}
