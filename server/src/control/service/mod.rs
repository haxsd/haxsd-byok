//! Implements control API routing and shared state.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    devin::gateway::{DevinBoundPorts, DevinListening},
    local_app::CursorHarness,
    model::{CursorRunTraceSummary, LlmCallRequest, LlmCallResponseChunk, LlmCallSummary},
    plugin::{PluginRegistry, PluginRuntime},
    provider::Provider,
    store::Store,
    Result,
};

mod calls;
mod devin;
mod models;
mod plugins;
mod settings;

pub use models::{
    DiscoveredModels, LegacyModelImportPreview, LegacyModelImportResult, ModelConnectivityResult,
    ModelDiscoveryInput,
};

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

/// the bound ports to be exactly the saved ports.
#[derive(Clone, Debug, Serialize)]
pub struct DevinStatus {
    pub enabled: bool,
    pub listening: bool,
    pub ready: bool,
    pub bound_ports: Option<DevinBoundPorts>,
    pub calls: usize,
    pub last_call_success: Option<bool>,
    pub last_call_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallDetail {
    pub call: CallSummary,
    pub request: Option<LlmCallRequest>,
    pub response_chunks: Vec<LlmCallResponseChunk>,
    pub cursor_trace: Option<CursorTraceDetail>,
    /// 正文被闸门截断过：只回了一部分，剩下的没取。
    pub truncated: bool,
    /// 这个 run 触发过上下文压缩时的记录：压掉多少条、摘要多长、归档在哪。
    pub compaction: Option<crate::store::CompactionEvent>,
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
}
