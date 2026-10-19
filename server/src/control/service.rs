//! Implements control API routing and shared state.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    devin::{gateway::DevinListening, DevinSettings},
    local_app::CursorHarness,
    model::{
        ContentPart, CursorRunTraceArtifact, CursorRunTraceSummary, LlmCallRequest,
        LlmCallResponseChunk, LlmCallSummary, ModelConfig, ModelConfigInput, ModelInvocation,
        ModelRequest, ModelSpec, ModelType, Overview, ProjectedContent, ProjectedMessage,
        PromptSpec, ProviderType, Role,
    },
    plugin::{PluginDescriptor, PluginRegistry, PluginRuntime, PluginRuntimeStatus},
    provider::{is_valid_response_event, ModelEvent, Provider},
    store::{
        CommitSettings, DesktopSettings, DesktopSettingsPatch, ModelTokenPricing, PortSettings,
        ProxySettings, ProxySettingsInput, StatisticsStorage, StatisticsStorageScope, Store,
        TabSettings, TokenPricingSettings,
    },
    Error, Result,
};

use super::settings::OutboundStatus;

/// 模型发现的整体上界:候选 URL 串行试、Anthropic 还要翻页,单个请求的上界盖不住总时长。
/// 超时按上游错误返回,让界面拿到真实原因,而不是自己的 15 秒超时。
const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Anthropic 模型列表翻页上限(每页 100 个):端点不守约时不能一圈圈打下去。
const ANTHROPIC_DISCOVERY_MAX_PAGES: usize = 20;

#[derive(Clone)]
pub struct ControlService {
    store: Store,
    cursor_harness: CursorHarness,
    provider: Arc<dyn Provider>,
    plugin_runtime: PluginRuntime,
    plugins: PluginRegistry,
    clients: crate::network::NetworkClients,
    /// 正在运行的 Devin 网关持有的端口。保存 Devin 设置时要靠它区分"占用者是
    /// 本应用自己"与"占用者是别的程序"；没在监听时三个端口都归别人判。
    devin_bound_ports: DevinListening,
    model_tests: ModelTests,
}

/// 正在跑的连通性测试：`test_id` → 取消令牌。
type ModelTests = Arc<Mutex<BTreeMap<String, CancellationToken>>>;

/// 一条连通性测试的登记：离开作用域（正常结束，或 handler future 被丢弃）时注销。
///
/// 过去注销语句写在 `await` 之后：页面关掉测试弹窗、前端先 abort 再 DELETE 时，handler
/// future 被丢弃，那条 `remove` 永远不会执行，条目就永久留在注册表里（而且之后的"取消"
/// 对已经断开的那次测试不再起作用）。RAII 让"断开"也走到注销。
struct TestRegistration {
    registry: ModelTests,
    test_id: String,
    cancellation: CancellationToken,
}

impl TestRegistration {
    fn register(registry: &ModelTests, test_id: &str) -> Self {
        let cancellation = CancellationToken::new();
        registry
            .lock()
            .expect("model test registry mutex poisoned")
            .insert(test_id.to_owned(), cancellation.clone());
        Self {
            registry: registry.clone(),
            test_id: test_id.to_owned(),
            cancellation,
        }
    }

    fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

impl Drop for TestRegistration {
    fn drop(&mut self) {
        let Ok(mut tests) = self.registry.lock() else {
            // 锁中毒说明别处 panic 过：drop 里不能再抛，只能放弃这一次注销。
            return;
        };
        // 只删自己登记的那一条：同一个 test_id 被后来的调用顶替时，别把新的那条删掉。
        if tests
            .get(&self.test_id)
            .is_some_and(|cancellation| cancellation == &self.cancellation)
        {
            tests.remove(&self.test_id);
        }
    }
}

/// 取消一条正在跑的连通性测试。
///
/// 只查不建：对已经不存在的测试调用取消是正常竞态（前端先 abort 再 DELETE），过去这里用
/// `entry().or_default()` 会为已消失的 id 新建一个条目并取消它，那条永远没人清理。
fn cancel_registered_test(registry: &ModelTests, test_id: &str) {
    let cancellation = registry
        .lock()
        .expect("model test registry mutex poisoned")
        .get(test_id)
        .cloned();
    if let Some(cancellation) = cancellation {
        cancellation.cancel();
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DiscoveredModels {
    pub models: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LegacyModelImportResult {
    pub imported: usize,
    pub skipped: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct LegacyModelImportPreview {
    pub source: String,
    pub total: usize,
    pub new_models: usize,
    pub existing_models: usize,
    pub models: Vec<LegacyModelImportPreviewItem>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LegacyModelImportPreviewItem {
    pub model_hash: String,
    pub display_name: String,
    pub model_id: String,
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub existing: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelDiscoveryInput {
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub base_url: String,
    pub api_key: String,
    /// 编辑已有模型时表单不回填明文 key：留空并带上模型哈希，服务端沿用已存的凭据。
    #[serde(default)]
    pub model_hash: Option<String>,
    #[serde(default)]
    pub custom_headers_enabled: bool,
    #[serde(default = "empty_json_object")]
    pub custom_headers: serde_json::Value,
}

fn empty_json_object() -> serde_json::Value {
    serde_json::json!({})
}

fn empty_json_object_ref() -> &'static serde_json::Value {
    static EMPTY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    EMPTY.get_or_init(empty_json_object)
}

/// What the management UI needs to show whether Devin can already work. The
/// listening flag comes from the gateway itself, because those ports belong to a
/// different origin and answer no browser probe.
#[derive(Clone, Debug, Serialize)]
pub struct DevinStatus {
    pub enabled: bool,
    pub listening: bool,
    pub calls: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelConnectivityResult {
    pub duration_ms: u64,
    pub first_valid_response_ms: Option<u64>,
    pub output_tokens: u64,
    pub tokens_per_second: f64,
    pub tokens_estimated: bool,
    pub output: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallDetail {
    pub call: CallSummary,
    pub request: Option<LlmCallRequest>,
    pub response_chunks: Vec<LlmCallResponseChunk>,
    pub cursor_trace: Option<CursorTraceDetail>,
    /// 正文被闸门截断过：只回了一部分，剩下的没取。
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallSummary {
    #[serde(flatten)]
    pub call: LlmCallSummary,
    pub call_kind: &'static str,
    pub route: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct CursorTraceDetail {
    pub trace: CursorRunTraceSummary,
    pub artifacts: Vec<CursorTraceArtifactDetail>,
    /// 与 `CallDetail::truncated` 同义，给只读这一块的调用方。
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CursorTraceArtifactDetail {
    pub seq: i64,
    pub artifact_type: String,
    pub source: String,
    pub metadata: serde_json::Value,
    pub created_at_ms: i64,
    pub byte_count: usize,
    pub encoding: &'static str,
    pub data: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ObservabilitySettings {
    pub detailed: bool,
}

/// 连通性测试的整体上界：超过它按"已取消"收尾（见 `collect_connectivity_stream`）。
const CONNECTIVITY_TEST_TIMEOUT: Duration = Duration::from_secs(45);

/// 一次连通性测试从流里采到的东西。
#[derive(Debug)]
struct ConnectivitySample {
    elapsed: Duration,
    first_valid_response_at: Option<Instant>,
    output_tokens: Option<u64>,
    output: String,
}

/// 在 `timeout` 内消费 provider 流，采出连通性测试要用的几个量。
///
/// 超时按 2026-10-16 的裁定收尾：取消这一轮后**立即**返回，不等流自然结束；调用记录
/// 交给 provider 流的 `CancelOnDrop` → recorder 写成"已取消"，控制面不再自己
/// `finish_llm_call`——过去这里写的是 `error/timeout`，两处抢同一行
/// （`WHERE status='running'`，先到者赢），同一个故障在调用列表里会出现两种原因。
/// 超时这个可读原因只由返回的错误文案给出（界面照文案显示）。
async fn collect_connectivity_stream<S>(
    stream: S,
    cancellation: CancellationToken,
    started: Instant,
    timeout: Duration,
) -> Result<ConnectivitySample>
where
    S: futures_util::Stream<Item = Result<ModelEvent>>,
{
    let mut first_valid_response_at = None;
    let mut output_tokens = None;
    let mut output = String::new();
    let completed = tokio::time::timeout(timeout, async {
        futures_util::pin_mut!(stream);
        let mut finished = false;
        while let Some(event) = stream.next().await {
            let event = event?;
            if first_valid_response_at.is_none() && is_valid_response_event(&event) {
                first_valid_response_at = Some(Instant::now());
            }
            match event {
                ModelEvent::TextDelta(delta) => {
                    output.push_str(&delta);
                }
                ModelEvent::Usage(usage) => {
                    if let Some(tokens) = usage.output_tokens.filter(|tokens| *tokens > 0) {
                        output_tokens =
                            Some(output_tokens.map_or(tokens, |current: u64| current.max(tokens)));
                    }
                }
                ModelEvent::Done(_) => finished = true,
                _ => {}
            }
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !finished {
            return Err(Error::Protocol(
                "provider stream ended without Done during connectivity test".into(),
            ));
        }
        Ok(())
    })
    .await;
    match completed {
        Ok(result) => result?,
        Err(_) => {
            cancellation.cancel();
            return Err(connectivity_test_timeout_error(timeout));
        }
    }
    Ok(ConnectivitySample {
        elapsed: started.elapsed(),
        first_valid_response_at,
        output_tokens,
        output,
    })
}

/// 连通性测试超时的可读原因。
fn connectivity_test_timeout_error(timeout: Duration) -> Error {
    Error::Provider(format!(
        "model connectivity test timed out after {} seconds",
        timeout.as_secs()
    ))
}

/// 发现模型时用哪份 key：编辑已有模型且表单留空时沿用服务端保存的那一份。
///
/// 表单不回填明文 key，"留空 = 不修改"是这里唯一需要判断的事。模型已经被删掉时走的也是
/// `Store::require_model` 那条统一文案（审计 11/P3-5：过去这里回 `RunNotFound`，界面会弹
/// "run not found"，用户既不知道是模型没了、也不知道该刷新列表）。
async fn discovery_api_key(store: &Store, input: &ModelDiscoveryInput) -> Result<String> {
    let Some(hash) = input
        .model_hash
        .as_deref()
        .filter(|_| input.api_key.trim().is_empty())
    else {
        return Ok(input.api_key.trim().to_owned());
    };
    Ok(store.require_model(hash).await?.api_key)
}

impl ControlService {
    pub fn new(
        store: Store,
        provider: Arc<dyn Provider>,
        plugin_runtime: PluginRuntime,
        plugins: PluginRegistry,
        clients: crate::network::NetworkClients,
        devin_bound_ports: DevinListening,
    ) -> Result<Self> {
        Ok(Self {
            cursor_harness: CursorHarness::new(store.clone())?,
            store,
            provider,
            plugin_runtime,
            plugins,
            clients,
            devin_bound_ports,
            model_tests: ModelTests::default(),
        })
    }

    pub fn cursor_harness(&self) -> &CursorHarness {
        &self.cursor_harness
    }

    pub async fn plugins(&self) -> Vec<PluginDescriptor> {
        self.plugins.plugins().await
    }

    pub async fn plugin_oauth_begin(
        &self,
        plugin_id: &str,
        resource_type: &str,
        method_id: &str,
    ) -> Result<crate::plugin::OAuthBeginResponse> {
        self.plugins
            .oauth_begin(plugin_id, resource_type, method_id)
            .await
    }

    pub async fn plugin_oauth_poll(
        &self,
        session_id: &str,
    ) -> Result<crate::plugin::OAuthPollResponse> {
        self.plugins.oauth_poll(session_id).await
    }

    pub async fn plugin_import(
        &self,
        plugin_id: &str,
        resource_type: &str,
        files: serde_json::Value,
    ) -> Result<crate::plugin::ImportResponse> {
        self.plugins
            .import_resources(plugin_id, resource_type, files)
            .await
    }

    pub async fn plugin_export_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
    ) -> Result<serde_json::Value> {
        self.plugins
            .export_resources(plugin_id, resource_type)
            .await
    }

    pub async fn plugin_refresh_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<()> {
        self.plugins
            .refresh_resource(plugin_id, resource_type, resource_id)
            .await
    }

    pub async fn plugin_resource_action(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
        action_id: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.plugins
            .resource_action(plugin_id, resource_type, resource_id, action_id, input)
            .await
    }

    pub async fn plugin_delete_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<()> {
        self.plugins
            .delete_resource(plugin_id, resource_type, resource_id)
            .await
    }

    pub async fn plugin_sync_models(&self, plugin_id: &str, provider_id: &str) -> Result<usize> {
        self.plugins.sync_models(plugin_id, provider_id).await
    }

    pub async fn plugin_set_model_enabled(
        &self,
        plugin_id: &str,
        provider_id: &str,
        model_id: &str,
        enabled: bool,
    ) -> Result<()> {
        self.plugins
            .set_model_enabled(plugin_id, provider_id, model_id, enabled)
            .await
    }

    pub async fn remove_plugin_configuration(&self, plugin_id: &str) -> Result<()> {
        self.plugins.remove(plugin_id).await
    }

    pub fn plugin_runtime_status(&self) -> PluginRuntimeStatus {
        self.plugin_runtime.status()
    }

    pub fn initialize_plugin_runtime(&self) -> PluginRuntimeStatus {
        self.plugin_runtime.initialize(self.store.clone())
    }

    pub fn cancel_plugin_runtime_initialization(&self) -> PluginRuntimeStatus {
        self.plugin_runtime.cancel_initialization()
    }

    pub async fn models(&self) -> Result<Vec<ModelConfig>> {
        self.store.models().await
    }

    pub async fn overview(
        &self,
        start_ms: Option<i64>,
        end_ms: Option<i64>,
        model_hashes: Option<&str>,
        bucket_ms: Option<i64>,
    ) -> Result<Overview> {
        self.store
            .overview(start_ms, end_ms, model_hashes, bucket_ms)
            .await
    }

    pub async fn create_models(&self, models: &[ModelConfigInput]) -> Result<Vec<ModelConfig>> {
        self.store.create_models(models).await
    }

    /// 复制一份模型配置：凭据留在服务端，由 `Store` 从源模型读回。
    pub async fn duplicate_model(
        &self,
        model_hash: &str,
        display_name: &str,
    ) -> Result<ModelConfig> {
        self.store.duplicate_model(model_hash, display_name).await
    }

    pub async fn reorder_models(&self, model_hashes: &[String]) -> Result<Vec<ModelConfig>> {
        self.store.reorder_models(model_hashes).await
    }

    pub async fn delete_model(&self, model_hash: &str) -> Result<()> {
        self.store.delete_model(model_hash).await
    }

    pub async fn update_model(
        &self,
        model_hash: &str,
        input: &ModelConfigInput,
    ) -> Result<ModelConfig> {
        self.store.update_model(model_hash, input).await
    }

    pub async fn test_model(
        &self,
        model_hash: &str,
        test_id: &str,
    ) -> Result<ModelConnectivityResult> {
        // 登记与注销成对：`TestRegistration` 在这整个 future 结束（正常返回，或客户端
        // 断开导致 handler future 被丢弃）时注销这一条。
        let registration = TestRegistration::register(&self.model_tests, test_id);
        self.run_model_test(model_hash, registration.cancellation().clone())
            .await
    }

    pub fn cancel_model_test(&self, test_id: &str) {
        cancel_registered_test(&self.model_tests, test_id);
    }

    async fn run_model_test(
        &self,
        model_hash: &str,
        cancellation: CancellationToken,
    ) -> Result<ModelConnectivityResult> {
        const TEST_PROMPT: &str = "Output the numbers 1 through 120 separated by a single space. No commas, no newlines, no explanation.";

        let mut model = ModelSpec::new(model_hash);
        if model_hash.starts_with(crate::plugin::ADAPTER_ID_PREFIX) {
            let descriptor = self.plugins.model_descriptor(model_hash).await?;
            model.display_name = Some(descriptor.display_name);
            model.max_output_tokens = Some(descriptor.max_output_tokens.unwrap_or(65_536));
        } else {
            let configured = self.store.require_model(model_hash).await?;
            configured.configure(&mut model);
            model.max_output_tokens = Some(configured.max_output_tokens().unwrap_or(65_536));
        }
        let call_id = format!("model-test-{}", uuid::Uuid::new_v4());
        let invocation = ModelInvocation {
            client_source: crate::model::CallClient::Internal,
            call_id: call_id.clone(),
            run_id: call_id.clone(),
            conversation_id: call_id.clone(),
            provider_call_index: 0,
            request: ModelRequest {
                prompt: PromptSpec {
                    instructions: String::new(),
                    tools: Vec::new(),
                },
                model,
                history: vec![ProjectedMessage {
                    message_id: "connectivity-test".into(),
                    role: Role::User,
                    content: ProjectedContent::Parts(vec![ContentPart::Text {
                        text: TEST_PROMPT.into(),
                    }]),
                }],
            },
        };
        let started = Instant::now();
        let sample = collect_connectivity_stream(
            self.provider.stream(invocation, cancellation.clone()),
            cancellation,
            started,
            CONNECTIVITY_TEST_TIMEOUT,
        )
        .await?;
        let elapsed = sample.elapsed;
        let output = sample.output.trim().to_string();
        if sample.first_valid_response_at.is_none() {
            return Err(Error::Provider(
                "model connectivity test received no valid response".into(),
            ));
        }
        let tokens_estimated = sample.output_tokens.is_none();
        let output_tokens = sample
            .output_tokens
            .unwrap_or_else(|| estimate_output_tokens(&output));
        Ok(ModelConnectivityResult {
            duration_ms: elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
            first_valid_response_ms: sample.first_valid_response_at.map(|first| {
                first
                    .duration_since(started)
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64
            }),
            output_tokens,
            tokens_per_second: if elapsed.is_zero() {
                0.0
            } else {
                output_tokens as f64 / elapsed.as_secs_f64()
            },
            tokens_estimated,
            output,
        })
    }

    pub async fn discover_models(&self, input: &ModelDiscoveryInput) -> Result<DiscoveredModels> {
        let client = self.clients.default_client().await?;
        let base_url = crate::model::normalize_request_url(&input.base_url)?;
        let api_key = discovery_api_key(&self.store, input).await?;
        let discovery = discover_models_from_endpoint(
            &client,
            match input.model_type {
                ModelType::OpenAi => ProviderType::OpenAiResponses,
                ModelType::Anthropic => ProviderType::Anthropic,
            },
            &base_url,
            &api_key,
            if input.custom_headers_enabled {
                &input.custom_headers
            } else {
                empty_json_object_ref()
            },
        );
        tokio::time::timeout(MODEL_DISCOVERY_TIMEOUT, discovery)
            .await
            .map_err(|_| {
                Error::Provider(format!(
                    "model discovery timed out after {MODEL_DISCOVERY_TIMEOUT:?}"
                ))
            })?
    }

    pub async fn import_v0049_models(&self) -> Result<LegacyModelImportResult> {
        let path = crate::config::v0049_config_path()?;
        let outcome = self.store.import_v0049_model_config(&path).await?;
        Ok(LegacyModelImportResult {
            imported: outcome.imported,
            skipped: outcome.skipped,
            total: outcome.total,
        })
    }

    pub async fn preview_v0049_models(&self) -> Result<LegacyModelImportPreview> {
        let path = crate::config::v0049_config_path()?;
        let plan = self.store.preview_v0049_model_config(&path).await?;
        let total = plan.models.len();
        let existing_models = plan.models.iter().filter(|model| model.existing).count();
        Ok(LegacyModelImportPreview {
            source: path.display().to_string(),
            total,
            new_models: total - existing_models,
            existing_models,
            models: plan
                .models
                .into_iter()
                .map(|model| LegacyModelImportPreviewItem {
                    model_hash: model.model_hash,
                    display_name: model.input.display_name,
                    model_id: model.input.model_id,
                    model_type: model.input.model_type,
                    existing: model.existing,
                })
                .collect(),
        })
    }

    /// Counts the calls Devin produced, which is how the UI knows whether a real
    /// conversation has travelled through the gateway yet.
    pub async fn devin_status(&self, listening: bool) -> Result<DevinStatus> {
        let settings = self.store.devin_settings().await?;
        let calls = self.store.devin_call_count().await?;
        Ok(DevinStatus {
            enabled: settings.enabled,
            listening,
            calls,
        })
    }

    pub async fn calls(&self, limit: i64) -> Result<Vec<CallSummary>> {
        let mut calls = self
            .store
            .llm_calls(limit)
            .await?
            .into_iter()
            .map(|call| CallSummary {
                call,
                call_kind: "provider_llm",
                route: "local_byok",
            })
            .collect::<Vec<_>>();
        calls.extend(
            self.store
                .official_cursor_traces(limit)
                .await?
                .into_iter()
                .map(official_call),
        );
        calls.sort_by_key(|call| std::cmp::Reverse(call.call.created_at_ms));
        calls.truncate(limit.clamp(1, 500) as usize);
        Ok(calls)
    }

    pub async fn call(&self, call_id: &str) -> Result<CallDetail> {
        if let Some(call) = self.store.llm_call(call_id).await? {
            let cursor_trace = self.cursor_trace_detail(&call.run_id).await?;
            let trace_truncated = cursor_trace.as_ref().is_some_and(|trace| trace.truncated);
            let (response_chunks, chunks_truncated) = clip_to_budget(
                self.store.llm_call_chunks(call_id).await?,
                CALL_DETAIL_BUDGET,
                |chunk| chunk.data.len(),
            );
            return Ok(CallDetail {
                request: self.store.llm_call_request(call_id).await?,
                response_chunks,
                truncated: chunks_truncated || trace_truncated,
                call: CallSummary {
                    call,
                    call_kind: "provider_llm",
                    route: "local_byok",
                },
                cursor_trace,
            });
        }
        let request_id = call_id.strip_prefix("cursor:").unwrap_or(call_id);
        let trace = self
            .store
            .cursor_trace(request_id)
            .await?
            .filter(|trace| trace.route == "cursor_official")
            .ok_or_else(|| Error::RunNotFound(format!("call {call_id}")))?;
        let cursor_trace = self.cursor_trace_detail_from(trace.clone()).await?;
        Ok(CallDetail {
            call: official_call(trace),
            request: None,
            response_chunks: Vec::new(),
            truncated: cursor_trace.truncated,
            cursor_trace: Some(cursor_trace),
        })
    }

    async fn cursor_trace_detail(&self, request_id: &str) -> Result<Option<CursorTraceDetail>> {
        let Some(trace) = self.store.cursor_trace(request_id).await? else {
            return Ok(None);
        };
        Ok(Some(self.cursor_trace_detail_from(trace).await?))
    }

    async fn cursor_trace_detail_from(
        &self,
        trace: CursorRunTraceSummary,
    ) -> Result<CursorTraceDetail> {
        let (artifacts, truncated) = clip_to_budget(
            self.store.cursor_trace_artifacts(&trace.request_id).await?,
            CALL_DETAIL_BUDGET,
            |artifact| artifact.data.len(),
        );
        Ok(CursorTraceDetail {
            trace,
            artifacts: artifacts.into_iter().map(cursor_artifact).collect(),
            truncated,
        })
    }

    pub async fn observability(&self) -> Result<ObservabilitySettings> {
        Ok(ObservabilitySettings {
            detailed: self.store.detailed_logging().await?,
        })
    }

    pub async fn set_observability(
        &self,
        settings: ObservabilitySettings,
    ) -> Result<ObservabilitySettings> {
        self.store.set_detailed_logging(settings.detailed).await?;
        Ok(settings)
    }

    pub async fn ports(&self) -> Result<PortSettings> {
        self.store.port_settings().await
    }

    pub async fn devin_settings(&self) -> Result<DevinSettings> {
        self.store.devin_settings().await
    }

    /// 保存 Devin 设置。端口占用在这里拦下：网关只在启动时绑一次端口，没有重试与
    /// 回退，被占用时只会表现为"已保存、未监听、待重启"，重启也救不回来。
    pub async fn set_devin_settings(&self, settings: DevinSettings) -> Result<DevinSettings> {
        super::devin::save_settings(&self.store, settings, self.devin_bound_ports.bound_ports())
            .await
    }

    /// 本进程正在监听的 Devin 端口。宿主补丁要指向真正在监听的端口，
    /// 否则客户端会打到没人接的地址上。
    pub fn devin_bound_ports(&self) -> DevinListening {
        self.devin_bound_ports.clone()
    }

    pub async fn set_ports(&self, settings: PortSettings) -> Result<PortSettings> {
        self.store.set_port_settings(settings).await?;
        Ok(settings)
    }

    pub async fn statistics_storage(&self) -> Result<StatisticsStorage> {
        self.store.statistics_storage().await
    }

    /// 开始一次清理：立刻返回"已在清理"，不在这里等全部批次跑完。
    ///
    /// 删除本身跑在 store 的后台任务里（每批一个事务、批间放掉写锁），接口只负责把任务
    /// 启动起来并把此刻的进度带回去——库大的时候几百个批次要跑几十秒，同步等的那个请求
    /// 会先撞上桌面端 15 秒超时，用户看到的是"清理失败"，而库其实正在被清。
    pub async fn start_clear_statistics_storage(
        &self,
        scope: StatisticsStorageScope,
    ) -> Result<StatisticsStorage> {
        self.store.start_clear_statistics_storage(scope).await
    }

    pub async fn proxy_settings(&self) -> Result<ProxySettings> {
        self.store.proxy_settings().await
    }

    /// 出网此刻实际走哪条路。设置页用它说明"正在绕过系统代理"这类与配置不一致的状态。
    pub async fn outbound_status(&self) -> Result<OutboundStatus> {
        Ok(OutboundStatus {
            system_proxy_bypassed: crate::network::system_proxy_bypassed(),
        })
    }

    pub async fn set_proxy_settings(&self, settings: ProxySettingsInput) -> Result<ProxySettings> {
        if settings.mode.is_custom() {
            let local_proxy_port = match self.cursor_harness.proxy_port().await {
                Some(port) => port,
                None => self.store.port_settings().await?.proxy_port,
            };
            crate::network::reject_self_proxy(&settings.address, local_proxy_port)?;
        }
        let settings = self.store.set_proxy_settings(settings).await?;
        self.clients.settings_saved().await;
        Ok(settings)
    }

    pub async fn tab_settings(&self) -> Result<TabSettings> {
        self.store.tab_settings().await
    }

    pub async fn set_tab_settings(&self, settings: TabSettings) -> Result<TabSettings> {
        self.cursor_harness.set_tab_settings(settings).await
    }

    pub async fn desktop_settings(&self) -> Result<DesktopSettings> {
        self.store.desktop_settings().await
    }

    /// 桌面设置按字段更新：界面每次只提交改动的字段，合并由 store 在写锁内完成。
    pub async fn update_desktop_settings(
        &self,
        patch: DesktopSettingsPatch,
    ) -> Result<DesktopSettings> {
        self.store.update_desktop_settings(patch).await
    }

    pub async fn commit_settings(&self) -> Result<CommitSettings> {
        self.store.commit_settings().await
    }

    pub async fn set_commit_settings(&self, settings: CommitSettings) -> Result<CommitSettings> {
        self.store.set_commit_settings(settings).await
    }

    pub async fn compaction_settings(&self) -> Result<crate::store::CompactionSettings> {
        self.store.compaction_settings().await
    }

    pub async fn set_compaction_settings(
        &self,
        settings: crate::store::CompactionSettings,
    ) -> Result<crate::store::CompactionSettings> {
        self.store.set_compaction_settings(settings).await
    }

    pub async fn pricing_settings(&self) -> Result<TokenPricingSettings> {
        self.store.pricing_settings().await
    }

    /// 保存一个模型的价格规则，返回合并后的完整规则表。
    pub async fn set_model_pricing(
        &self,
        model_hash: &str,
        pricing: ModelTokenPricing,
    ) -> Result<TokenPricingSettings> {
        self.store.set_model_pricing(model_hash, pricing).await
    }

    /// 删除一个模型的价格规则，返回合并后的完整规则表。
    pub async fn remove_model_pricing(&self, model_hash: &str) -> Result<TokenPricingSettings> {
        self.store.remove_model_pricing(model_hash).await
    }
}

fn official_call(trace: CursorRunTraceSummary) -> CallSummary {
    let model_id = trace.model_id.clone().unwrap_or_else(|| "Cursor".into());
    let ttfb = trace
        .first_response_at_ms
        .map(|value| (value - trace.received_at_ms).max(0));
    let duration = trace
        .finished_at_ms
        .map(|value| (value - trace.received_at_ms).max(0));
    let error = trace.error_message.clone();
    CallSummary {
        call: LlmCallSummary {
            client_source: "cursor".into(),
            call_id: format!("cursor:{}", trace.request_id),
            run_id: trace.request_id.clone(),
            conversation_id: trace
                .conversation_id
                .clone()
                .unwrap_or_else(|| trace.request_id.clone()),
            provider_call_index: 0,
            model_hash: None,
            provider_type: "cursor-official".into(),
            provider_url: "https://api2.cursor.sh".into(),
            request_type: "cursor-run-sse".into(),
            request_url: "https://api2.cursor.sh/agent.v1.AgentService/RunSSE".into(),
            model_id: model_id.clone(),
            display_name: model_id,
            reasoning_effort: None,
            fast: None,
            status: trace.status.clone(),
            finish_reason: None,
            created_at_ms: trace.received_at_ms,
            request_started_at_ms: Some(trace.received_at_ms),
            response_headers_at_ms: trace.first_response_at_ms,
            first_event_at_ms: trace.first_response_at_ms,
            first_text_at_ms: None,
            first_valid_response_at_ms: None,
            finished_at_ms: trace.finished_at_ms,
            queue_ms: None,
            ttfb_ms: ttfb,
            ttft_ms: None,
            ttfr_ms: None,
            duration_ms: duration,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            usage: None,
            message_count: 0,
            tool_count: 0,
            request_bytes: Some(trace.request_bytes),
            response_bytes: trace.response_bytes,
            stream_event_count: trace.response_event_count,
            http_status: trace.http_status,
            error_kind: error.as_ref().map(|_| "cursor_official".into()),
            error_message: error,
            detailed: true,
        },
        call_kind: "cursor_official",
        route: "cursor_official",
    }
}

/// 一次调用详情响应的正文预算。
///
/// 详情页把响应原样读进内存：前端还要 `JSON.parse` + `JSON.stringify(…, null, 2)` 再交给
/// Monaco 建全文模型，服务端 serialize 一次也是同一量级。以前这里没有任何上限——单次请求
/// 的轨迹工件与响应分片全量取回，3 天保留窗口内的一次大请求就够让详情窗口在主线程上长
/// 时间无响应。闸门按"条数 + 总字节"两道，截断时在响应里标 `truncated`：用户至少知道看到
/// 的不是全部，而不是以为详情就这么点。
#[derive(Clone, Copy)]
struct DetailBudget {
    items: usize,
    bytes: usize,
}

/// 500 条、4 MiB 原始正文。base64 会放大 4/3，所以真实响应体在这个数的 1.34 倍以内；
/// 正文远超这个量级的调用，详情页本来也读不完。轨迹工件与响应分片各用一份预算，整个详情
/// 响应因此被限制在 8 MiB 正文（加一份请求体）以内。
const CALL_DETAIL_BUDGET: DetailBudget = DetailBudget {
    items: 500,
    bytes: 4 * 1024 * 1024,
};

/// 按预算截断正文列表，返回保留下来的部分与"是否截断过"。
///
/// 用原始字节而不是序列化后的长度：`data.len()` 已经在手里，而且放大倍数固定，不必为了
/// 量一遍再序列化一次。
fn clip_to_budget<T>(
    items: Vec<T>,
    budget: DetailBudget,
    length_of: impl Fn(&T) -> usize,
) -> (Vec<T>, bool) {
    let mut kept = Vec::new();
    let mut bytes = 0usize;
    for item in items {
        if kept.len() >= budget.items || bytes.saturating_add(length_of(&item)) > budget.bytes {
            return (kept, true);
        }
        bytes += length_of(&item);
        kept.push(item);
    }
    (kept, false)
}

fn cursor_artifact(artifact: CursorRunTraceArtifact) -> CursorTraceArtifactDetail {
    let byte_count = artifact.data.len();
    let (encoding, data) = match readable_utf8(&artifact.data) {
        Some(value) => ("utf8", value.into()),
        None => ("base64", STANDARD.encode(&artifact.data)),
    };
    CursorTraceArtifactDetail {
        seq: artifact.seq,
        artifact_type: artifact.artifact_type,
        source: artifact.source,
        metadata: artifact.metadata,
        created_at_ms: artifact.created_at_ms,
        byte_count,
        encoding,
        data,
    }
}

fn readable_utf8(data: &[u8]) -> Option<&str> {
    let value = std::str::from_utf8(data).ok()?;
    value
        .chars()
        .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .then_some(value)
}

async fn discover_models_from_endpoint(
    client: &reqwest::Client,
    provider_type: ProviderType,
    base_url: &str,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<DiscoveredModels> {
    let mut models = match provider_type {
        ProviderType::OpenAiChat | ProviderType::OpenAiResponses => {
            openai_models(client, base_url, api_key, custom_headers).await?
        }
        ProviderType::Anthropic => {
            anthropic_models(client, base_url, api_key, custom_headers).await?
        }
        ProviderType::Plugin => {
            return Err(Error::Config(
                "plugin providers discover models through their plugin".into(),
            ))
        }
    };
    models.sort();
    models.dedup();
    Ok(DiscoveredModels { models })
}

fn model_discovery_url(base_url: &str) -> Result<Url> {
    let mut url = Url::parse(base_url)
        .map_err(|error| Error::Config(format!("invalid model request URL: {error}")))?;
    if url.host_str().is_none() {
        return Err(Error::Config(
            "model request URL must contain a host".into(),
        ));
    }
    // 在现有路径上追加，而不是整段替换：多数编程套餐的 API 挂在子路径下
    // （/api/anthropic、/coding、/api/paas/v4 等），直接 set_path("/v1/models")
    // 会把这些前缀吃掉，发现请求必然 404
    let path = url.path().trim_end_matches('/');
    let last = path.rsplit('/').next().unwrap_or("");
    let versioned = last.len() > 1
        && last.starts_with('v')
        && last[1..].bytes().all(|byte| byte.is_ascii_digit());
    let new_path = if let Some(parent) = path.strip_suffix("/chat/completions") {
        // 完整请求 URL：剥掉端点段（chat/completions 是两段），换成 models
        format!("{parent}/models")
    } else if let Some(parent) = path
        .strip_suffix("/responses")
        .or_else(|| path.strip_suffix("/messages"))
        .or_else(|| path.strip_suffix("/completions"))
    {
        format!("{parent}/models")
    } else if path.is_empty() {
        "/v1/models".to_string()
    } else if versioned {
        // 已带版本段（/v1、/api/v3、/api/paas/v4）：只补 models
        format!("{path}/models")
    } else {
        format!("{path}/v1/models")
    };
    url.set_path(&new_path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn model_discovery_urls(base_url: &str) -> Result<Vec<Url>> {
    let mut configured = Url::parse(base_url)
        .map_err(|error| Error::Config(format!("invalid model request URL: {error}")))?;
    let path = configured.path().trim_end_matches('/');
    let tail = path.rsplit('/').next().unwrap_or_default();
    if matches!(tail.to_ascii_lowercase().as_str(), "model" | "models") {
        configured.set_query(None);
        configured.set_fragment(None);
        return Ok(vec![configured]);
    }

    let primary = model_discovery_url(base_url)?;
    let versioned = tail.len() > 1
        && tail.starts_with('v')
        && tail[1..].bytes().all(|byte| byte.is_ascii_digit());
    let complete_request_url = [
        "/chat/completions",
        "/responses",
        "/messages",
        "/completions",
    ]
    .iter()
    .any(|suffix| path.to_ascii_lowercase().ends_with(suffix));
    if versioned || complete_request_url {
        return Ok(vec![primary]);
    }

    let Some(prefix) = primary.path().strip_suffix("/v1/models") else {
        return Ok(vec![primary]);
    };
    let mut fallback = primary.clone();
    fallback.set_path(&format!("{prefix}/models"));
    Ok(vec![primary, fallback])
}

async fn openai_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut last_error = None;
    for url in model_discovery_urls(base_url)? {
        match openai_models_at(client, url, api_key, custom_headers).await {
            Ok(models) => return Ok(models),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| Error::Provider("no model discovery URL available".into())))
}

async fn openai_models_at(
    client: &reqwest::Client,
    url: Url,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut request = client.get(url);
    if !api_key.is_empty() {
        request = request.bearer_auth(api_key);
    }
    let response = apply_discovery_headers(request, custom_headers)?
        .send()
        .await?;
    let (status, body) = read_discovery_body(response).await?;
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "model discovery failed ({status}): {body}"
        )));
    }
    Ok(model_ids(body.get("data").unwrap_or(&body)))
}

/// 读一次模型发现响应：先拿状态码与原始字节，再按需解析 JSON。
///
/// 端点回 HTML 404、网关错误页或空体时，`response.json()` 会先失败——状态码与真实正文
/// 一起丢掉，报错退化成 `http error: error decoding response body`，而"404 + 上游正文"
/// 恰恰是排查 base_url 填错/被反代挡住最有用的信息（审计 11/P3-3）。
async fn read_discovery_body(
    response: reqwest::Response,
) -> Result<(reqwest::StatusCode, serde_json::Value)> {
    let status = response.status();
    let bytes = response.bytes().await?;
    decode_discovery_body(status, &bytes)
}

/// 正文解码：成功给 JSON，失败给"状态码 + 正文前缀"的可读错误。
fn decode_discovery_body(
    status: reqwest::StatusCode,
    bytes: &[u8],
) -> Result<(reqwest::StatusCode, serde_json::Value)> {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(body) => Ok((status, body)),
        Err(error) => Err(Error::Provider(format!(
            "model discovery failed ({status}): {error}; body starts with {}",
            body_prefix(bytes)
        ))),
    }
}

/// 错误正文的前若干字符：要能看出上游到底回了什么，又不把整页 HTML 搬进界面与日志。
fn body_prefix(bytes: &[u8]) -> String {
    const PREFIX_CHARS: usize = 200;
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "<empty body>".to_string();
    }
    let prefix = trimmed.chars().take(PREFIX_CHARS).collect::<String>();
    if trimmed.chars().count() > PREFIX_CHARS {
        format!("{prefix}…")
    } else {
        prefix
    }
}

async fn anthropic_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut last_error = None;
    for url in model_discovery_urls(base_url)? {
        match anthropic_models_at(client, url, api_key, custom_headers).await {
            Ok(models) => return Ok(models),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| Error::Provider("no model discovery URL available".into())))
}

async fn anthropic_models_at(
    client: &reqwest::Client,
    url: Url,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut after_id = None::<String>;
    let mut found = BTreeSet::new();
    let mut pages = 0usize;
    loop {
        pages += 1;
        let mut request = client
            .get(url.clone())
            .query(&[("limit", "100")])
            .header("anthropic-version", "2023-06-01");
        if !api_key.is_empty() {
            request = request.header("x-api-key", api_key);
        }
        if let Some(after_id) = &after_id {
            request = request.query(&[("after_id", after_id)]);
        }
        let response = apply_discovery_headers(request, custom_headers)?
            .send()
            .await?;
        let (status, body) = read_discovery_body(response).await?;
        if !status.is_success() {
            return Err(Error::Provider(format!(
                "model discovery failed ({status}): {body}"
            )));
        }
        found.extend(model_ids(body.get("data").unwrap_or(&body)));
        let Some(next) = next_anthropic_page(&body, after_id.as_deref(), pages)? else {
            break;
        };
        after_id = Some(next);
    }
    Ok(found.into_iter().collect())
}

/// 一页之后要不要继续翻。端点不守约时(一直 `has_more`、`last_id` 不前进、翻过页数上限)
/// 直接报错:否则每一圈都是一次真实上游请求,发现模型这个接口永远不返回。
fn next_anthropic_page(
    body: &serde_json::Value,
    after_id: Option<&str>,
    pages: usize,
) -> Result<Option<String>> {
    if body.get("has_more").and_then(serde_json::Value::as_bool) != Some(true) {
        return Ok(None);
    }
    if pages >= ANTHROPIC_DISCOVERY_MAX_PAGES {
        return Err(Error::Provider(format!(
            "Anthropic model discovery exceeded {ANTHROPIC_DISCOVERY_MAX_PAGES} pages"
        )));
    }
    let Some(next) = body.get("last_id").and_then(serde_json::Value::as_str) else {
        return Err(Error::Provider(
            "Anthropic model response has_more without last_id".into(),
        ));
    };
    if after_id == Some(next) {
        return Err(Error::Provider(
            "Anthropic model discovery did not advance last_id".into(),
        ));
    }
    Ok(Some(next.to_owned()))
}

fn model_ids(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| match item {
            serde_json::Value::String(id) => Some(id.clone()),
            serde_json::Value::Object(object) => object
                .get("id")
                .or_else(|| object.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .collect()
}

fn estimate_output_tokens(output: &str) -> u64 {
    let words = output.split_whitespace().count() as u64;
    if words > 0 {
        words
    } else if output.is_empty() {
        0
    } else {
        (output.chars().count() as u64).div_ceil(4)
    }
}

fn apply_discovery_headers(
    mut request: reqwest::RequestBuilder,
    headers: &serde_json::Value,
) -> Result<reqwest::RequestBuilder> {
    let object = headers
        .as_object()
        .ok_or_else(|| Error::Config("custom headers must be an object".into()))?;
    for (name, value) in object {
        if name.eq_ignore_ascii_case("user-agent") {
            continue;
        }
        let value = value
            .as_str()
            .ok_or_else(|| Error::Config(format!("custom header {name} must be a string")))?;
        let name = HeaderName::try_from(name)
            .map_err(|error| Error::Config(format!("invalid header name: {error}")))?;
        let value = HeaderValue::try_from(value)
            .map_err(|error| Error::Config(format!("invalid header value: {error}")))?;
        request = request.header(name, value);
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(has_more: bool, last_id: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "data": [{"id": "claude-sonnet-4"}],
            "has_more": has_more,
            "last_id": last_id,
        })
    }

    /// 端点不守约时翻页必须自己收住,否则发现模型永远不返回。
    #[test]
    fn anthropic_pagination_refuses_an_endpoint_that_never_advances() {
        // 最后一页。
        assert_eq!(
            next_anthropic_page(&page(false, None), None, 1).unwrap(),
            None
        );
        // 正常翻页。
        assert_eq!(
            next_anthropic_page(&page(true, Some("b")), Some("a"), 2).unwrap(),
            Some("b".to_owned())
        );
        // 回了 has_more 却不给 last_id。
        assert!(next_anthropic_page(&page(true, None), Some("a"), 2).is_err());
        // last_id 不前进:再翻就是打同一个端点。
        assert!(next_anthropic_page(&page(true, Some("a")), Some("a"), 2).is_err());
        // 翻过页数上限。
        assert!(next_anthropic_page(
            &page(true, Some("b")),
            Some("a"),
            ANTHROPIC_DISCOVERY_MAX_PAGES
        )
        .is_err());
    }

    /// 调用详情必须有两道闸门：条目数与总字节，并且如实报告截断。
    ///
    /// 旧行为是把单次请求的全部轨迹工件与响应分片无上限地塞进一个响应，前端拿到之后
    /// 还要再序列化两遍（见 `CALL_DETAIL_BUDGET` 的说明）。
    #[test]
    fn the_call_detail_budget_clips_the_body_and_reports_it() {
        let budget = DetailBudget {
            items: 3,
            bytes: 10,
        };

        // 没超预算：原样返回，不能谎报截断。
        let (kept, truncated) = clip_to_budget(vec![2, 3], budget, |size| *size as usize);
        assert_eq!(kept, vec![2, 3]);
        assert!(!truncated);

        // 条目闸门。
        let (kept, truncated) = clip_to_budget(vec![1, 1, 1, 1], budget, |size| *size as usize);
        assert_eq!(kept, vec![1, 1, 1]);
        assert!(truncated);

        // 字节闸门：一条超大的正文不能因为"排在第一"就整个塞进去。
        let (kept, truncated) = clip_to_budget(vec![1, 1, 9, 1], budget, |size| *size as usize);
        assert_eq!(kept, vec![1, 1]);
        assert!(truncated);

        // 第一条就超预算：回空列表也要标截断，页面才知道"不是没有正文"。
        let (kept, truncated) = clip_to_budget(vec![11], budget, |size| *size as usize);
        assert!(kept.is_empty());
        assert!(truncated);
    }

    /// 连通性测试的登记必须跟着 future 走：客户端断开（handler future 被丢弃）时也要注销。
    ///
    /// 这就是审计 11/P3-1 要的"客户端中途断开"夹具——`tokio::spawn` 出来的任务被 abort
    /// 时，它的 future 在挂起点被丢弃，和 hyper 在连接关闭时丢弃 handler future 是同一个
    /// 动作。登记用一次握手确认（不靠"让出若干次调度"这种猜测），注销靠 join 收尾确认，
    /// 每一步都只碰自己这份 registry，并行跑多少用例都不影响它。
    #[tokio::test]
    async fn dropping_the_handler_future_unregisters_the_test() {
        let tests = ModelTests::default();
        let (registered, wait_registered) = tokio::sync::oneshot::channel::<()>();
        let handler = tokio::spawn({
            let tests = tests.clone();
            async move {
                let _registration = TestRegistration::register(&tests, "aborted-test");
                let _ = registered.send(());
                // 卡在挂起点：只有被丢弃才会离开这里。
                futures_util::future::pending::<()>().await;
            }
        });
        wait_registered.await.expect("handler 必须先完成登记");
        assert_eq!(
            tests.lock().unwrap().keys().collect::<Vec<_>>(),
            vec!["aborted-test"],
            "登记应当已经建立"
        );

        handler.abort();
        // join 收尾即任务与它的守卫都已经 drop，不能只 abort 不等。
        let _ = handler.await;
        assert!(
            tests.lock().unwrap().is_empty(),
            "断开之后注册表必须回到空，不能留下永久条目"
        );
    }

    /// 登记守卫只注销自己那一条：同一个 test_id 被后来的调用顶替时，旧守卫不许删掉新的。
    #[test]
    fn the_registration_guard_only_removes_its_own_entry() {
        let tests = ModelTests::default();
        let first = TestRegistration::register(&tests, "same-id");
        assert_eq!(tests.lock().unwrap().len(), 1);
        drop(first);
        assert!(tests.lock().unwrap().is_empty(), "离开作用域即注销自己那条");

        let stale = TestRegistration::register(&tests, "same-id");
        let replacement = TestRegistration::register(&tests, "same-id");
        let replacement_token = replacement.cancellation().clone();
        // 显式按"旧的先 drop"来验证：守卫认的是自己手里的令牌，而不是这个 id。
        drop(stale);
        assert_eq!(
            tests
                .lock()
                .unwrap()
                .get("same-id")
                .cloned()
                .map(|token| token == replacement_token),
            Some(true),
            "顶替上来的那条必须还在"
        );
        drop(replacement);
        assert!(
            tests.lock().unwrap().is_empty(),
            "最后一条离开作用域后注册表必须清空"
        );
    }

    /// 取消只查不建：对已经不存在的测试调用取消是正常竞态，不能凭空登记一条。
    #[test]
    fn cancelling_a_test_only_touches_an_existing_registration() {
        let tests = ModelTests::default();
        cancel_registered_test(&tests, "never-registered");
        assert!(tests.lock().unwrap().is_empty(), "取消未知 id 不该留下条目");

        let registration = TestRegistration::register(&tests, "live-test");
        cancel_registered_test(&tests, "live-test");
        assert!(registration.cancellation().is_cancelled());
    }

    /// 裁定（11/P3-2）：超时按"已取消"收尾——控制面不写调用记录、立即返回、文案里带超时。
    ///
    /// 这里用真实的 recorder（它自己会建那条 running 记录）加一条永不产出事件的流：过去
    /// 超时分支会抢在 recorder 前面把记录写成 `error/timeout`，而且还要等流自然结束。
    #[tokio::test]
    async fn a_timed_out_connectivity_test_leaves_the_record_to_the_recorder() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let call_id = "model-test-timeout".to_owned();
        let recorder = crate::provider::CallRecorder::start(
            store.clone(),
            crate::model::NewLlmCall {
                client_source: crate::model::CallClient::Internal,
                call_id: call_id.clone(),
                run_id: call_id.clone(),
                conversation_id: call_id.clone(),
                provider_call_index: 0,
                model_hash: "test-model".into(),
                provider_type: ProviderType::OpenAiChat,
                provider_url: "http://127.0.0.1:1".into(),
                request_type: ProviderType::OpenAiChat,
                request_url: "http://127.0.0.1:1/v1/chat/completions".into(),
                model_id: "test-model".into(),
                display_name: "test-model".into(),
                reasoning_effort: None,
                fast: false,
                message_count: 1,
                tool_count: 0,
                detailed: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            store.llm_call(&call_id).await.unwrap().unwrap().status,
            "running"
        );

        let cancellation = CancellationToken::new();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            collect_connectivity_stream(
                futures_util::stream::pending::<Result<ModelEvent>>(),
                cancellation.clone(),
                Instant::now(),
                Duration::from_millis(50),
            ),
        )
        .await
        .expect("超时必须收尾，不能等流自然结束")
        .unwrap_err();
        assert!(
            error.to_string().contains("timed out after 0 seconds")
                || error.to_string().contains("timed out after"),
            "可读原因必须带超时: {error}"
        );
        assert!(cancellation.is_cancelled(), "超时必须取消这一轮");
        assert_eq!(
            store.llm_call(&call_id).await.unwrap().unwrap().status,
            "running",
            "控制面不再写第二次记录，这条留给 recorder 收尾"
        );

        // recorder 的 CancelOnDrop 走的就是这个入口，落库结果是"已取消"。
        assert!(!recorder.is_finished());
        recorder.cancelled().await.unwrap();
        let recorded = store.llm_call(&call_id).await.unwrap().unwrap();
        assert_eq!(recorded.status, "cancelled");
    }

    /// 成功路径照旧：采样带出首字时间、token 数与正文。
    #[tokio::test]
    async fn a_completed_connectivity_test_samples_the_stream() {
        let events = vec![
            Ok(ModelEvent::TextDelta("1 2 3".into())),
            Ok(ModelEvent::Usage(crate::model::Usage {
                output_tokens: Some(3),
                ..Default::default()
            })),
            Ok(ModelEvent::Done(crate::provider::FinishReason::Stop)),
        ];
        let sample = collect_connectivity_stream(
            futures_util::stream::iter(events),
            CancellationToken::new(),
            Instant::now(),
            CONNECTIVITY_TEST_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(sample.output, "1 2 3");
        assert_eq!(sample.output_tokens, Some(3));
        assert!(sample.first_valid_response_at.is_some());
    }

    /// 流没给 Done 就结束仍然是协议错误，不能当成测试成功。
    #[tokio::test]
    async fn a_stream_without_done_stays_a_protocol_error() {
        let events = vec![Ok(ModelEvent::TextDelta("1 2 3".into()))];
        let error = collect_connectivity_stream(
            futures_util::stream::iter(events),
            CancellationToken::new(),
            Instant::now(),
            CONNECTIVITY_TEST_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("without Done"), "{error}");
    }

    /// 发现模型时回填凭据（审计 11/P3-5）：留空沿用已存的那份；模型没了要报"配置问题 +
    /// 刷新模型列表"，不能是 `run not found`（界面会把 RunNotFound 原样弹出来）。
    #[tokio::test]
    async fn the_discovery_key_falls_back_to_the_saved_one_and_reports_a_missing_model() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("discovery.db").display()
        ))
        .await
        .unwrap();
        let model = store
            .create_model(&ModelConfigInput {
                sort_order: 0,
                display_name: "Discovery".into(),
                group_name: None,
                model_type: ModelType::OpenAi,
                base_url: "https://example.com/v1/chat/completions".into(),
                use_full_url: true,
                api_key: "saved-key".into(),
                tooltip_data: "Discovery".into(),
                model_id: "discovery".into(),
                reasoning_effort: None,
                openai_endpoint: crate::model::OPENAI_CHAT_ENDPOINT.into(),
                openai_extra_params_enabled: false,
                openai_extra_params: serde_json::json!({}),
                custom_headers_enabled: false,
                custom_headers: serde_json::json!({}),
                anthropic_extra_params_enabled: false,
                anthropic_extra_params: serde_json::json!({}),
                context_window_tokens: None,
                max_completion_tokens: None,
                anthropic_max_tokens: None,
                anthropic_thinking_effort: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap();
        let input = |api_key: &str, model_hash: Option<&str>| ModelDiscoveryInput {
            model_type: ModelType::OpenAi,
            base_url: "https://api.example.com/v1".into(),
            api_key: api_key.into(),
            model_hash: model_hash.map(str::to_owned),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
        };

        // 留空 + 带哈希：沿用服务端保存的那份（表单不回填明文 key）。
        assert_eq!(
            discovery_api_key(&store, &input("", Some(&model.model_hash)))
                .await
                .unwrap(),
            "saved-key"
        );
        // 表单给了 key：用它，不去查库。
        assert_eq!(
            discovery_api_key(&store, &input(" typed-key ", Some(&model.model_hash)))
                .await
                .unwrap(),
            "typed-key"
        );
        // 新建（没有哈希）：空 key 就是空。
        assert_eq!(
            discovery_api_key(&store, &input("", None)).await.unwrap(),
            ""
        );
        // 模型没了：统一的配置问题文案，界面能给出下一步。
        let error = discovery_api_key(&store, &input("", Some("gone-model")))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Config(_)), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("gone-model"), "{message}");
        assert!(message.contains("reload the model list"), "{message}");
        assert!(!message.contains("run not found"), "{message}");
    }

    /// 发现 URL 的构造与回退（审计 11/P3-7）：子路径、版本段、完整端点、无路径、带 query。
    #[test]
    fn the_discovery_url_follows_the_configured_base_path() {
        for (base, expected) in [
            // 无路径：补 /v1/models。
            (
                "https://api.example.com",
                "https://api.example.com/v1/models",
            ),
            // 已带版本段：只补 models。
            (
                "https://api.example.com/v1",
                "https://api.example.com/v1/models",
            ),
            // 编程套餐的子路径必须保留。
            (
                "https://api.example.com/api/anthropic",
                "https://api.example.com/api/anthropic/v1/models",
            ),
            (
                "https://api.example.com/api/paas/v4/",
                "https://api.example.com/api/paas/v4/models",
            ),
            // 完整请求端点：剥掉端点段换成 models。
            (
                "https://api.example.com/v1/chat/completions",
                "https://api.example.com/v1/models",
            ),
            (
                "https://api.example.com/v1/messages",
                "https://api.example.com/v1/models",
            ),
            (
                "https://api.example.com/v1/responses",
                "https://api.example.com/v1/models",
            ),
            // query/fragment 不进发现请求。
            (
                "https://api.example.com/v1?key=1#frag",
                "https://api.example.com/v1/models",
            ),
        ] {
            assert_eq!(
                model_discovery_url(base).unwrap().as_str(),
                expected,
                "{base}"
            );
        }
        assert!(model_discovery_url("not a url").is_err());
        assert!(model_discovery_url("file:///tmp/models").is_err());
    }

    /// 候选顺序与回退：只有"没有版本段也没有完整端点"时才补试一次不带 /v1 的路径。
    #[test]
    fn the_discovery_candidates_fall_back_only_for_a_bare_base_url() {
        let candidates = |base: &str| {
            model_discovery_urls(base)
                .unwrap()
                .into_iter()
                .map(|url| url.as_str().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            candidates("https://api.example.com"),
            vec![
                "https://api.example.com/v1/models".to_owned(),
                "https://api.example.com/models".to_owned(),
            ]
        );
        assert_eq!(
            candidates("https://api.example.com/v1"),
            vec!["https://api.example.com/v1/models".to_owned()]
        );
        assert_eq!(
            candidates("https://api.example.com/v1/chat/completions"),
            vec!["https://api.example.com/v1/models".to_owned()]
        );
        // 配置本身就是 models 端点：原样使用，不动 query 之外的任何东西。
        assert_eq!(
            candidates("https://api.example.com/v1/models?x=1"),
            vec!["https://api.example.com/v1/models".to_owned()]
        );
    }

    /// 非 JSON 正文不能把状态码吞掉（审计 11/P3-3）：错误里要有状态码与正文前缀。
    #[test]
    fn a_non_json_discovery_body_keeps_its_status_and_prefix() {
        let cases = [
            (
                reqwest::StatusCode::NOT_FOUND,
                b"<!DOCTYPE html><html><body>404 Not Found</body></html>".as_slice(),
                "404",
            ),
            (
                reqwest::StatusCode::BAD_GATEWAY,
                b"<html>bad gateway</html>".as_slice(),
                "502",
            ),
            (
                reqwest::StatusCode::OK,
                b"not json at all".as_slice(),
                "200",
            ),
            (reqwest::StatusCode::UNAUTHORIZED, b"".as_slice(), "401"),
        ];
        for (status, body, expected_status) in cases {
            let error = decode_discovery_body(status, body).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains(expected_status),
                "{expected_status} 必须留在错误里: {message}"
            );
            assert!(
                message.contains("body starts with"),
                "要带上正文前缀: {message}"
            );
            if body.is_empty() {
                assert!(message.contains("<empty body>"), "{message}");
            } else {
                assert!(
                    message.contains("404 Not Found")
                        || message.contains("bad gateway")
                        || message.contains("not json at all"),
                    "要走正文前 200 字符: {message}"
                );
            }
        }
        // 合法 JSON：状态码原样带出，交给调用方判成功与否。
        let (status, body) =
            decode_discovery_body(reqwest::StatusCode::OK, br#"{"data":[{"id":"m"}]}"#).unwrap();
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(body["data"][0]["id"], "m");
    }

    /// 端到端：端点回 HTML 错误页时，错误里必须带着状态码与正文（审计 11/P3-3 的夹具）。
    ///
    /// 这条走真实的 reqwest → `openai_models_at`：过去 `response.json()` 先失败，报错退化成
    /// 一句解码错误，404 与上游正文一起丢掉。
    #[tokio::test]
    async fn model_discovery_reports_the_status_of_an_html_error_page() {
        use axum::{http::StatusCode, routing::get, Router};

        let app = Router::new()
            .route(
                "/v1/models",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        [("content-type", "text/html")],
                        "<!DOCTYPE html><html><body>404 page not found</body></html>",
                    )
                }),
            )
            .route(
                "/ok/v1/models",
                get(|| async { axum::Json(serde_json::json!({"data": [{"id": "m1"}]})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        // 本地回环请求不经过系统代理。
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let headers = serde_json::json!({});

        let error = openai_models_at(
            &client,
            Url::parse(&format!("{base}/v1/models")).unwrap(),
            "test-key",
            &headers,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("404"), "状态码必须留在错误里: {error}");
        assert!(
            error.contains("404 page not found"),
            "上游正文前缀必须留在错误里: {error}"
        );

        let models = openai_models_at(
            &client,
            Url::parse(&format!("{base}/ok/v1/models")).unwrap(),
            "test-key",
            &headers,
        )
        .await
        .unwrap();
        assert_eq!(models, vec!["m1".to_owned()]);
    }

    #[test]
    fn the_body_prefix_is_bounded_and_marks_truncation() {
        assert_eq!(body_prefix(b"   \n\t "), "<empty body>");
        assert_eq!(body_prefix(b"short"), "short");
        let long = "x".repeat(500);
        let prefix = body_prefix(long.as_bytes());
        assert_eq!(prefix.chars().count(), 201, "200 字符 + 省略号");
        assert!(prefix.ends_with('…'));
    }

    /// 工件编码：可读 UTF-8 走 utf8，其余走 base64（审计 11/P3-7）。
    #[test]
    fn an_artifact_uses_utf8_only_when_it_is_readable_text() {
        let artifact = |data: Vec<u8>| CursorRunTraceArtifact {
            seq: 1,
            artifact_type: "response".into(),
            source: "upstream".into(),
            metadata: serde_json::json!({}),
            data,
            created_at_ms: 0,
        };

        let text = cursor_artifact(artifact(b"line one\nline two\ttabbed\r\n".to_vec()));
        assert_eq!(text.encoding, "utf8");
        assert!(text.data.starts_with("line one"));
        assert_eq!(text.byte_count, 26);

        // 二进制不是合法 UTF-8。
        let binary = cursor_artifact(artifact(vec![0xff, 0xfe, 0x00, 0x01]));
        assert_eq!(binary.encoding, "base64");
        assert_eq!(binary.data, STANDARD.encode([0xff, 0xfe, 0x00, 0x01]));
        assert_eq!(binary.byte_count, 4);

        // 合法 UTF-8 但含控制字符（终端转义等）：进 base64，免得前端把它当文本渲染。
        let control = cursor_artifact(artifact(b"\x1b[31mred\x1b[0m".to_vec()));
        assert_eq!(control.encoding, "base64");
    }

    /// 输出 token 估算：按空白切词，全是空白时退回字符数/4，空串为 0。
    #[test]
    fn output_tokens_are_estimated_from_words_then_characters() {
        assert_eq!(estimate_output_tokens(""), 0);
        assert_eq!(estimate_output_tokens("1 2 3 4 5"), 5);
        // 全是空白：一个词都没有，按字符数估（5 / 4 向上取整）。
        assert_eq!(estimate_output_tokens("     "), 2);
        // 没有空格的 CJK 串按定义就是一个"词"。
        assert_eq!(estimate_output_tokens("一二三四五六七八"), 1);
    }

    /// 模型 id 的抽取：数组元素可以是字符串，也可以是带 id/name 的对象。
    #[test]
    fn model_ids_accept_both_shapes() {
        assert_eq!(
            model_ids(&serde_json::json!(["a", "b"])),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert_eq!(
            model_ids(&serde_json::json!([{"id": "a"}, {"name": "b"}, {"other": "c"}, 7])),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(model_ids(&serde_json::json!({"object": "list"})).is_empty());
    }
}
