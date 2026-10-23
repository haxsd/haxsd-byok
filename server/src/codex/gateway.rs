//! Codex 独立回环监听与配置生命周期；读状态不写用户文件。
use super::{
    config::{self, ConfigRoute},
    CodexSettings, CodexStatus,
};
use crate::{network::NetworkClients, store::Store, Error, Result};
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct CodexGateway {
    inner: Arc<Inner>,
}
struct Inner {
    store: Store,
    clients: NetworkClients,
    runtime: Mutex<Option<Running>>,
    operations: Mutex<()>,
    last_error: std::sync::Mutex<Option<String>>,
    active: AtomicUsize,
    request_timeout: Duration,
    idle_timeout: Duration,
    /// 网关令牌在库里只生成一次；`store.codex_token()` 每次都要拿全局写锁，而
    /// 校验请求只需要读它，所以取到之后常驻内存。
    token: tokio::sync::OnceCell<String>,
}

/// 客户端正文的体积上限，以及从连接上读完它的时限。
///
/// 读体发生在全局配置锁之外（见 `responses`），所以时限是唯一的兜底：没有它，
/// 一个连上却不发正文的客户端会让这次请求一直挂到连接消失。
const MAX_REQUEST_BODY: usize = 24 * 1024 * 1024;
const MAX_REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);
struct Running {
    port: u16,
    shutdown: CancellationToken,
    alive: Arc<AtomicBool>,
    task: JoinHandle<()>,
    /// 未登录时把客户端保持在本地界面：Codex 自己的 `conversationDetailMode` 被切到
    /// Work 模式后，它会在创建本地任务前就拒绝请求。轮询得很轻（2s 一次，只在需要时写）。
    client_guard: JoinHandle<()>,
}

/// 跨流的活动计数；响应 Body 被客户端丢弃也释放所有权。
pub(super) struct ActiveRequest {
    gateway: CodexGateway,
}
impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.gateway.inner.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CodexGateway {
    pub fn new(store: Store, clients: NetworkClients) -> Self {
        Self::with_timeouts(
            store,
            clients,
            Duration::from_secs(3600),
            Duration::from_secs(1800),
        )
    }

    pub fn with_timeouts(
        store: Store,
        clients: NetworkClients,
        request_timeout: Duration,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                clients,
                runtime: Mutex::new(None),
                operations: Mutex::new(()),
                last_error: std::sync::Mutex::new(None),
                active: AtomicUsize::new(0),
                request_timeout,
                idle_timeout,
                token: tokio::sync::OnceCell::new(),
            }),
        }
    }

    /// 校验用的网关令牌：第一次从库里取（可能生成），之后直接读缓存。
    async fn token(&self) -> Result<&str> {
        self.inner
            .token
            .get_or_try_init(|| async { self.inner.store.codex_token().await })
            .await
            .map(String::as_str)
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route(
                "/health",
                get(|| async { Json(serde_json::json!({"ok":true,"service":"codex"})) }),
            )
            .route("/v1/responses", post(responses))
            .with_state(self.clone())
    }

    fn record_error(&self, error: Option<String>) {
        *self
            .inner
            .last_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = error;
    }

    pub async fn start(&self) -> Result<()> {
        let _operation = self.inner.operations.lock().await;
        let settings = self.inner.store.codex_settings().await?;
        // 旧版本把默认模型的窗口写成全局覆盖；启动时清一次，客户端界面不必等到
        // 下一次模型编辑或重新应用才显示逐模型窗口。
        if let Ok(home) = settings.resolved_home() {
            if config::inspect(&home).is_ok_and(|status| status.applied) {
                if let Err(error) = config::drop_global_limits(&home) {
                    tracing::warn!(%error, "Codex global context limit cleanup failed");
                }
            }
        }
        self.sync_listener(&settings).await
    }

    async fn sync_listener(&self, settings: &CodexSettings) -> Result<()> {
        let mut runtime = self.inner.runtime.lock().await;
        if settings.enabled
            && runtime
                .as_ref()
                .is_some_and(|r| r.port == settings.port && r.alive.load(Ordering::SeqCst))
        {
            return Ok(());
        }
        // 先绑定新端口，失败时旧监听仍可服务，配置也没有被写入。
        let listener = if settings.enabled {
            match TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], settings.port))).await {
                Ok(listener) => Some(listener),
                Err(error) => {
                    self.record_error(Some(error.to_string()));
                    return Err(error.into());
                }
            }
        } else {
            None
        };
        if let Some(previous) = runtime.take() {
            previous.shutdown.cancel();
            previous.client_guard.abort();
            let _ = previous.client_guard.await;
            let mut task = previous.task;
            match tokio::time::timeout(Duration::from_secs(10), &mut task).await {
                Ok(_) => {}
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    tracing::warn!("Codex listener shutdown timed out");
                }
            }
        }
        if let Some(listener) = listener {
            let shutdown = CancellationToken::new();
            let graceful = shutdown.clone();
            let alive = Arc::new(AtomicBool::new(true));
            let serving = alive.clone();
            let router = self.router();
            let task = tokio::spawn(async move {
                if let Err(error) = axum::serve(listener, router)
                    .with_graceful_shutdown(graceful.cancelled_owned())
                    .await
                {
                    tracing::error!(%error, "Codex listener stopped");
                }
                serving.store(false, Ordering::SeqCst);
            });
            let guard_shutdown = shutdown.clone();
            let guard_home = settings.resolved_home().ok();
            let guard_gateway = self.clone();
            let guard_alive = alive.clone();
            // 用 try_lock 与 apply/restore/shutdown 互斥：配置生命周期操作持锁时跳过本轮，
            // 绝不阻塞等待（否则 shutdown 持锁等 guard、guard 等锁会互相卡死）。
            let client_guard = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(2));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = guard_shutdown.cancelled() => break,
                        _ = ticker.tick() => {
                            if !guard_alive.load(Ordering::SeqCst) {
                                break;
                            }
                            let Some(home) = guard_home.as_deref() else {
                                continue;
                            };
                            let Ok(_operation) = guard_gateway.inner.operations.try_lock() else {
                                continue;
                            };
                            if let Err(error) = config::ensure_local_mode(home) {
                                tracing::warn!(%error, "Codex client mode guard failed");
                            }
                        }
                    }
                }
            });
            *runtime = Some(Running {
                port: settings.port,
                shutdown,
                alive,
                task,
                client_guard,
            });
        }
        self.record_error(None);
        Ok(())
    }

    /// 当前正在监听的端口（自己有监听器时才有值）。
    ///
    /// 保存端口时它算"我们自己的监听器"，不能被当成"被别的程序占用"——否则把同一个端口
    /// 再存一次都会报冲突。与 `status()` 判定 `listening` 的口径一致。
    pub async fn listening_port(&self) -> Option<u16> {
        let runtime = self.inner.runtime.lock().await;
        runtime
            .as_ref()
            .filter(|running| running.alive.load(Ordering::SeqCst))
            .map(|running| running.port)
    }

    pub async fn save_settings(&self, settings: CodexSettings) -> Result<CodexSettings> {
        let _operation = self.inner.operations.lock().await;
        settings.validate()?;
        if self.inner.active.load(Ordering::SeqCst) != 0 {
            return Err(Error::Config(
                "Codex has active requests; wait before changing its configuration".into(),
            ));
        }
        let previous = self.inner.store.codex_settings().await?;
        if settings != previous && config::inspect(&previous.resolved_home()?)?.applied {
            return Err(Error::Config(
                "restore the applied Codex configuration before changing gateway settings".into(),
            ));
        }
        // 校验持久化后尝试运行；启动失败回滚设置，旧监听保持运行。
        let saved = self.inner.store.set_codex_settings(settings).await?;
        if let Err(error) = self.sync_listener(&saved).await {
            self.inner.store.set_codex_settings(previous).await?;
            return Err(error);
        }
        Ok(saved)
    }

    pub async fn status(&self) -> Result<CodexStatus> {
        let settings = self.inner.store.codex_settings().await?;
        let home = settings.resolved_home()?;
        let inspected = config::inspect(&home);
        let (
            configured,
            conflict,
            restorable,
            conflicts,
            chatgpt_signed_in,
            conversation_detail_mode,
            local_mode_guard,
            config_error,
        ) = match inspected {
            Ok(status) => (
                status.applied,
                status.conflict,
                status.restorable,
                status.conflicts,
                status.chatgpt_signed_in,
                status.conversation_detail_mode,
                status.local_mode_guard,
                None,
            ),
            Err(error) => (
                false,
                true,
                false,
                Vec::new(),
                false,
                None,
                false,
                Some(error.to_string()),
            ),
        };
        let runtime = self.inner.runtime.lock().await;
        let listening = runtime
            .as_ref()
            .is_some_and(|r| r.port == settings.port && r.alive.load(Ordering::SeqCst));
        let calls: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM llm_calls WHERE client_source = 'codex'")
                .fetch_one(self.inner.store.pool())
                .await?;
        Ok(CodexStatus {
            enabled: settings.enabled,
            listening,
            port: settings.port,
            configured,
            config_conflict: conflict,
            restorable,
            conflicts,
            chatgpt_signed_in,
            conversation_detail_mode,
            local_mode_guard,
            home: home.display().to_string(),
            calls: calls.max(0) as usize,
            last_error: config_error.or_else(|| {
                self.inner
                    .last_error
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
            }),
        })
    }

    pub async fn apply(&self) -> Result<CodexStatus> {
        let _operation = self.inner.operations.lock().await;
        let settings = self.inner.store.codex_settings().await?;
        if !settings.enabled {
            return Err(Error::Config(
                "enable the Codex gateway before applying configuration".into(),
            ));
        }
        if self.inner.active.load(Ordering::SeqCst) != 0 {
            return Err(Error::Config(
                "Codex has active requests; wait before applying configuration".into(),
            ));
        }
        let home = settings.resolved_home()?;
        if config::inspect(&home)?.conflict {
            return Err(Error::Config(
                "Codex routing conflicts with external configuration".into(),
            ));
        }
        self.sync_listener(&settings).await?;
        // 目录正文与自愈发布共用同一份序列化，两边不会漂移。
        let catalog = self.catalog_bytes(&settings).await?;
        let default = settings
            .default_model_hash
            .as_ref()
            .filter(|id| settings.model_hashes.contains(id))
            .ok_or_else(|| Error::Config("select a default Codex model".into()))?;
        let default = self
            .inner
            .store
            .model(default)
            .await?
            .ok_or_else(|| Error::Config(format!("unknown Codex model: {default}")))?;
        let catalog_path = config::write_catalog(&home, &catalog)?;
        config::apply(
            &home,
            &ConfigRoute {
                model: default.model_hash.clone(),
                base_url: format!("http://127.0.0.1:{}/v1", settings.port),
                token: self.inner.store.codex_token().await?,
                catalog_path,
            },
        )?;
        self.status().await
    }

    /// 本次发布选中的模型 + 每个模型当前的路由指纹；`apply` 与请求路径上的目录
    /// 自愈共用它，保证两边写出的目录是同一份东西。
    async fn catalog_bytes(&self, settings: &CodexSettings) -> Result<Vec<u8>> {
        let mut models = Vec::new();
        for id in &settings.model_hashes {
            models.push(
                self.inner
                    .store
                    .model(id)
                    .await?
                    .ok_or_else(|| Error::Config(format!("unknown Codex model: {id}")))?,
            );
        }
        Ok(serde_json::to_vec_pretty(&super::catalog::build_catalog(
            &models,
        )?)?)
    }

    /// 目录是写给客户端的快照，模型被编辑后它会过期。让用户为一次正常编辑付出
    /// 整条链路不可用的代价不对：请求本来就走模型库里的最新配置，这里把目录刷新
    /// 到最新，客户端下一次会话会读到它。
    ///
    /// 比较的是**整份目录**（新组装的 vs 已发布的），不是单个路由指纹：指纹只覆盖路由
    /// 相关字段，改 `display_name` 这类目录可见字段不会让它变化，于是客户端永远显示旧名字。
    ///
    /// 调用方必须持有 `operations` 锁：`config::republish_catalog` 会重写收据，而
    /// `apply` / `restore` / `save_settings` 同样在锁内读写收据——两边并发就会互相覆盖。
    /// 这里不自己取锁（调用方已持有），锁的作用是"每次请求与配置生命周期操作互斥"，
    /// 不是"与无活跃请求前提互斥"。
    ///
    /// 读写都是同步文件 IO，放到阻塞线程池：这条路径每次请求都会走，不能占住
    /// async 工作线程。旧版本全局窗口的清理不在这里——请求路径不做强制修改，
    /// 它留在 `start` / `apply`。
    async fn publish_current_catalog(
        &self,
        home: &std::path::Path,
        settings: &CodexSettings,
    ) -> Result<()> {
        let path = home.to_path_buf();
        let applied = tokio::task::spawn_blocking({
            let path = path.clone();
            move || config::applied_catalog(&path)
        })
        .await
        .map_err(|error| Error::Store(format!("catalog read task failed: {error}")))??;
        let Some(catalog) = applied else {
            return Ok(());
        };
        let fresh: serde_json::Value =
            serde_json::from_slice(&self.catalog_bytes(settings).await?)?;
        if fresh == catalog {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&fresh)?;
        tokio::task::spawn_blocking(move || config::republish_catalog(&path, &bytes))
            .await
            .map_err(|error| Error::Store(format!("catalog republish task failed: {error}")))??;
        Ok(())
    }

    pub async fn restore(&self) -> Result<CodexStatus> {
        let _operation = self.inner.operations.lock().await;
        if self.inner.active.load(Ordering::SeqCst) != 0 {
            return Err(Error::Config(
                "Codex has active requests; wait before restoring configuration".into(),
            ));
        }
        let settings = self.inner.store.codex_settings().await?;
        config::restore(&settings.resolved_home()?)?;
        self.status().await
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _operation = self.inner.operations.lock().await;
        let settings = self.inner.store.codex_settings().await?;
        let restored = match config::inspect(&settings.resolved_home()?) {
            Ok(status) if status.applied => config::restore(&settings.resolved_home()?).map(|_| ()),
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        };
        let disabled = CodexSettings {
            enabled: false,
            ..settings
        };
        self.sync_listener(&disabled).await?;
        restored
    }
}

async fn responses(State(gateway): State<CodexGateway>, request: Request<Body>) -> Response {
    let expected = match gateway.token().await {
        Ok(token) => token.to_owned(),
        Err(error) => return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if authorization != format!("Bearer {expected}") {
        return protocol_error(StatusCode::UNAUTHORIZED, "invalid Codex gateway token");
    }
    // 正文必须在取全局配置锁之前读完：那把锁的持有者是配置生命周期操作
    // （apply / save_settings / restore / shutdown），让一个慢客户端在锁里等正文，
    // 等于让配置操作和退出收回一起等它。读体本身也要有时限，否则一个连上却不发
    // 正文的客户端会把这次请求永久挂住。
    let bytes = match tokio::time::timeout(
        MAX_REQUEST_BODY_TIMEOUT,
        to_bytes(request.into_body(), MAX_REQUEST_BODY),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return protocol_error(StatusCode::PAYLOAD_TOO_LARGE, "Codex request is too large")
        }
        Err(_) => {
            return protocol_error(
                StatusCode::REQUEST_TIMEOUT,
                "Codex request body was not received in time",
            )
        }
    };
    let body: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(error) => return protocol_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let id = body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let operation = gateway.inner.operations.lock().await;
    let settings = match gateway.inner.store.codex_settings().await {
        Ok(s) => s,
        Err(error) => return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    if !settings.enabled {
        return protocol_error(StatusCode::SERVICE_UNAVAILABLE, "Codex gateway is disabled");
    }
    if !settings.model_hashes.iter().any(|selected| selected == id) {
        return protocol_error(
            StatusCode::BAD_REQUEST,
            "Codex requested a model that is not enabled",
        );
    }
    let model = match gateway.inner.store.model(id).await {
        Ok(Some(model)) => model,
        Ok(None) => return protocol_error(StatusCode::NOT_FOUND, "Codex model no longer exists"),
        Err(error) => return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    // `home` 解析不出来是环境问题（没有可用的用户主目录），不是"配置冲突"：以前它被折进
    // 下面那条 409 里，用户看到的是"去处理配置冲突"，而真正该做的是显式填一个 home。
    let home = match settings.resolved_home() {
        Ok(home) => home,
        Err(error) => return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    // 目录过期（模型在 apply 之后被编辑）不是错误：把目录刷新成当前配置后继续
    // 服务。刷新失败也只留 warn——请求用的是模型库里的最新配置，目录只影响客户端
    // 下一次会话；让一次旁路自愈失败把整轮请求变成 409 并不划算。
    if let Err(error) = gateway.publish_current_catalog(&home, &settings).await {
        tracing::warn!(
            error = %crate::diagnostics::error_chain(&error),
            "Codex model catalog refresh failed; serving with the model store"
        );
    }
    gateway.inner.active.fetch_add(1, Ordering::SeqCst);
    let active = ActiveRequest {
        gateway: gateway.clone(),
    };
    let cancellation = gateway
        .inner
        .runtime
        .lock()
        .await
        .as_ref()
        .map(|r| r.shutdown.clone())
        .unwrap_or_default();
    drop(operation);
    super::upstream::respond(
        gateway.inner.store.clone(),
        gateway.inner.clients.clone(),
        super::upstream::RequestContext {
            model,
            source: body,
            timeout: gateway.inner.request_timeout,
            idle: gateway.inner.idle_timeout,
            cancellation,
        },
        active,
    )
    .await
}

pub(super) fn protocol_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(
            serde_json::json!({"error":{"type":"invalid_request_error","message":message.into()}}),
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelConfigInput, ModelType};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// 一个只连上、不发正文的客户端：第一次被读时通知测试，然后永远 Pending，
    /// 也不注册唤醒——正是"连接还在，正文不再前进"的形态。
    struct StalledBody {
        started: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl futures_util::Stream for StalledBody {
        type Item = std::result::Result<axum::body::Bytes, std::io::Error>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            Poll::Pending
        }
    }

    async fn enabled_gateway() -> (tempfile::TempDir, Store, CodexGateway, String) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let model = store
            .create_model(&ModelConfigInput {
                sort_order: 0,
                display_name: "Codex lock test model".into(),
                group_name: None,
                model_type: ModelType::OpenAi,
                base_url: "http://127.0.0.1:1".into(),
                use_full_url: true,
                api_key: "vendor-secret".into(),
                tooltip_data: "Codex lock test".into(),
                model_id: "vendor-model".into(),
                reasoning_effort: None,
                openai_endpoint: "/v1/responses".into(),
                openai_extra_params_enabled: false,
                openai_extra_params: serde_json::json!({}),
                custom_headers_enabled: false,
                custom_headers: serde_json::json!({}),
                anthropic_extra_params_enabled: false,
                anthropic_extra_params: serde_json::json!({}),
                context_window_tokens: Some(32_768),
                max_completion_tokens: Some(4_096),
                anthropic_max_tokens: None,
                anthropic_thinking_effort: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        store
            .set_codex_settings(CodexSettings {
                enabled: true,
                port,
                model_hashes: vec![model.model_hash.clone()],
                default_model_hash: Some(model.model_hash.clone()),
                ..CodexSettings::default()
            })
            .await
            .unwrap();
        let token = store.codex_token().await.unwrap();
        let gateway = CodexGateway::new(store.clone(), NetworkClients::new(store.clone()));
        (directory, store, gateway, token)
    }

    /// 准入段读客户端正文时不能握着全局配置锁：慢客户端卡住的是它自己的连接，不能
    /// 连带卡住配置操作与退出收回（`shutdown` 抢的就是同一把锁）。
    #[tokio::test]
    async fn a_stalled_request_body_does_not_hold_the_configuration_lock() {
        let (_directory, _store, gateway, token) = enabled_gateway().await;
        let (started, reading) = tokio::sync::oneshot::channel();
        let request = Request::post("/v1/responses")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from_stream(StalledBody {
                started: Some(started),
            }))
            .unwrap();
        let router = gateway.router();
        let stalled = tokio::spawn(async move {
            use tower::ServiceExt;
            router.oneshot(request).await
        });
        // 读到正文了才说明请求已经进入准入段：此刻配置操作必须还能拿到锁。
        reading.await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), gateway.start())
            .await
            .expect("配置操作被一个读不完正文的客户端挡在锁外")
            .unwrap();

        stalled.abort();
    }

    /// 只改模型显示名（slug 是 `model_hash`，改名不动它）也必须刷新已发布的目录，
    /// 否则 Codex 的模型列表里永远是旧名字。路由指纹只覆盖路由字段，看它会漏掉这次编辑，
    /// 所以要比较整份目录。
    #[tokio::test]
    async fn renaming_a_model_republishes_the_catalog() {
        let (directory, store, gateway, _token) = enabled_gateway().await;
        let mut settings = store.codex_settings().await.unwrap();
        settings.home = directory.path().join("home").display().to_string();
        std::fs::create_dir_all(&settings.home).unwrap();
        store.set_codex_settings(settings.clone()).await.unwrap();
        gateway.apply().await.unwrap();
        let home = std::path::PathBuf::from(&settings.home);
        let before = config::applied_catalog(&home).unwrap().unwrap();

        let model = store
            .model(&settings.model_hashes[0])
            .await
            .unwrap()
            .unwrap();
        let renamed = store
            .update_model(
                &model.model_hash,
                &model.config_input("Renamed Model".into(), model.sort_order),
            )
            .await
            .unwrap();
        assert_eq!(renamed.model_hash, model.model_hash, "改名不动 slug");

        gateway
            .publish_current_catalog(&home, &settings)
            .await
            .unwrap();

        let after = config::applied_catalog(&home).unwrap().unwrap();
        assert_eq!(
            after["models"][0]["display_name"], "Renamed Model",
            "改名后目录没刷新"
        );
        assert_ne!(
            after["models"][0]["display_name"],
            before["models"][0]["display_name"]
        );
    }

    /// 网关令牌在库里只生成一次：`store.codex_token()` 每次都要拿全局写锁，而校验只需要
    /// 读它，所以第一次取到之后常驻内存（`token()` 里那枚 `OnceCell`）。
    #[tokio::test]
    async fn the_gateway_token_is_read_from_the_store_once() {
        let (_directory, _store, gateway, token) = enabled_gateway().await;
        assert!(gateway.inner.token.get().is_none(), "还没用过就不该有缓存");

        assert_eq!(gateway.token().await.unwrap(), token);
        assert_eq!(
            gateway.inner.token.get().map(String::as_str),
            Some(token.as_str())
        );
        assert_eq!(gateway.token().await.unwrap(), token);
    }
}
