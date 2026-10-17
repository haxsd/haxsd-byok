//! Orchestrates plugin capabilities: resources, model catalogs, and invocation.
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use async_stream::try_stream;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use super::{
    catalog::{PluginCatalog, PluginEntry},
    data::PluginDataStore,
    descriptor::{
        parse_model_id, PluginDescriptor, PluginModelDescriptor, PluginProviderDescriptor,
        PluginResourceDescriptor, PluginResourceView, ProviderDefinition, ResourceActionResponse,
        ResourceActionResult, ResourceDefinition, ResourcePresentation, OAUTH2_ADD_METHOD,
        OAUTH2_AUTHORIZATION_CODE_ADD_METHOD,
    },
    oauth_callback::{self, CallbackHandle, CallbackOutcome, CallbackRequest},
    runtime::PluginRuntime,
    state::{now_ms, PluginStateStore, ResourceDraft, ResourcePatch, ResourceRecord, StoredModel},
    wire,
    worker::{PluginWorker, WorkerStreamItem},
};
use crate::{
    model::ModelInvocation,
    provider::ProviderStream,
    provider::{CallRecorder, ModelEvent},
    store::Store,
    Error, Result,
};

const OAUTH_SLOW_DOWN_STEP_MS: i64 = 5_000;
const MAX_IMPORT_DRAFTS: usize = 256;
/// OAuth 轮询间隔的上下限：它完全由插件给（`pollIntervalMs`），不夹取就是让插件决定
/// 会话多久被问一次——超大的值等于永不轮询，超小的值会把上游打成限流。
const OAUTH_POLL_MIN_MS: i64 = 1_000;
const OAUTH_POLL_MAX_MS: i64 = 300_000;
/// 单个 OAuth 会话的最长存活时间。插件给的 `expiresAtMs` 同样不可信：一个远期时间戳
/// 会让会话与授权码回调监听一直留着。
const OAUTH_SESSION_MAX_MS: i64 = 30 * 60 * 1_000;

#[derive(Clone)]
pub struct PluginRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    store: Store,
    runtime: PluginRuntime,
    catalog: PluginCatalog,
    state: PluginStateStore,
    entries: RwLock<Option<CachedEntries>>,
    workers: Mutex<HashMap<String, Arc<PluginWorker>>>,
    oauth_sessions: Mutex<HashMap<String, OAuthSession>>,
    /// 串行化 `oauth_begin`：它中间要等插件求值、又要绑 loopback 监听，两个并发 begin
    /// 会各自插进一个会话（授权码流程还会多一个监听）。
    oauth_begin: Mutex<()>,
}

/// 求值过的插件表 + 它对应的目录指纹（见 `PluginCatalog::fingerprint`）。
struct CachedEntries {
    fingerprint: String,
    entries: Vec<PluginEntry>,
}

struct OAuthSession {
    plugin_id: String,
    resource_type: String,
    method_id: String,
    session: serde_json::Value,
    expires_at_ms: i64,
    poll_interval_ms: i64,
    next_poll_at_ms: i64,
    flow: OAuthFlow,
}

enum OAuthFlow {
    DeviceCode,
    AuthorizationCode {
        redirect_uri: String,
        code_verifier: String,
        callback: CallbackHandle,
    },
}

enum OAuthPollWork {
    DeviceCode {
        plugin_id: String,
        resource_type: String,
        method_id: String,
        session: serde_json::Value,
        poll_interval_ms: i64,
    },
    AuthorizationCode {
        plugin_id: String,
        resource_type: String,
        method_id: String,
        session: serde_json::Value,
        redirect_uri: String,
        code_verifier: String,
        callback_request: CallbackRequest,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthBeginResponse {
    pub session_id: String,
    pub user_code: Option<String>,
    pub verification_url: String,
    pub verification_url_complete: Option<String>,
    pub expires_at_ms: i64,
    pub poll_interval_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum OAuthPollResponse {
    #[serde(rename_all = "camelCase")]
    Pending { poll_interval_ms: i64 },
    #[serde(rename_all = "camelCase")]
    Completed {
        added: usize,
        updated: usize,
        model_sync_error: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Denied { message: Option<String> },
    #[serde(rename_all = "camelCase")]
    Failed { message: String },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResponse {
    pub added: usize,
    pub updated: usize,
    pub warnings: Vec<String>,
    pub model_sync_error: Option<String>,
}

/// 路由分支在建立 Recorder 时需要的插件模型元数据。
#[derive(Clone, Debug)]
pub struct PluginInvocationPlan {
    pub model: PluginModelDescriptor,
    pub request_url: String,
}

impl PluginRegistry {
    pub fn managed(store: Store, runtime: PluginRuntime, app_version: String) -> Result<Self> {
        let data = PluginDataStore::managed()?;
        Ok(Self {
            inner: Arc::new(RegistryInner {
                store,
                runtime,
                catalog: PluginCatalog::managed(app_version)?,
                state: PluginStateStore::new(data),
                entries: RwLock::new(None),
                workers: Mutex::new(HashMap::new()),
                oauth_sessions: Mutex::new(HashMap::new()),
                oauth_begin: Mutex::new(()),
            }),
        })
    }

    pub async fn plugins(&self) -> Vec<PluginDescriptor> {
        let Some(executable) = self.inner.runtime.executable() else {
            return self
                .inner
                .catalog
                .manifests()
                .into_iter()
                .map(|(manifest, icon)| PluginDescriptor {
                    id: manifest.id,
                    name: manifest.name,
                    version: manifest.version,
                    author: manifest.author,
                    icon,
                    providers: Vec::new(),
                    resources: Vec::new(),
                })
                .collect();
        };
        let mut plugins = Vec::new();
        for entry in self.entries(&executable).await {
            plugins.push(self.descriptor(&entry, &executable).await);
        }
        plugins
    }

    /// 已满足调用条件的全部插件模型;每个模型独立进入 Cursor 目录。
    pub async fn configured_models(&self) -> Vec<PluginModelDescriptor> {
        let Some(executable) = self.inner.runtime.executable() else {
            return Vec::new();
        };
        let mut models = Vec::new();
        for entry in self.entries(&executable).await {
            for provider in &entry.definition.providers {
                if !self.provider_configured(&entry, provider).await {
                    continue;
                }
                let stored = Self::state_or_empty(
                    &entry.manifest.id,
                    "models",
                    self.inner
                        .state
                        .models(&entry.manifest.id, &provider.id)
                        .await,
                );
                models.extend(stored.iter().filter(|model| model.enabled).map(|model| {
                    PluginModelDescriptor::new(
                        &entry.manifest.id,
                        &entry.manifest.name,
                        &entry.icon,
                        provider,
                        model,
                    )
                }));
            }
        }
        models
    }

    pub async fn model_descriptor(&self, model_id: &str) -> Result<PluginModelDescriptor> {
        let (plugin_id, provider_id, upstream_id) = parse_model_id(model_id)
            .ok_or_else(|| Error::Provider(format!("invalid plugin model ID: {model_id}")))?;
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let provider = find_provider(&entry, provider_id)?;
        let stored = self
            .inner
            .state
            .models(plugin_id, provider_id)
            .await?
            .into_iter()
            .find(|model| model.id == upstream_id)
            .ok_or_else(|| Error::RunNotFound(format!("plugin model {model_id}")))?;
        Self::ensure_invocable(stored.enabled, model_id)?;
        Ok(PluginModelDescriptor::new(
            plugin_id,
            &entry.manifest.name,
            &entry.icon,
            provider,
            &stored,
        ))
    }

    pub async fn plan_model(&self, model_id: &str) -> Result<PluginInvocationPlan> {
        let model = self.model_descriptor(model_id).await?;
        let request_url = format!("plugin://{}/{}", model.plugin_id, model.provider_id);
        Ok(PluginInvocationPlan { model, request_url })
    }

    /// 停用是"别再用它"，不是"只是列表里不显示"：调用路径与列表读同一个开关。
    fn ensure_invocable(enabled: bool, model_id: &str) -> Result<()> {
        if enabled {
            return Ok(());
        }
        Err(Error::Provider(format!(
            "plugin model {model_id} is disabled; enable it in the plugin's model management and try again"
        )))
    }

    /// 插件模型的统一 Provider 流:选首个可用资源,经 Worker 执行,
    /// 事件与内置 Provider 走同一管道。未来的负载均衡在这里换资源重试。
    pub fn stream_model(
        &self,
        invocation: ModelInvocation,
        cancellation: CancellationToken,
        recorder: CallRecorder,
    ) -> ProviderStream {
        let registry = self.clone();
        Box::pin(try_stream! {
            let model_id = invocation.request.model.model_id.clone();
            let (plugin_id, provider_id, upstream_id) = parse_model_id(&model_id)
                .map(|(plugin, provider, model)| (plugin.to_owned(), provider.to_owned(), model.to_owned()))
                .ok_or_else(|| Error::Provider(format!("invalid plugin model ID: {model_id}")))?;
            let executable = registry.executable()?;
            let entry = registry.find_entry(&executable, &plugin_id).await?;
            let provider = find_provider(&entry, &provider_id)?.clone();
            let stored = registry.inner.state.models(&plugin_id, &provider_id).await?
                .into_iter()
                .find(|model| model.id == upstream_id)
                .ok_or_else(|| Error::RunNotFound(format!("plugin model {model_id}")))?;
            Self::ensure_invocable(stored.enabled, &model_id)?;
            let resource = match &provider.resource_type {
                Some(resource_type) => Some((
                    resource_type.clone(),
                    registry.select_resource(&plugin_id, resource_type).await?,
                )),
                None => None,
            };
            let request = wire::llm_request(&invocation)?;
            let params = serde_json::json!({
                "providerId": provider_id,
                "model": stored.snapshot(),
                "resource": resource.as_ref().map(|(resource_type, record)| record.snapshot(resource_type)),
                "request": request,
            });
            let worker = registry.worker(&entry, &executable).await;
            let mut items = worker.invoke_streaming("provider.invoke", params, cancellation.clone(), Some(recorder)).await?;
            yield ModelEvent::Start { model_call_id: invocation.call_id.clone() };
            while let Some(item) = items.recv().await {
                match item {
                    WorkerStreamItem::Event(event) => {
                        yield wire::model_event(&event)?;
                    }
                    WorkerStreamItem::Result(result) => {
                        let value = result?;
                        let status = value.get("status").and_then(serde_json::Value::as_str).unwrap_or_default();
                        let patch = value.get("patch")
                            .filter(|patch| !patch.is_null())
                            .map(|patch| serde_json::from_value::<ResourcePatch>(patch.clone()))
                            .transpose()?;
                        if let (Some(patch), Some((resource_type, record))) = (patch, resource.as_ref()) {
                            if let Err(error) = registry.inner.state
                                .apply_patch(&plugin_id, resource_type, &record.id, patch).await
                            {
                                tracing::warn!(plugin = %plugin_id, %error, "failed to apply plugin resource patch");
                            }
                        }
                        match status {
                            "completed" => return,
                            "resource-error" | "request-error" => {
                                let message = value.get("message")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("plugin provider call failed");
                                Err(Error::Provider(message.to_owned()))?;
                            }
                            status => {
                                Err(Error::Protocol(format!("unknown plugin provider result: {status}")))?;
                            }
                        }
                    }
                }
            }
            Err(Error::Provider(format!("plugin '{plugin_id}' worker stopped mid-stream")))?;
        })
    }

    pub async fn oauth_begin(
        &self,
        plugin_id: &str,
        resource_type: &str,
        method_id: &str,
    ) -> Result<OAuthBeginResponse> {
        // 整段 begin 串行化：retain 与 insert 之间隔着插件求值（Deno）与 loopback 绑定两个
        // await 点，两个并发 begin 会各自 retain 到空、再各自 insert，留下两个会话与两个
        // 监听。人类点按很难撞上，但"重试"与"换一个入口再点一次"就会撞上。
        let _begin = self.inner.oauth_begin.lock().await;
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        let method = resource
            .add
            .iter()
            .find(|method| method.id == method_id)
            .ok_or_else(|| {
                Error::Config(format!(
                    "plugin '{plugin_id}' does not define OAuth method '{method_id}'"
                ))
            })?;
        // 同一添加入口只有一个活跃生命周期。重新开始时先丢弃旧会话，
        // Drop 授权码会话中的 CallbackHandle 会立即释放 loopback listener。
        self.inner.oauth_sessions.lock().await.retain(|_, session| {
            session.plugin_id != plugin_id
                || session.resource_type != resource_type
                || session.method_id != method_id
        });
        let worker = self.worker(&entry, &executable).await;
        let session_id = uuid::Uuid::new_v4().to_string();

        let (
            session,
            verification_url,
            verification_url_complete,
            expires_at_ms,
            poll_interval_ms,
            flow,
            user_code,
        ) = match method.method_type.as_str() {
            OAUTH2_ADD_METHOD => {
                let value = worker
                    .invoke(
                        "oauth.begin",
                        serde_json::json!({
                            "resourceType": resource_type,
                            "methodId": method.id,
                        }),
                        CancellationToken::new(),
                    )
                    .await?;
                let begin: OAuth2Begin = serde_json::from_value(value)?;
                (
                    begin.session,
                    begin.verification_url,
                    begin.verification_url_complete,
                    begin.expires_at_ms,
                    clamp_poll_interval(begin.poll_interval_ms),
                    OAuthFlow::DeviceCode,
                    Some(begin.user_code),
                )
            }
            OAUTH2_AUTHORIZATION_CODE_ADD_METHOD => {
                let state = oauth_random_secret();
                let code_verifier = oauth_random_secret();
                let code_challenge =
                    URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
                let callback = method.callback.as_ref();
                let callback = oauth_callback::bind(
                    callback.and_then(|value| value.port),
                    callback
                        .and_then(|value| value.path.as_deref())
                        .unwrap_or("/oauth-callback"),
                    state.clone(),
                    entry.manifest.name.clone(),
                    entry.icon.clone(),
                    serde_json::to_value(&resource.display_name)?,
                )
                .await?;
                let redirect_uri = callback.redirect_uri.clone();
                let value = worker
                    .invoke(
                        "oauth.begin",
                        serde_json::json!({
                            "resourceType": resource_type,
                            "methodId": method.id,
                            "authorization": {
                                "redirectUri": redirect_uri,
                                "state": state,
                                "codeChallenge": code_challenge,
                            },
                        }),
                        CancellationToken::new(),
                    )
                    .await?;
                let begin: OAuth2AuthorizationCodeBegin = serde_json::from_value(value)?;
                let poll_interval_ms = clamp_poll_interval(begin.poll_interval_ms.unwrap_or(1_000));
                (
                    begin.session,
                    begin.authorization_url,
                    None,
                    begin.expires_at_ms,
                    poll_interval_ms,
                    OAuthFlow::AuthorizationCode {
                        redirect_uri,
                        code_verifier,
                        callback,
                    },
                    None,
                )
            }
            method_type => {
                return Err(Error::Config(format!(
                        "plugin '{plugin_id}' OAuth method '{method_id}' uses unsupported type '{method_type}'"
                    )));
            }
        };

        // 过期判定必须在夹取**之前**：夹取会把过去的时间戳抬到"至少 now+1"，于是插进去一个
        // 1 毫秒后就过期的会话——看上去合法，实际第一次轮询就失败，而"返回过期会话"这个
        // 明确的错误信号被吞掉了。先拒过期，再对未来的值设上限。
        let now = now_ms();
        let Some(expires_at_ms) = accept_session_expiry(expires_at_ms, now) else {
            return Err(Error::Protocol(format!(
                "plugin '{plugin_id}' OAuth method '{method_id}' returned an expired session"
            )));
        };
        // 授权 URL 由桌面端直接打开，必须在清单声明的白名单里（见函数注释）。
        let declared_hosts = &entry.manifest.permissions.network;
        validate_authorization_url(plugin_id, declared_hosts, &verification_url)?;
        if let Some(complete) = &verification_url_complete {
            validate_authorization_url(plugin_id, declared_hosts, complete)?;
        }
        self.inner.oauth_sessions.lock().await.insert(
            session_id.clone(),
            OAuthSession {
                plugin_id: plugin_id.to_owned(),
                resource_type: resource_type.to_owned(),
                method_id: method_id.to_owned(),
                session,
                expires_at_ms,
                poll_interval_ms,
                next_poll_at_ms: now.saturating_add(poll_interval_ms),
                flow,
            },
        );
        let cleanup = self.clone();
        let cleanup_session_id = session_id.clone();
        let cleanup_delay_ms = expires_at_ms.saturating_sub(now) as u64;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(cleanup_delay_ms)).await;
            cleanup
                .inner
                .oauth_sessions
                .lock()
                .await
                .remove(&cleanup_session_id);
        });
        Ok(OAuthBeginResponse {
            session_id,
            user_code,
            verification_url,
            verification_url_complete,
            expires_at_ms,
            poll_interval_ms,
        })
    }

    pub async fn oauth_poll(&self, session_id: &str) -> Result<OAuthPollResponse> {
        let work = {
            let now = now_ms();
            let mut sessions = self.inner.oauth_sessions.lock().await;
            let Some(state) = sessions.get_mut(session_id) else {
                return Ok(OAuthPollResponse::Failed {
                    message: "authorization session no longer exists".into(),
                });
            };
            if now >= state.expires_at_ms {
                sessions.remove(session_id);
                return Ok(OAuthPollResponse::Failed {
                    message: "authorization expired".into(),
                });
            }
            if now < state.next_poll_at_ms {
                return Ok(OAuthPollResponse::Pending {
                    poll_interval_ms: state.poll_interval_ms,
                });
            }
            state.next_poll_at_ms = now + state.poll_interval_ms;

            let common = (
                state.plugin_id.clone(),
                state.resource_type.clone(),
                state.method_id.clone(),
                state.session.clone(),
            );
            match &mut state.flow {
                OAuthFlow::DeviceCode => OAuthPollWork::DeviceCode {
                    plugin_id: common.0,
                    resource_type: common.1,
                    method_id: common.2,
                    session: common.3,
                    poll_interval_ms: state.poll_interval_ms,
                },
                OAuthFlow::AuthorizationCode {
                    redirect_uri,
                    code_verifier,
                    callback,
                } => match callback.receiver.try_recv() {
                    Ok(callback_request) => OAuthPollWork::AuthorizationCode {
                        plugin_id: common.0,
                        resource_type: common.1,
                        method_id: common.2,
                        session: common.3,
                        redirect_uri: redirect_uri.clone(),
                        code_verifier: code_verifier.clone(),
                        callback_request,
                    },
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                        return Ok(OAuthPollResponse::Pending {
                            poll_interval_ms: state.poll_interval_ms,
                        });
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        sessions.remove(session_id);
                        return Ok(OAuthPollResponse::Failed {
                            message: "authorization callback stopped before completion".into(),
                        });
                    }
                },
            }
        };

        match work {
            OAuthPollWork::DeviceCode {
                plugin_id,
                resource_type,
                method_id,
                session,
                poll_interval_ms,
            } => {
                self.poll_device_code(
                    session_id,
                    plugin_id,
                    resource_type,
                    method_id,
                    session,
                    poll_interval_ms,
                )
                .await
            }
            OAuthPollWork::AuthorizationCode {
                plugin_id,
                resource_type,
                method_id,
                session,
                redirect_uri,
                code_verifier,
                callback_request,
            } => {
                self.complete_authorization_code(
                    session_id,
                    plugin_id,
                    resource_type,
                    method_id,
                    session,
                    redirect_uri,
                    code_verifier,
                    callback_request,
                )
                .await
            }
        }
    }

    async fn poll_device_code(
        &self,
        session_id: &str,
        plugin_id: String,
        resource_type: String,
        method_id: String,
        session: serde_json::Value,
        poll_interval_ms: i64,
    ) -> Result<OAuthPollResponse> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, &plugin_id).await?;
        let value = self
            .worker(&entry, &executable)
            .await
            .invoke(
                "oauth.poll",
                serde_json::json!({
                    "resourceType": resource_type,
                    "methodId": method_id,
                    "session": session,
                }),
                CancellationToken::new(),
            )
            .await?;
        let poll: OAuth2Poll = serde_json::from_value(value)?;
        match poll {
            OAuth2Poll::Pending { session } => {
                self.update_session(session_id, session, None).await;
                Ok(OAuthPollResponse::Pending { poll_interval_ms })
            }
            OAuth2Poll::SlowDown { session } => {
                let interval = poll_interval_ms + OAUTH_SLOW_DOWN_STEP_MS;
                self.update_session(session_id, session, Some(interval))
                    .await;
                Ok(OAuthPollResponse::Pending {
                    poll_interval_ms: interval,
                })
            }
            OAuth2Poll::Completed { resources } => {
                // 持久化成功后才销毁设备码会话:写盘瞬时失败时下次轮询还能重试。
                let response = self
                    .persist_oauth_resources(
                        &entry,
                        &executable,
                        &plugin_id,
                        &resource_type,
                        resources,
                    )
                    .await?;
                self.inner.oauth_sessions.lock().await.remove(session_id);
                Ok(response)
            }
            OAuth2Poll::Denied { message } => {
                self.inner.oauth_sessions.lock().await.remove(session_id);
                Ok(OAuthPollResponse::Denied { message })
            }
            OAuth2Poll::Failed { message } => {
                self.inner.oauth_sessions.lock().await.remove(session_id);
                Ok(OAuthPollResponse::Failed { message })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_authorization_code(
        &self,
        session_id: &str,
        plugin_id: String,
        resource_type: String,
        method_id: String,
        session: serde_json::Value,
        redirect_uri: String,
        code_verifier: String,
        callback_request: CallbackRequest,
    ) -> Result<OAuthPollResponse> {
        let CallbackRequest { result, response } = callback_request;
        let code = match result {
            Ok(code) => code,
            Err(message) => {
                self.inner.oauth_sessions.lock().await.remove(session_id);
                let _ = response.send(CallbackOutcome {
                    success: false,
                    message: Some(message.clone()),
                });
                return Ok(OAuthPollResponse::Denied {
                    message: Some(message),
                });
            }
        };

        let result = async {
            let executable = self.executable()?;
            let entry = self.find_entry(&executable, &plugin_id).await?;
            let value = self
                .worker(&entry, &executable)
                .await
                .invoke(
                    "oauth.complete",
                    serde_json::json!({
                        "resourceType": resource_type,
                        "methodId": method_id,
                        "session": session,
                        "authorization": {
                            "code": code,
                            "redirectUri": redirect_uri,
                            "codeVerifier": code_verifier,
                        },
                    }),
                    CancellationToken::new(),
                )
                .await?;
            let resources: Vec<ResourceDraft> = serde_json::from_value(value)?;
            self.persist_oauth_resources(&entry, &executable, &plugin_id, &resource_type, resources)
                .await
        }
        .await;

        self.inner.oauth_sessions.lock().await.remove(session_id);
        match result {
            Ok(completed) => {
                let _ = response.send(CallbackOutcome {
                    success: true,
                    message: None,
                });
                Ok(completed)
            }
            Err(error) => {
                let message = error.to_string();
                let _ = response.send(CallbackOutcome {
                    success: false,
                    message: Some(message.clone()),
                });
                Ok(OAuthPollResponse::Failed { message })
            }
        }
    }

    async fn persist_oauth_resources(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        plugin_id: &str,
        resource_type: &str,
        resources: Vec<ResourceDraft>,
    ) -> Result<OAuthPollResponse> {
        ensure_draft_count(plugin_id, resource_type, &resources)?;
        let outcome = self
            .inner
            .state
            .upsert_resources(plugin_id, resource_type, resources)
            .await?;
        let model_sync_error = self
            .sync_provider_models_for_resource(entry, executable, resource_type)
            .await;
        Ok(OAuthPollResponse::Completed {
            added: outcome.added,
            updated: outcome.updated,
            model_sync_error,
        })
    }

    pub async fn import_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
        files: serde_json::Value,
    ) -> Result<ImportResponse> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        if resource.import.is_none() {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' resource '{resource_type}' does not support import"
            )));
        }
        let value = self
            .worker(&entry, &executable)
            .await
            .invoke(
                "import.parse",
                serde_json::json!({ "resourceType": resource_type, "files": files }),
                CancellationToken::new(),
            )
            .await?;
        let parsed: ImportParseResult = serde_json::from_value(value)?;
        if parsed.resources.is_empty() {
            return Err(Error::Config(
                parsed
                    .warnings
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "import produced no resources".into()),
            ));
        }
        if parsed.resources.len() > MAX_IMPORT_DRAFTS {
            return Err(Error::Config(format!(
                "import produced more than {MAX_IMPORT_DRAFTS} resources"
            )));
        }
        let outcome = self
            .inner
            .state
            .upsert_resources(plugin_id, resource_type, parsed.resources)
            .await?;
        let model_sync_error = self
            .sync_provider_models_for_resource(&entry, &executable, resource_type)
            .await;
        Ok(ImportResponse {
            added: outcome.added,
            updated: outcome.updated,
            warnings: parsed.warnings,
            model_sync_error,
        })
    }

    /// 导出某资源类型的全部私有数据,供备份或迁移;格式与批量导入兼容。
    pub async fn export_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
    ) -> Result<serde_json::Value> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        find_resource(&entry, resource_type)?;
        let records = self.inner.state.resources(plugin_id, resource_type).await?;
        Ok(serde_json::json!({
            "accounts": records
                .iter()
                .map(|record| record.private_data.clone())
                .collect::<Vec<_>>(),
        }))
    }

    pub async fn refresh_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<()> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        if !resource.can_refresh {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' resource '{resource_type}' does not support refresh"
            )));
        }
        let record = self
            .find_record(plugin_id, resource_type, resource_id)
            .await?;
        let value = self
            .worker(&entry, &executable)
            .await
            .invoke(
                "resource.refresh",
                serde_json::json!({
                    "resourceType": resource_type,
                    "resource": record.snapshot(resource_type),
                }),
                CancellationToken::new(),
            )
            .await?;
        let patch: ResourcePatch = serde_json::from_value(value)?;
        self.inner
            .state
            .apply_patch(plugin_id, resource_type, resource_id, patch)
            .await
    }

    pub async fn resource_action(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
        action_id: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        let action = resource
            .actions
            .iter()
            .find(|action| action.id == action_id)
            .ok_or_else(|| {
                Error::Config(format!(
                    "plugin '{plugin_id}' resource '{resource_type}' does not define action '{action_id}'"
                ))
            })?;
        if !matches!(action.target.as_str(), "resource" | "card") {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' resource action '{action_id}' has an invalid target"
            )));
        }
        let record = self
            .find_record(plugin_id, resource_type, resource_id)
            .await?;
        let value = self
            .worker(&entry, &executable)
            .await
            .invoke(
                "resource.action",
                serde_json::json!({
                    "resourceType": resource_type,
                    "actionId": action_id,
                    "resource": record.snapshot(resource_type),
                    "input": input,
                }),
                CancellationToken::new(),
            )
            .await?;
        let result: ResourceActionResult = serde_json::from_value(value)?;
        if let Some(patch) = result.patch.clone() {
            self.inner
                .state
                .apply_patch(plugin_id, resource_type, resource_id, patch)
                .await?;
        }
        Ok(serde_json::to_value(ResourceActionResponse::from(result))?)
    }

    pub async fn delete_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<()> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        let record = self
            .find_record(plugin_id, resource_type, resource_id)
            .await?;
        if resource.can_remove {
            // 上游撤销失败不阻塞本地删除:用户必须能移除已失效的资源。
            if let Err(error) = self
                .worker(&entry, &executable)
                .await
                .invoke(
                    "resource.remove",
                    serde_json::json!({
                        "resourceType": resource_type,
                        "resource": record.snapshot(resource_type),
                    }),
                    CancellationToken::new(),
                )
                .await
            {
                tracing::warn!(plugin = %plugin_id, %error, "plugin resource remove hook failed");
            }
        }
        self.inner
            .state
            .remove_resource(plugin_id, resource_type, resource_id)
            .await?;
        Ok(())
    }

    pub async fn sync_models(&self, plugin_id: &str, provider_id: &str) -> Result<usize> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let provider = find_provider(&entry, provider_id)?.clone();
        self.sync_provider_models(&entry, &executable, &provider)
            .await
    }

    pub async fn set_model_enabled(
        &self,
        plugin_id: &str,
        provider_id: &str,
        model_id: &str,
        enabled: bool,
    ) -> Result<()> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let provider = find_provider(&entry, provider_id)?;
        if !provider.has_models {
            return Err(Error::Config(format!(
                "plugin provider '{provider_id}' does not enumerate models"
            )));
        }
        self.inner
            .state
            .set_model_enabled(plugin_id, provider_id, model_id, enabled)
            .await
    }

    pub async fn remove(&self, plugin_id: &str) -> Result<()> {
        // 先把 worker 取出来、放开 workers 锁，再等子进程收尾：`stop()` 要等进程退出，
        // 持锁等待会把同一插件的其它调用一起堵在锁上（它们只是想取 worker）。
        let worker = self.inner.workers.lock().await.remove(plugin_id);
        if let Some(worker) = worker {
            worker.stop().await;
        }
        // 卸载后必须重扫：缓存里还留着这个插件的定义，它已经不在了。
        *self.inner.entries.write().await = None;
        self.inner.state.clear(plugin_id).await
    }

    /// 读插件私有状态：失败当空继续（模型目录缺一段不该让整个插件页失败），但必须留下
    /// 诊断线索。以前这里是 `unwrap_or_default()`，于是"磁盘上的数据读不出来"和"没配过"
    /// 在日志里完全一样，用户只看到模型/资源凭空消失。
    fn state_or_empty<T>(plugin_id: &str, scope: &str, result: Result<Vec<T>>) -> Vec<T> {
        match result {
            Ok(values) => values,
            Err(error) => {
                tracing::warn!(
                    plugin = %plugin_id,
                    scope,
                    %error,
                    "plugin state is unreadable; continuing without it"
                );
                Vec::new()
            }
        }
    }

    async fn descriptor(&self, entry: &PluginEntry, executable: &Path) -> PluginDescriptor {
        let plugin_id = &entry.manifest.id;
        let mut providers = Vec::new();
        for provider in &entry.definition.providers {
            let stored = Self::state_or_empty(
                plugin_id,
                "models",
                self.inner.state.models(plugin_id, &provider.id).await,
            );
            let configured = self.provider_configured(entry, provider).await;
            providers.push(PluginProviderDescriptor {
                id: provider.id.clone(),
                plugin_id: plugin_id.clone(),
                display_name: provider.display_name.clone(),
                description: provider.description.clone(),
                provider_type: provider.provider_type.clone(),
                resource_type: provider.resource_type.clone(),
                has_models: provider.has_models,
                configured,
                models: stored
                    .iter()
                    .map(|model| {
                        PluginModelDescriptor::new(
                            plugin_id,
                            &entry.manifest.name,
                            &entry.icon,
                            provider,
                            model,
                        )
                    })
                    .collect(),
            });
        }
        let mut resources = Vec::new();
        for definition in &entry.definition.resources {
            let records = Self::state_or_empty(
                plugin_id,
                "resources",
                self.inner
                    .state
                    .resources(plugin_id, &definition.resource_type)
                    .await,
            );
            let views = self
                .present_resources(entry, executable, definition, &records)
                .await;
            resources.push(PluginResourceDescriptor {
                resource_type: definition.resource_type.clone(),
                display_name: definition.display_name.clone(),
                add: definition.add.clone(),
                import: definition.import.clone(),
                actions: definition.actions.clone(),
                can_refresh: definition.can_refresh,
                can_remove: definition.can_remove,
                resources: views,
            });
        }
        PluginDescriptor {
            id: plugin_id.clone(),
            name: entry.manifest.name.clone(),
            version: entry.manifest.version.clone(),
            author: entry.manifest.author.clone(),
            icon: entry.icon.clone(),
            providers,
            resources,
        }
    }

    async fn present_resources(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        definition: &ResourceDefinition,
        records: &[ResourceRecord],
    ) -> Vec<PluginResourceView> {
        if records.is_empty() {
            return Vec::new();
        }
        let snapshots = records
            .iter()
            .map(|record| record.snapshot(&definition.resource_type))
            .collect::<Vec<_>>();
        let presented = self
            .worker(entry, executable)
            .await
            .invoke(
                "resource.present",
                serde_json::json!({
                    "resourceType": definition.resource_type,
                    "resources": snapshots,
                }),
                CancellationToken::new(),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<Vec<ResourcePresentation>>(value).map_err(Error::from)
            });
        // 插件没给出可用的展示时退回 key：资源还在，只是没有好名字。
        let fallback = || {
            records
                .iter()
                .map(|record| {
                    PluginResourceView::from_record(
                        record,
                        ResourcePresentation {
                            display_name: record.key.clone(),
                            description: serde_json::Value::Null,
                            metrics: Vec::new(),
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        match presented {
            Ok(views) if views.len() == records.len() => records
                .iter()
                .zip(views)
                .map(|(record, view)| PluginResourceView::from_record(record, view))
                .collect(),
            Ok(views) => {
                tracing::warn!(
                    plugin = %entry.manifest.id,
                    resource_type = %definition.resource_type,
                    expected = records.len(),
                    got = views.len(),
                    "plugin resource presentation does not match the stored resources; showing keys"
                );
                fallback()
            }
            Err(error) => {
                tracing::warn!(
                    plugin = %entry.manifest.id,
                    resource_type = %definition.resource_type,
                    %error,
                    "plugin resource presentation failed; showing keys"
                );
                fallback()
            }
        }
    }

    async fn provider_configured(
        &self,
        entry: &PluginEntry,
        provider: &ProviderDefinition,
    ) -> bool {
        let plugin_id = &entry.manifest.id;
        if provider.has_models {
            let models = Self::state_or_empty(
                plugin_id,
                "models",
                self.inner.state.models(plugin_id, &provider.id).await,
            );
            if models.is_empty() {
                return false;
            }
        }
        match &provider.resource_type {
            Some(resource_type) => !Self::state_or_empty(
                plugin_id,
                "resources",
                self.inner.state.resources(plugin_id, resource_type).await,
            )
            .is_empty(),
            None => true,
        }
    }

    /// 资源到位后刷新使用该资源类型的 Provider 模型目录;失败只报告不中断。
    async fn sync_provider_models_for_resource(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        resource_type: &str,
    ) -> Option<String> {
        let mut errors = Vec::new();
        for provider in entry.definition.providers.clone() {
            if provider.resource_type.as_deref() != Some(resource_type) || !provider.has_models {
                continue;
            }
            if let Err(error) = self
                .sync_provider_models(entry, executable, &provider)
                .await
            {
                errors.push(format!("{}: {error}", provider.id));
            }
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    async fn sync_provider_models(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        provider: &ProviderDefinition,
    ) -> Result<usize> {
        if !provider.has_models {
            return Err(Error::Config(format!(
                "plugin provider '{}' does not enumerate models",
                provider.id
            )));
        }
        let plugin_id = &entry.manifest.id;
        let resource = match &provider.resource_type {
            Some(resource_type) => {
                let record = self.select_resource(plugin_id, resource_type).await?;
                Some(record.snapshot(resource_type))
            }
            None => None,
        };
        let value = self
            .worker(entry, executable)
            .await
            .invoke(
                "models.list",
                serde_json::json!({ "providerId": provider.id, "resource": resource }),
                CancellationToken::new(),
            )
            .await?;
        let definitions = value
            .as_array()
            .ok_or_else(|| Error::Protocol("plugin models.list must return an array".into()))?;
        let mut models = Vec::with_capacity(definitions.len());
        let mut seen = std::collections::HashSet::new();
        for definition in definitions {
            let model = StoredModel::from_definition(definition)?;
            if seen.insert(model.id.clone()) {
                models.push(model);
            }
        }
        if models.is_empty() {
            return Err(Error::Provider(format!(
                "plugin provider '{}' returned no models",
                provider.id
            )));
        }
        self.inner
            .state
            .replace_models(plugin_id, &provider.id, &models)
            .await?;
        Ok(models.len())
    }

    /// 第一版选择策略:按创建顺序取首个可用资源;冷却到期视为可用。
    async fn select_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
    ) -> Result<ResourceRecord> {
        let records = self.inner.state.resources(plugin_id, resource_type).await?;
        if records.is_empty() {
            return Err(Error::Provider(format!(
                "plugin '{plugin_id}' has no '{resource_type}' resource; add one first"
            )));
        }
        let now = now_ms();
        records
            .iter()
            .find(|record| record.state.is_ready(now))
            .cloned()
            .ok_or_else(|| {
                Error::Provider(
                    "no plugin account is ready; wait for cooldown or sign in again".into(),
                )
            })
    }

    async fn find_record(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<ResourceRecord> {
        self.inner
            .state
            .resources(plugin_id, resource_type)
            .await?
            .into_iter()
            .find(|record| record.id == resource_id)
            .ok_or_else(|| Error::RunNotFound(format!("plugin resource {resource_id}")))
    }

    async fn update_session(
        &self,
        session_id: &str,
        session: Option<serde_json::Value>,
        poll_interval_ms: Option<i64>,
    ) {
        let mut sessions = self.inner.oauth_sessions.lock().await;
        if let Some(state) = sessions.get_mut(session_id) {
            if let Some(session) = session {
                state.session = session;
            }
            if let Some(interval) = poll_interval_ms {
                state.poll_interval_ms = interval;
            }
        }
    }

    fn executable(&self) -> Result<std::path::PathBuf> {
        self.inner
            .runtime
            .executable()
            .ok_or_else(|| Error::Config("plugin runtime is not ready".into()))
    }

    async fn entries(&self, executable: &Path) -> Vec<PluginEntry> {
        // 缓存键是插件目录指纹：求值一次插件要跑一次 Deno，不能每次请求都重来；但改了
        // 插件、修好清单、预装或卸载之后必须立刻生效（以前缓存一旦填充就永不失效，
        // 只能重启应用）。
        let fingerprint = self.inner.catalog.fingerprint();
        if let Some(cached) = self.inner.entries.read().await.as_ref() {
            if cached.fingerprint == fingerprint {
                return cached.entries.clone();
            }
        }
        let loaded = self.inner.catalog.entries(executable).await;
        *self.inner.entries.write().await = Some(CachedEntries {
            fingerprint,
            entries: loaded.clone(),
        });
        loaded
    }

    async fn find_entry(&self, executable: &Path, plugin_id: &str) -> Result<PluginEntry> {
        self.entries(executable)
            .await
            .into_iter()
            .find(|entry| entry.manifest.id == plugin_id)
            .ok_or_else(|| Error::RunNotFound(format!("plugin {plugin_id}")))
    }

    async fn worker(&self, entry: &PluginEntry, executable: &Path) -> Arc<PluginWorker> {
        let mut workers = self.inner.workers.lock().await;
        workers
            .entry(entry.manifest.id.clone())
            .or_insert_with(|| {
                Arc::new(PluginWorker::new(
                    entry,
                    executable.to_path_buf(),
                    self.inner.catalog.loader().clone(),
                    self.inner.store.clone(),
                ))
            })
            .clone()
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OAuth2Begin {
    session: serde_json::Value,
    user_code: String,
    verification_url: String,
    #[serde(default)]
    verification_url_complete: Option<String>,
    expires_at_ms: i64,
    poll_interval_ms: i64,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OAuth2AuthorizationCodeBegin {
    session: serde_json::Value,
    authorization_url: String,
    expires_at_ms: i64,
    #[serde(default)]
    poll_interval_ms: Option<i64>,
}

/// 插件返回的授权 URL 会由桌面端直接交给系统浏览器打开，所以它必须落在清单声明的网络
/// 白名单里：否则插件可以把用户带去任意站点（白名单是安装时用户能审阅、也是宿主的出网
/// 校验依据），而这一步发生在宿主的 `network.fetch` 之外，绕开了那层校验。
///
/// 只接受 https 且必须带主机名；子域算命中（清单通常声明 `example.com`，授权页在
/// `login.example.com`）。
fn validate_authorization_url(plugin_id: &str, declared_hosts: &[String], url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).map_err(|error| {
        Error::Config(format!(
            "plugin '{plugin_id}' returned an invalid authorization URL: {error}"
        ))
    })?;
    if parsed.scheme() != "https" {
        return Err(Error::Config(format!(
            "plugin '{plugin_id}' returned an authorization URL that is not https"
        )));
    }
    let host = parsed.host_str().ok_or_else(|| {
        Error::Config(format!(
            "plugin '{plugin_id}' returned an authorization URL without a host"
        ))
    })?;
    let host = host.to_ascii_lowercase();
    let declared = declared_hosts.iter().any(|allowed| {
        let allowed = allowed.to_ascii_lowercase();
        host == allowed || host.ends_with(&format!(".{allowed}"))
    });
    if !declared {
        return Err(Error::Config(format!(
            "plugin '{plugin_id}' returned an authorization URL for '{host}', which is not in its declared network hosts"
        )));
    }
    Ok(())
}

/// 轮询间隔夹到 1s~300s：插件给的值直接进定时器，不给上限等于让它决定会话多久被问一次。
fn clamp_poll_interval(interval_ms: i64) -> i64 {
    interval_ms.clamp(OAUTH_POLL_MIN_MS, OAUTH_POLL_MAX_MS)
}

/// 校验并夹取会话过期时间：**先**拒掉已经过期的时间戳（插件返回过去的时间就是"这个会话
/// 已经废了"，必须明确报错），再对未来的值设 30 分钟上限。
///
/// 顺序不能反：`clamp_session_expiry` 会把任何值抬到至少 `now+1`，先夹取就再也看不出
/// "插件给的是过期时间"——会话会以"1 毫秒后过期"的合法样子插进去，第一次轮询才失败。
fn accept_session_expiry(expires_at_ms: i64, now: i64) -> Option<i64> {
    if expires_at_ms <= now {
        return None;
    }
    Some(clamp_session_expiry(expires_at_ms, now))
}

/// 过期时间取"插件给的"与"现在 + 30 分钟"里更早的那个，并且至少留 1ms：插件给一个远期
/// 时间戳会让会话与回调监听一直留着；给一个过去的时间戳则被调用方判成"已过期"而拒绝。
fn clamp_session_expiry(expires_at_ms: i64, now: i64) -> i64 {
    let ceiling = now.saturating_add(OAUTH_SESSION_MAX_MS);
    expires_at_ms.min(ceiling).max(now.saturating_add(1))
}

/// OAuth 完成时落库的资源条数上限：与导入路径共用同一个常量——插件返回多少就写多少
/// 等于让它决定磁盘占用与界面上的列表长度。
fn ensure_draft_count(
    plugin_id: &str,
    resource_type: &str,
    resources: &[ResourceDraft],
) -> Result<()> {
    if resources.len() > MAX_IMPORT_DRAFTS {
        return Err(Error::Config(format!(
            "plugin '{plugin_id}' returned {} resources for '{resource_type}'; the limit is {MAX_IMPORT_DRAFTS}",
            resources.len()
        )));
    }
    Ok(())
}

fn oauth_random_secret() -> String {
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(first.as_bytes());
    bytes[16..].copy_from_slice(second.as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "kebab-case", tag = "status")]
enum OAuth2Poll {
    #[serde(rename_all = "camelCase")]
    Pending {
        #[serde(default)]
        session: Option<serde_json::Value>,
    },
    #[serde(rename_all = "camelCase")]
    SlowDown {
        #[serde(default)]
        session: Option<serde_json::Value>,
    },
    #[serde(rename_all = "camelCase")]
    Completed { resources: Vec<ResourceDraft> },
    #[serde(rename_all = "camelCase")]
    Denied {
        #[serde(default)]
        message: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Failed { message: String },
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportParseResult {
    resources: Vec<ResourceDraft>,
    #[serde(default)]
    warnings: Vec<String>,
}

fn find_provider<'a>(entry: &'a PluginEntry, provider_id: &str) -> Result<&'a ProviderDefinition> {
    entry
        .definition
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| {
            Error::RunNotFound(format!(
                "plugin '{}' provider {provider_id}",
                entry.manifest.id
            ))
        })
}

fn find_resource<'a>(
    entry: &'a PluginEntry,
    resource_type: &str,
) -> Result<&'a ResourceDefinition> {
    entry
        .definition
        .resources
        .iter()
        .find(|resource| resource.resource_type == resource_type)
        .ok_or_else(|| {
            Error::RunNotFound(format!(
                "plugin '{}' resource type {resource_type}",
                entry.manifest.id
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::state::ResourceStateInput;

    async fn fixture() -> (tempfile::TempDir, PluginRegistry) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let registry = PluginRegistry {
            inner: Arc::new(RegistryInner {
                store,
                runtime: super::super::runtime::tests::runtime(directory.path()),
                catalog: super::super::catalog::tests::catalog(directory.path()),
                state: PluginStateStore::new(
                    PluginDataStore::for_test(directory.path().join("data")).unwrap(),
                ),
                entries: RwLock::new(None),
                workers: Mutex::new(HashMap::new()),
                oauth_sessions: Mutex::new(HashMap::new()),
                oauth_begin: Mutex::new(()),
            }),
        };
        (directory, registry)
    }

    fn resource(key: &str, state: ResourceStateInput) -> ResourceDraft {
        ResourceDraft {
            key: key.into(),
            private_data: serde_json::json!({"accessToken": "fixture-only"}),
            state: Some(state),
        }
    }

    #[tokio::test]
    async fn selection_never_reuses_invalid_or_cooling_accounts() {
        let (_directory, registry) = fixture().await;
        for state in [
            ResourceStateInput::Invalid { message: None },
            ResourceStateInput::Cooling {
                retry_at_ms: Some(now_ms() + 60_000),
                message: None,
            },
            ResourceStateInput::Cooling {
                retry_at_ms: None,
                message: None,
            },
        ] {
            registry
                .inner
                .state
                .upsert_resources("test", "account", vec![resource("first", state)])
                .await
                .unwrap();
            assert!(
                registry.select_resource("test", "account").await.is_err(),
                "a blocked account must not receive another model call"
            );
        }
    }

    #[tokio::test]
    async fn selection_skips_blocked_accounts_and_recovers_expired_cooling() {
        let (_directory, registry) = fixture().await;
        registry
            .inner
            .state
            .upsert_resources(
                "test",
                "account",
                vec![
                    resource("blocked", ResourceStateInput::Invalid { message: None }),
                    resource("usable", ResourceStateInput::Ready),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            registry
                .select_resource("test", "account")
                .await
                .unwrap()
                .key,
            "usable"
        );

        registry
            .inner
            .state
            .upsert_resources(
                "test",
                "account",
                vec![resource(
                    "blocked",
                    ResourceStateInput::Cooling {
                        retry_at_ms: Some(now_ms() - 1),
                        message: None,
                    },
                )],
            )
            .await
            .unwrap();
        assert_eq!(
            registry
                .select_resource("test", "account")
                .await
                .unwrap()
                .key,
            "blocked"
        );
    }

    /// 停用必须挡住调用，而不只是从列表里消失：模型管理里关掉的模型，
    /// 任何指向它的请求（旧映射、手工测试）都要被拒绝并说清怎么办。
    #[test]
    fn a_disabled_model_cannot_be_invoked() {
        let error = PluginRegistry::ensure_invocable(false, "plugin:test/provider/model")
            .expect_err("disabled models must not be invocable");
        assert!(
            error.to_string().contains("is disabled"),
            "错误要说清是停用：{error}"
        );
        assert!(PluginRegistry::ensure_invocable(true, "plugin:test/provider/model").is_ok());
    }

    struct LogWriter {
        logs: Arc<parking_lot::Mutex<Vec<u8>>>,
    }

    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.logs.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 状态读不出来时要降级为空（模型目录缺一段不该让整个插件页失败），但必须留下日志：
    /// 以前 `unwrap_or_default()` 让"数据读不出来"和"没配过"在日志里完全一样。
    #[test]
    fn unreadable_plugin_state_is_logged_before_degrading_to_empty() {
        let logs = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let logs = logs.clone();
                move || LogWriter { logs: logs.clone() }
            })
            .with_ansi(false)
            .finish();

        let degraded = tracing::subscriber::with_default(subscriber, || {
            PluginRegistry::state_or_empty::<u32>(
                "dev.example",
                "models",
                Err(Error::Config("corrupt state file".into())),
            )
        });

        assert!(degraded.is_empty(), "读不出来仍然要降级为空");
        let logs = String::from_utf8(logs.lock().clone()).unwrap();
        assert!(
            logs.contains("dev.example") && logs.contains("models"),
            "日志要能定位到插件与范围：{logs}"
        );
        assert!(
            logs.contains("corrupt state file"),
            "日志要带上原因：{logs}"
        );
    }

    /// 读得到的状态原样通过，不该因为加了日志就改变行为。
    #[test]
    fn readable_plugin_state_passes_through() {
        assert_eq!(
            PluginRegistry::state_or_empty("dev.example", "models", Ok(vec![1_u32, 2])),
            vec![1, 2]
        );
    }

    fn draft(key: &str) -> ResourceDraft {
        serde_json::from_value(serde_json::json!({
            "key": key,
            "privateData": {"token": "secret"},
        }))
        .unwrap()
    }

    /// OAuth 完成时落库的条数必须与导入路径同一个上限：插件返回多少就写多少，等于让它
    /// 决定磁盘占用与界面上的列表长度。
    #[test]
    fn an_oauth_completion_cannot_persist_more_drafts_than_the_import_limit() {
        let allowed = (0..MAX_IMPORT_DRAFTS)
            .map(|index| draft(&format!("key-{index}")))
            .collect::<Vec<_>>();
        assert!(ensure_draft_count("dev.example", "account", &allowed).is_ok());

        let mut too_many = allowed;
        too_many.push(draft("one-too-many"));
        let error =
            ensure_draft_count("dev.example", "account", &too_many).expect_err("超过上限必须拒绝");
        assert!(error.to_string().contains("the limit is 256"), "{error}");
        assert!(error.to_string().contains("dev.example"), "{error}");
    }

    /// 轮询间隔与过期时间完全由插件给：不夹取就是让插件决定会话多久被问一次、留多久。
    #[test]
    fn plugin_supplied_oauth_timing_is_clamped() {
        assert_eq!(clamp_poll_interval(1), OAUTH_POLL_MIN_MS);
        assert_eq!(clamp_poll_interval(i64::MAX), OAUTH_POLL_MAX_MS);
        assert_eq!(clamp_poll_interval(5_000), 5_000);

        let now = 1_000_000;
        assert_eq!(
            clamp_session_expiry(i64::MAX, now),
            now + OAUTH_SESSION_MAX_MS,
            "远期时间戳被压到 30 分钟"
        );
        assert_eq!(
            clamp_session_expiry(now + 60_000, now),
            now + 60_000,
            "窗口内的值原样保留"
        );
        assert_eq!(
            clamp_session_expiry(i64::MIN, now),
            now + 1,
            "过去的时间戳不会变成负数等待"
        );
    }

    /// 插件目录变了就必须重扫：以前缓存一旦填充永不失效，改插件、修清单或卸载都要重启。
    /// 断言缓存里的指纹被换掉（而不是结果变了），因为测试环境没有 Deno，插件求值本就失败。
    #[tokio::test]
    async fn a_changed_plugin_directory_invalidates_the_entry_cache() {
        let (directory, registry) = fixture().await;
        let root = directory.path().join("installed");
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::write(root.join("alpha/plugin.json"), b"{}").unwrap();

        let missing_deno = Path::new("missing-deno-executable");
        assert!(registry.entries(missing_deno).await.is_empty());
        let first = cached_fingerprint(&registry).await;

        // 目录没动：走缓存，指纹不变。
        registry.entries(missing_deno).await;
        assert_eq!(cached_fingerprint(&registry).await, first);

        // 目录动了（长度也变了）：必须重扫。
        std::fs::write(root.join("alpha/plugin.json"), b"{\"id\":\"alpha\"}").unwrap();
        registry.entries(missing_deno).await;
        assert_ne!(
            cached_fingerprint(&registry).await,
            first,
            "插件目录变化后缓存必须失效"
        );
    }

    /// 卸载要放开 workers 锁再等进程退出，并且把插件表缓存清掉。
    /// 这里只钉住"缓存被清空"这一步：没有插件在跑，`stop()` 不会阻塞。
    #[tokio::test]
    async fn removing_a_plugin_clears_the_entry_cache() {
        let (_directory, registry) = fixture().await;
        registry.entries(Path::new("missing-deno-executable")).await;
        assert!(registry.inner.entries.read().await.is_some());

        registry.remove("dev.example").await.unwrap();

        assert!(
            registry.inner.entries.read().await.is_none(),
            "卸载后必须重扫插件表"
        );
    }

    /// 两个并发 begin 不能各插一个会话：begin 整段被同一把锁串行化，第二个进来时
    /// 第一个的 retain 已经生效。这里用"锁被占住时 begin 不推进"来钉住它。
    #[tokio::test]
    async fn oauth_begin_waits_for_the_previous_begin_to_finish() {
        let (_directory, registry) = fixture().await;
        let guard = registry.inner.oauth_begin.lock().await;

        let mut blocked = tokio::spawn({
            let registry = registry.clone();
            async move {
                registry
                    .oauth_begin("dev.example", "account", "add")
                    .await
                    .err()
                    .map(|error| error.to_string())
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut blocked)
                .await
                .is_err(),
            "锁被占住时 begin 必须等待，而不是直接跑进去"
        );

        drop(guard);
        let outcome = tokio::time::timeout(Duration::from_secs(5), blocked)
            .await
            .expect("放开锁之后 begin 必须能继续")
            .unwrap();
        // 测试环境没有 Deno 运行时，begin 会在取运行时那一步失败；关键的是它在放开锁之后
        // 真的继续执行了，而不是一直等在锁上。
        assert!(
            outcome.is_some(),
            "放开锁之后 begin 必须继续执行并给出结果：{outcome:?}"
        );
    }

    /// 插件返回的授权 URL 会由桌面端直接交给系统浏览器：它必须在清单声明的白名单里，
    /// 否则插件能把用户带去任意站点（这条路绕开了宿主的 network.fetch 校验）。
    #[test]
    fn plugin_authorization_urls_must_stay_inside_the_declared_hosts() {
        let hosts = vec!["example.com".to_string(), "accounts.google.com".to_string()];

        assert!(
            validate_authorization_url("dev.example", &hosts, "https://example.com/device").is_ok()
        );
        assert!(
            validate_authorization_url("dev.example", &hosts, "https://login.example.com/activate")
                .is_ok(),
            "子域算命中"
        );
        assert!(validate_authorization_url(
            "dev.example",
            &hosts,
            "https://accounts.google.com/o/oauth2/device"
        )
        .is_ok());

        for (url, hint) in [
            (
                "https://evil.example.net/steal",
                "not in its declared network hosts",
            ),
            (
                "https://example.com.evil.net/steal",
                "not in its declared network hosts",
            ),
            ("http://example.com/device", "not https"),
            // `https:///no-host` 会被 url 解析成主机名 `no-host`，所以这里用真正缺主机的形态。
            ("https://", "invalid authorization URL"),
            ("not a url", "invalid authorization URL"),
        ] {
            let error = validate_authorization_url("dev.example", &hosts, url)
                .expect_err("这个 URL 必须被拒绝");
            assert!(error.to_string().contains(hint), "{url}: {error}");
            assert!(error.to_string().contains("dev.example"), "{url}: {error}");
        }
    }

    /// 内置插件的授权域必须真的写在各自清单里：白名单校验（`validate_authorization_url`）
    /// 一上，清单漏声明就等于把登录按钮打死——插件返回的授权 URL 会被直接拒绝。
    ///
    /// URL 取自插件源码（注释给了位置），清单从磁盘读，所以改清单/改授权域两端都会碰到这里。
    /// 逆变在循环体里：把清单里覆盖该主机的域删掉后，同一个 URL 必须被拒——证明"通过"来自
    /// 清单声明，而不是校验被绕过。
    #[test]
    fn built_in_plugins_declare_the_authorization_hosts_they_return() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins/build-in");
        // (插件目录, 该插件 `oauth.begin` 会返回的授权 / 设备激活页 URL)
        let cases: [(&str, &[&str]); 3] = [
            (
                // google_oauth.ts 的 GOOGLE_AUTHORIZATION_URL
                "antigravity-auth",
                &["https://accounts.google.com/o/oauth2/v2/auth?client_id=google"],
            ),
            (
                // codex-auth/oauth.ts 的 VERIFICATION_URI
                "codex-auth",
                &["https://auth.openai.com/codex/device"],
            ),
            (
                // grok-auth 的设备码激活页（grok_test.ts 断言 verification_uri 就是它）
                "grok-auth",
                &[
                    "https://accounts.x.ai/activate",
                    "https://accounts.x.ai/activate?code=ABCD-EFGH",
                ],
            ),
        ];
        for (plugin, urls) in cases {
            let raw = std::fs::read(root.join(plugin).join("plugin.json")).unwrap();
            let manifest: crate::plugin::manifest::PluginManifest =
                serde_json::from_slice(&raw).unwrap();
            for url in urls {
                validate_authorization_url(&manifest.id, &manifest.permissions.network, url)
                    .unwrap_or_else(|error| {
                        panic!("{plugin}: 内置插件的授权 URL 过不了清单白名单: {error}")
                    });

                // 逆变：把覆盖该主机的声明去掉，同一个 URL 必须被拒绝。
                let host = url::Url::parse(url).unwrap().host_str().unwrap().to_owned();
                let without = manifest
                    .permissions
                    .network
                    .iter()
                    .filter(|declared| {
                        let declared = declared.to_ascii_lowercase();
                        !(host == declared || host.ends_with(&format!(".{declared}")))
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                assert!(
                    validate_authorization_url(&manifest.id, &without, url).is_err(),
                    "{plugin}: 删掉清单里的域之后 {url} 必须被拒绝（否则这条用例证明不了什么）"
                );
            }
        }
    }

    /// 过期判定必须在夹取之前。
    #[test]
    fn an_expired_session_is_rejected_before_its_expiry_is_clamped() {
        let now = 1_000_000;

        assert_eq!(
            accept_session_expiry(now - 1, now),
            None,
            "过去的时间戳必须被明确拒绝，而不是夹成 1ms 后过期"
        );
        assert_eq!(accept_session_expiry(i64::MIN, now), None);
        assert_eq!(
            accept_session_expiry(now, now),
            None,
            "恰好等于 now 也算过期"
        );
        assert_eq!(accept_session_expiry(now + 60_000, now), Some(now + 60_000));
        assert_eq!(
            accept_session_expiry(i64::MAX, now),
            Some(now + OAUTH_SESSION_MAX_MS),
            "未来的远期时间戳才需要夹到 30 分钟"
        );
    }

    async fn cached_fingerprint(registry: &PluginRegistry) -> String {
        registry
            .inner
            .entries
            .read()
            .await
            .as_ref()
            .expect("entry cache is filled")
            .fingerprint
            .clone()
    }
}
