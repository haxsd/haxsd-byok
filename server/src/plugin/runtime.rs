//! Tracks Deno runtime readiness and coordinates one initialization task.
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::{
    asset::{RuntimeAsset, DENO_VERSION},
    installation::{self, InstallFailure},
};
use crate::{config, store::Store, Result};

#[derive(Clone)]
pub struct PluginRuntime {
    inner: Arc<PluginRuntimeInner>,
}

struct PluginRuntimeInner {
    root: PathBuf,
    asset: Option<RuntimeAsset>,
    status: RwLock<PluginRuntimeStatus>,
    initializing: AtomicBool,
    cancellation: Mutex<Option<CancellationToken>>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginRuntimeState {
    Uninitialized,
    Initializing,
    Ready,
    Failed,
    Unsupported,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginRuntimePhase {
    Checking,
    Downloading,
    Verifying,
    Installing,
    Validating,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PluginRuntimeStatus {
    pub state: PluginRuntimeState,
    pub version: String,
    pub target: Option<String>,
    pub phase: Option<PluginRuntimePhase>,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
}

impl PluginRuntimeStatus {
    fn uninitialized(asset: &RuntimeAsset) -> Self {
        Self::new(PluginRuntimeState::Uninitialized, Some(asset))
    }

    fn ready(asset: &RuntimeAsset) -> Self {
        Self::new(PluginRuntimeState::Ready, Some(asset))
    }

    fn unsupported() -> Self {
        Self {
            state: PluginRuntimeState::Unsupported,
            version: DENO_VERSION.into(),
            target: None,
            phase: None,
            downloaded_bytes: 0,
            total_bytes: None,
            error: Some(format!(
                "unsupported platform: {}/{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            )),
        }
    }

    fn new(state: PluginRuntimeState, asset: Option<&RuntimeAsset>) -> Self {
        Self {
            state,
            version: DENO_VERSION.into(),
            target: asset.map(|value| value.target.into()),
            phase: None,
            downloaded_bytes: 0,
            total_bytes: None,
            error: None,
        }
    }

    /// 初始化失败的终态：`error` 是给用户看的真实原因（见 `InstallFailure`），
    /// 不再是笼统的"初始化失败"。
    async fn failed(store: &Store, asset: &RuntimeAsset, failure: &InstallFailure) -> Self {
        let message = failure.status_message(store).await;
        tracing::error!(
            reason = %message,
            target = asset.target,
            "plugin runtime initialization failed"
        );
        let mut status = Self::new(PluginRuntimeState::Failed, Some(asset));
        status.error = Some(message);
        status
    }
}

impl PluginRuntime {
    pub fn managed() -> Result<Self> {
        Self::new(config::managed_data_dir()?.join("plugins").join("runtime"))
    }

    fn new(root: PathBuf) -> Result<Self> {
        Self::with_asset(root, RuntimeAsset::current())
    }

    /// 装配一个面向 `asset` 的运行时；`None` 表示当前平台没有可用的 Deno 归档。
    fn with_asset(root: PathBuf, asset: Option<RuntimeAsset>) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        }
        let status = match &asset {
            Some(asset) if installation::runtime_complete(&root, asset) => {
                PluginRuntimeStatus::ready(asset)
            }
            Some(asset) => PluginRuntimeStatus::uninitialized(asset),
            None => PluginRuntimeStatus::unsupported(),
        };
        Ok(Self {
            inner: Arc::new(PluginRuntimeInner {
                root,
                asset,
                status: RwLock::new(status),
                initializing: AtomicBool::new(false),
                cancellation: Mutex::new(None),
            }),
        })
    }

    pub fn status(&self) -> PluginRuntimeStatus {
        let mut status = self.inner.status.write();
        if status.state == PluginRuntimeState::Ready {
            if let Some(asset) = self.inner.asset.as_ref() {
                if !installation::runtime_complete(&self.inner.root, asset) {
                    *status = PluginRuntimeStatus::uninitialized(asset);
                }
            }
        }
        status.clone()
    }

    pub fn executable(&self) -> Option<PathBuf> {
        let asset = self.inner.asset.as_ref()?;
        if self.status().state != PluginRuntimeState::Ready {
            return None;
        }
        Some(installation::runtime_executable(&self.inner.root, asset))
    }

    pub fn initialize(&self, store: Store) -> PluginRuntimeStatus {
        let Some(asset) = self.inner.asset.clone() else {
            return self.status();
        };
        if installation::runtime_complete(&self.inner.root, &asset) {
            let ready = PluginRuntimeStatus::ready(&asset);
            *self.inner.status.write() = ready.clone();
            return ready;
        }
        if self
            .inner
            .initializing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return self.status();
        }

        let mut initializing =
            PluginRuntimeStatus::new(PluginRuntimeState::Initializing, Some(&asset));
        initializing.phase = Some(PluginRuntimePhase::Checking);
        *self.inner.status.write() = initializing.clone();

        let cancellation = CancellationToken::new();
        *self.inner.cancellation.lock() = Some(cancellation.clone());
        let runtime = self.clone();
        tokio::spawn(async move {
            let result = installation::install(
                &runtime.inner.root,
                &store,
                asset.clone(),
                cancellation,
                |phase, downloaded, total| {
                    runtime.update_progress(phase, downloaded, total);
                },
            )
            .await;
            let status = match result {
                Ok(()) => PluginRuntimeStatus::ready(&asset),
                // 取消不是失败：断点留着，状态回到未初始化，用户下次点了就接着下。
                Err(failure) if failure.is_cancelled() => {
                    PluginRuntimeStatus::uninitialized(&asset)
                }
                Err(failure) => PluginRuntimeStatus::failed(&store, &asset, &failure).await,
            };
            *runtime.inner.status.write() = status;
            runtime.inner.cancellation.lock().take();
            runtime.inner.initializing.store(false, Ordering::Release);
        });

        initializing
    }

    pub fn cancel_initialization(&self) -> PluginRuntimeStatus {
        if let Some(cancellation) = self.inner.cancellation.lock().as_ref() {
            cancellation.cancel();
        }
        self.status()
    }

    fn update_progress(
        &self,
        phase: PluginRuntimePhase,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    ) {
        let mut status = self.inner.status.write();
        status.state = PluginRuntimeState::Initializing;
        status.phase = Some(phase);
        status.downloaded_bytes = downloaded_bytes;
        status.total_bytes = total_bytes;
        status.error = None;
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::path::Path;

    use super::*;

    pub(in crate::plugin) fn runtime(root: &Path) -> PluginRuntime {
        PluginRuntime::new(root.join("runtime")).unwrap()
    }

    /// 面向一个给定 asset 的运行时：测试用假的下载地址与摘要走完整条初始化路径。
    pub(in crate::plugin) fn runtime_with(root: &Path, asset: RuntimeAsset) -> PluginRuntime {
        PluginRuntime::with_asset(root.join("runtime"), Some(asset)).unwrap()
    }
}
