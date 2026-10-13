//! Exposes the local desktop application integration.
mod account;
mod ca;
mod catalog_health;
mod cleanup;
mod process;
mod proxy;
mod settings;

use std::{net::SocketAddr, sync::Arc};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    store::{CursorMode, Store, TabMode, TabSettings},
    Error, Result,
};

use self::{ca::CaManager, proxy::ProxyRuntime};

pub use account::{AccountState, CursorAccount};
pub use catalog_health::{CatalogHealth, CatalogHealthTracker};
/// 收尾助手（`--cleanup-after-exit`）的入口：等应用退出后撤掉它留下的代理配置。
pub use cleanup::cleanup_after_exit;

/// 诊断快照要读 Cursor 里与代理有关的那几个键；`settings` 模块本身不对外。
pub use settings::proxy_settings_snapshot;

pub(crate) fn proxy_host_allowed(host: &str) -> bool {
    proxy::is_cursor_host(host)
}

pub(crate) fn request_uses_local_cursor_token(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(account::is_local_cursor_authorization)
}

#[cfg(test)]
pub(crate) fn local_cursor_authorization() -> String {
    format!("Bearer {}", account::local_token().unwrap())
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaState {
    Missing,
    Untrusted,
    Ready,
    Invalid,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    Disabled,
    Enabled,
    Degraded,
}

#[derive(Clone, Debug, Serialize)]
pub struct CursorHarnessStatus {
    pub platform: &'static str,
    pub ca: CaState,
    /// 模型来源模式：官方 / 本地 / 混合。
    pub mode: CursorMode,
    pub configured_models: usize,
    pub enabled_models: usize,
    pub integration: IntegrationState,
    pub settings_applied: bool,
    /// Cursor 的代理配置由**另一个软件**写入：我们不覆盖它，界面要把这件事说出来，
    /// 否则用户只会看到「开关开着但显示未接管」。
    pub foreign_configuration: bool,
    pub proxy_url: Option<String>,
    pub ca_install_command: Option<String>,
    /// 客户端现在用的是哪种账号：没有 / 我们注入的本地账号 / 用户自己的账号。
    /// 混合模式要求后者，界面靠它显示前置条件。
    pub account: CursorAccount,
    /// 官方模型目录最近一次抓取的成败。本地模式不用官方目录，界面可以不显示。
    pub catalog: CatalogHealth,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct SetMode {
    pub mode: CursorMode,
}

#[derive(Clone)]
pub struct CursorHarness {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    ca: CaManager,
    ca_initialization: Mutex<()>,
    backend_addr: RwLock<Option<SocketAddr>>,
    tab_mode: Arc<RwLock<TabMode>>,
    proxy: Mutex<ProxyRuntime>,
    catalog: CatalogHealthTracker,
}

impl CursorHarness {
    pub fn new(store: Store) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                ca: CaManager::managed()?,
                ca_initialization: Mutex::new(()),
                backend_addr: RwLock::new(None),
                tab_mode: Arc::new(RwLock::new(TabMode::default())),
                proxy: Mutex::new(ProxyRuntime::default()),
                catalog: CatalogHealthTracker::default(),
            }),
        })
    }

    pub fn set_backend_addr(&self, addr: SocketAddr) {
        *self.inner.backend_addr.write() = Some(addr);
    }

    /// 官方模型目录的抓取结果由 `services::model_catalog` 写进来，状态接口读出去。
    pub fn catalog_health(&self) -> CatalogHealthTracker {
        self.inner.catalog.clone()
    }

    pub async fn proxy_port(&self) -> Option<u16> {
        self.inner.proxy.lock().await.port()
    }

    pub async fn cleanup_stale_settings(&self) -> Result<()> {
        if std::env::var_os("HAXSD_BYOK_DATA_DIR").is_some() {
            return Ok(());
        }
        settings::clear_stale_managed_settings()
    }

    pub async fn status(&self) -> Result<CursorHarnessStatus> {
        let mode = self.inner.store.cursor_mode().await?;
        // 官方模式不该留着我们注入的本地账号：客户端带着它直连官方，每个请求都会
        // 被拒，而界面又不再 mock 账号，看上去就是"登录着但什么都不通"。这里只清
        // 我们写过的那一串，且不结束进程——读状态每几秒被前端调一次，结束编辑器
        // 不能放在这条路径上。
        if !mode.takes_over() && std::env::var_os("HAXSD_BYOK_DATA_DIR").is_none() {
            clear_injected_account().await?;
        }
        let models = self.inner.store.models().await?;
        let configured_models = models.len();
        let enabled_models = configured_models;
        let ca = self.inner.ca.state()?;
        let foreign_configuration = settings::proxy_configuration_is_foreign()?;
        if mode.takes_over()
            && matches!(ca, CaState::Ready)
            && self.inner.backend_addr.read().is_some()
            && std::env::var_os("HAXSD_BYOK_DATA_DIR").is_none()
        {
            // 读状态只能把配置补齐，**不许结束进程**：这个方法每几秒被前端调用一次，
            // 而结束进程是不可逆的（未保存的编辑会丢）。谁在什么时候可以杀 Cursor，
            // 见 apply_takeover 的说明。
            self.apply_takeover(false).await?;
        }
        let proxy = self.inner.proxy.lock().await;
        let proxy_url = proxy.url();
        let settings_applied = proxy_url
            .as_deref()
            .map(settings::settings_match)
            .transpose()?
            .unwrap_or(false);
        let integration = match (proxy.running(), settings_applied) {
            (false, false) => IntegrationState::Disabled,
            (true, true) => IntegrationState::Enabled,
            _ => IntegrationState::Degraded,
        };
        Ok(CursorHarnessStatus {
            platform: std::env::consts::OS,
            ca,
            mode,
            configured_models,
            enabled_models,
            integration,
            settings_applied,
            foreign_configuration,
            proxy_url,
            ca_install_command: self.inner.ca.install_command(),
            account: account::account_state().await,
            catalog: self.inner.catalog.snapshot(),
        })
    }

    pub async fn initialize_ca(&self) -> Result<CursorHarnessStatus> {
        reject_isolated_integration()?;
        let _initialization = self.inner.ca_initialization.lock().await;
        let manager = self.inner.ca.clone();
        tokio::task::spawn_blocking(move || manager.initialize_local())
            .await
            .map_err(|error| Error::Store(format!("CA initialization task failed: {error}")))??;
        self.status().await
    }

    /// 切换模型来源模式。一次调用完成这个模式要求的全部副作用：代理配置、账号
    /// 策略、以及在用户明确选择时结束 Cursor。
    pub async fn set_mode(&self, mode: CursorMode) -> Result<CursorHarnessStatus> {
        let previous = self.inner.store.cursor_mode().await?;
        self.inner.store.set_cursor_mode(mode).await?;
        if mode.takes_over() {
            // 从"官方"跨回来的那一步是用户按下模式卡片的结果，也是唯一可以让
            // Cursor 重新读取代理配置的方式（它只在启动时读一次）。
            self.apply_takeover(previous == CursorMode::Official)
                .await?;
        } else {
            self.enter_official(previous).await?;
        }
        self.status().await
    }

    pub async fn set_tab_settings(&self, settings: TabSettings) -> Result<TabSettings> {
        let saved = self.inner.store.set_tab_settings(settings).await?;
        *self.inner.tab_mode.write() = saved.mode;
        Ok(saved)
    }

    /// 官方模式：撤回代理配置、清掉我们注入的账号、停掉本机代理。
    ///
    /// 刚才是接管状态时才结束 Cursor：它内存里还指着本机代理，端口一停每个请求都会
    /// 失败，而配置与账号都只在它启动时才会被重新读取。本来就没接管时什么都不用做。
    async fn enter_official(&self, previous: CursorMode) -> Result<()> {
        reject_isolated_integration()?;
        if previous.takes_over() {
            process::terminate_cursor().await?;
            settings::clear_proxy_settings()?;
        }
        // 账号无条件按模式收敛：离开接管后还留着本地账号的话，客户端重开时会带着
        // 它直连官方，用户看到的就是满屏被拒的请求。
        reconcile_account(CursorMode::Official).await?;
        self.inner.proxy.lock().await.stop().await;
        Ok(())
    }

    /// 让 Cursor 的配置指向本机代理，必要时结束正在运行的 Cursor。
    ///
    /// Cursor 只在启动时读取代理配置，所以要让它立刻生效就得重启它。结束进程是不可逆
    /// 的（未保存的编辑会丢），因此**只有用户明确选择模式时才允许**：`explicit_takeover`
    /// 只有 `set_mode` 从"官方"切进接管模式时才会传 true，那一步前面有确认对话框。
    ///
    /// 以前读状态也走这条路（`status()` 每几秒被前端调一次）。后果是：同机上另一个
    /// 同类软件（Cursor BYOK 与本项目同源，写的是同一份 settings.json、同样的五个键）
    /// 写回它自己的代理配置之后，我们下一次刷新就会 `taskkill /F /T /IM Cursor.exe`，
    /// 把用户正在编辑的编辑器连同对方那条链路一起打断。现在读状态只补写配置，不动进程。
    async fn apply_takeover(&self, explicit_takeover: bool) -> Result<()> {
        reject_isolated_integration()?;
        if !matches!(self.inner.ca.state()?, CaState::Ready) {
            return Err(Error::Config(
                "initialize and trust the CA before enabling Cursor".into(),
            ));
        }
        // 另一个软件在管 Cursor 的代理配置：读状态时先生手——不写、不启动代理、不碰进程。
        // 两个软件轮流覆盖同一份配置，只会让用户两条链路都不稳定；要不要抢过来，由用户
        // 在那个开关上决定（那时才走下面这段）。
        if !explicit_takeover && settings::proxy_configuration_is_foreign()? {
            return Ok(());
        }
        let backend_addr = self
            .inner
            .backend_addr
            .read()
            .ok_or_else(|| Error::Config("desktop management server is not ready".into()))?;
        let mut proxy = self.inner.proxy.lock().await;
        let settings_applied = proxy
            .url()
            .as_deref()
            .map(settings::settings_match)
            .transpose()?
            .unwrap_or(false);
        if should_terminate_cursor(explicit_takeover, settings_applied) {
            process::terminate_cursor().await?;
        }
        let mode = self.inner.store.cursor_mode().await?;
        if proxy.running() {
            if let Some(url) = proxy.url() {
                apply_cursor_configuration(&url, mode).await?;
            }
            return Ok(());
        }
        let ca = self.inner.ca.load()?;
        let requested_port = self.inner.store.port_settings().await?.proxy_port;
        *self.inner.tab_mode.write() = self.inner.store.tab_settings().await?.mode;
        let (url, actual_port) = proxy
            .start(
                backend_addr,
                ca,
                requested_port,
                self.inner.tab_mode.clone(),
            )
            .await?;
        if let Err(error) = self.inner.store.set_proxy_port(actual_port).await {
            proxy.stop().await;
            return Err(error);
        }
        if let Err(error) = apply_cursor_configuration(&url, mode).await {
            proxy.stop().await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn disable(&self) -> Result<()> {
        if std::env::var_os("HAXSD_BYOK_DATA_DIR").is_none() {
            settings::clear_proxy_settings()?;
        }
        self.inner.proxy.lock().await.stop().await;
        Ok(())
    }
}

fn reject_isolated_integration() -> Result<()> {
    if std::env::var_os("HAXSD_BYOK_DATA_DIR").is_some() {
        return Err(Error::Config(
            "Cursor integration is disabled with an isolated data directory".into(),
        ));
    }
    Ok(())
}

/// 账号策略：只有"本地"模式需要、也只允许它留下本地账号；另外两个模式一发现
/// 我们写过的那串就清掉——官方模式要把 Cursor 交还给用户自己的账号，混合模式
/// 要求真的登录，都不能让一个假账号挡住登录入口。
///
/// 清不掉（数据库被客户端占着等）不能挡路：模式已经记下了，下一次状态刷新还会再试。
async fn reconcile_account(mode: CursorMode) -> Result<()> {
    if mode.injects_local_account() {
        return account::inject_if_missing().await;
    }
    clear_injected_account().await
}

/// 清掉本地账号，失败只记日志。
pub(crate) async fn clear_injected_account() -> Result<()> {
    match account::clear_injected_account().await {
        Ok(true) => {
            tracing::info!("cleared the injected local Cursor account");
            Ok(())
        }
        Ok(false) => Ok(()),
        Err(error) => {
            tracing::warn!(
                error = %crate::diagnostics::error_chain(&error),
                "cannot clear the injected local Cursor account"
            );
            Ok(())
        }
    }
}

/// 是否可以在应用代理配置前强制结束 Cursor。
///
/// 两个条件缺一不可：
/// 1. 用户**明确**选过模式（确认对话框里写明未保存内容会丢）。读状态那条路径传
///    false——它每几秒跑一次，不能把一个不可逆的动作放在那里；
/// 2. 配置确实还不是我们的。已经指向本机时再杀一次没有任何收益，只有代价。
fn should_terminate_cursor(explicit_takeover: bool, settings_already_ours: bool) -> bool {
    explicit_takeover && !settings_already_ours
}

async fn apply_cursor_configuration(proxy_url: &str, mode: CursorMode) -> Result<()> {
    reconcile_account(mode).await?;
    settings::write_proxy_settings(proxy_url)
}

#[cfg(test)]
mod tests {
    use super::should_terminate_cursor;
    use crate::store::CursorMode;

    #[test]
    fn only_an_explicit_choice_may_terminate_cursor() {
        // 用户按开关并确认过：配置还不是我们的，结束 Cursor 让它重新读取。
        assert!(should_terminate_cursor(true, false));
        // 刷新状态时绝不能结束进程——同机上另一个同类软件写回它自己的配置后，
        // 每几秒一次的刷新会把用户正在编辑的 Cursor 杀掉。
        assert!(!should_terminate_cursor(false, false));
        // 配置已经指向本机，再杀一次只有代价。
        assert!(!should_terminate_cursor(true, true));
        assert!(!should_terminate_cursor(false, true));
    }

    #[test]
    fn only_local_mode_keeps_a_local_account() {
        assert!(CursorMode::Local.injects_local_account());
        assert!(!CursorMode::Mixed.injects_local_account());
        assert!(!CursorMode::Official.injects_local_account());
    }

    #[test]
    fn only_local_mode_hides_official_models() {
        assert!(!CursorMode::Local.includes_official_models());
        assert!(CursorMode::Mixed.includes_official_models());
    }

    #[test]
    fn only_official_mode_leaves_cursor_alone() {
        assert!(!CursorMode::Official.takes_over());
        assert!(CursorMode::Local.takes_over());
        assert!(CursorMode::Mixed.takes_over());
    }
}
