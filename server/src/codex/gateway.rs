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
}
struct Running {
    port: u16,
    shutdown: CancellationToken,
    alive: Arc<AtomicBool>,
    task: JoinHandle<()>,
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
            }),
        }
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
            *runtime = Some(Running {
                port: settings.port,
                shutdown,
                alive,
                task,
            });
        }
        self.record_error(None);
        Ok(())
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
        let (configured, conflict, config_error) = match inspected {
            Ok(status) => (status.applied, status.conflict, None),
            Err(error) => (false, true, Some(error.to_string())),
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
        let default = settings
            .default_model_hash
            .as_ref()
            .and_then(|id| models.iter().find(|m| &m.model_hash == id))
            .ok_or_else(|| Error::Config("select a default Codex model".into()))?;
        let catalog = super::catalog::build_catalog(&models)?;
        let catalog_path = config::write_catalog(&home, &serde_json::to_vec_pretty(&catalog)?)?;
        config::apply(
            &home,
            &ConfigRoute {
                model: default.model_hash.clone(),
                base_url: format!("http://127.0.0.1:{}/v1", settings.port),
                token: self.inner.store.codex_token().await?,
                catalog_path,
                context_window: default
                    .context_window_tokens
                    .ok_or_else(|| Error::Config("configure the model context window".into()))?,
            },
        )?;
        self.status().await
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
    let expected = match gateway.inner.store.codex_token().await {
        Ok(token) => token,
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
    let operation = gateway.inner.operations.lock().await;
    let settings = match gateway.inner.store.codex_settings().await {
        Ok(s) => s,
        Err(error) => return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    if !settings.enabled {
        return protocol_error(StatusCode::SERVICE_UNAVAILABLE, "Codex gateway is disabled");
    }
    let bytes = match to_bytes(request.into_body(), 24 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return protocol_error(StatusCode::PAYLOAD_TOO_LARGE, "Codex request is too large")
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
    let ownership = (|| -> Result<()> {
        if let Some(catalog) = config::applied_catalog(&settings.resolved_home()?)? {
            let saved = catalog
                .get("models")
                .and_then(serde_json::Value::as_array)
                .and_then(|models| models.iter().find(|entry| entry["slug"] == id));
            let fingerprint = super::catalog::routing_fingerprint(&model)?;
            if saved
                .and_then(|entry| entry.get("haxsd_byok_routing_fingerprint"))
                .and_then(serde_json::Value::as_str)
                != Some(fingerprint.as_str())
            {
                return Err(Error::Config("Codex model configuration changed after apply; restore, reapply and start a new conversation".into()));
            }
        }
        Ok(())
    })();
    if let Err(error) = ownership {
        return protocol_error(StatusCode::CONFLICT, error.to_string());
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
