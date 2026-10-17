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

use self::proxy::ProxyRuntime;
pub use ca::{is_certificate_installed, CaManager};

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
    request_authorization(headers).is_some_and(account::is_local_cursor_authorization)
}

/// 请求头里的 `Authorization` 原文（含 `Bearer ` 前缀）。
fn request_authorization(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()
}

/// 本机入口认出的调用方身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalCaller {
    /// 我们注入的本地账号（本地模式）。
    LocalAccount,
    /// 客户端自己的账号（混合模式）。
    OfficialAccount,
}

/// 本机入口（本地代理）认不认这个调用方；`None` = 匿名，应当拒绝。
///
/// 本地代理是 Cursor 客户端唯一的入口：接管期间 Cursor 的 `http.proxy` 指着它，它只该服务
/// Cursor 本身。判据分两层：
///
/// 1. 出示**我们注入的本地账号 token** → `LocalAccount`。本地模式下 `inject_if_missing` 把这份
///    token 写进客户端 `state.vscdb`（`cursorAuth/accessToken` 等七个键），Cursor 之后每个请求
///    都带着它——这就是"要求本地账号 token"那条判据。
/// 2. 混合模式下客户端用的是用户自己的账号：token 由 Cursor 自己签发，本地无法验证
///    （`cursor::services::entitlement::official_token_hash` 也只是把它当"官方 token"用，从不与
///    库里的值比对），所以只确认它带着一个非空的 Bearer → `OfficialAccount`。
///
/// 于是这一层挡住的是**匿名调用**：浏览器、随手 curl、端口扫描、别的应用误打误撞调这套接口。
/// 它挡不住同 UID 的进程——那份 token 是固定常量（见 `account.rs` 开头与审计的低价值观察），
/// 同机进程读得到 `state.vscdb`、也构造得出；真正的边界是回环绑定与非回环默认拒绝启动
/// （`config::check_listen_addr`）。写在这里是为了别让人把它当成一道安全边界。
pub(crate) fn local_entry_caller(headers: &axum::http::HeaderMap) -> Option<LocalCaller> {
    let authorization = request_authorization(headers)?;
    if authorization.strip_prefix("Bearer ")?.is_empty() {
        return None;
    }
    Some(if account::is_local_cursor_authorization(authorization) {
        LocalCaller::LocalAccount
    } else {
        LocalCaller::OfficialAccount
    })
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
    /// 接管与撤回的顶层互斥。
    ///
    /// `status()` 的补齐与 `set_mode()` 的切换都会改用户机器上的配置与账号；两条路径
    /// 并发时的交错会留下自相矛盾的状态：切到官方已经清掉配置、正在写的 `status()` 又
    /// 把配置补回来，于是库里是官方、文件里还指着本机代理，而界面显示的正是"官方"——
    /// 用户以为已经退出接管，Cursor 下次启动却仍然走我们的代理。持锁顺序固定为
    /// `transition` → `proxy`，反向取锁会死锁。
    transition: Mutex<()>,
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
                transition: Mutex::new(()),
            }),
        })
    }

    /// 记下管理服务的监听地址，并在服务端起来之后按当前模式收敛一次接管。
    ///
    /// 这是"服务端已经起来"的唯一信号，也是**静默自启**唯一等得到的东西：那种启动没有
    /// 窗口，也就没有人调 `status()`，而 `status()` 是收敛的唯一入口——本地/混合模式会
    /// 静默失效（代理不起、`settings.json` 不写回），Cursor 直连官方而用户以为在走 BYOK，
    /// 直到他手动开窗、再重启一次 Cursor。收敛口径与读状态完全一致：不结束进程，失败只
    /// 记日志（下一次刷新还会走同一条路）。
    pub fn set_backend_addr(&self, addr: SocketAddr) {
        *self.inner.backend_addr.write() = Some(addr);
        if !should_converge_at_startup(
            manages_settings(),
            tokio::runtime::Handle::try_current().is_ok(),
        ) {
            return;
        }
        let harness = self.clone();
        tokio::spawn(async move {
            match harness.status().await {
                Ok(status) => tracing::info!(
                    mode = ?status.mode,
                    integration = ?status.integration,
                    "cursor takeover converged after startup"
                ),
                Err(error) => tracing::warn!(
                    error = %crate::diagnostics::error_chain(&error),
                    "cursor takeover convergence failed at startup"
                ),
            }
        });
    }

    /// 官方模型目录的抓取结果由 `services::model_catalog` 写进来，状态接口读出去。
    pub fn catalog_health(&self) -> CatalogHealthTracker {
        self.inner.catalog.clone()
    }

    pub async fn proxy_port(&self) -> Option<u16> {
        self.inner.proxy.lock().await.port()
    }

    pub async fn cleanup_stale_settings(&self) -> Result<()> {
        if !manages_settings() {
            return Ok(());
        }
        settings::clear_stale_managed_settings()
    }

    pub async fn status(&self) -> Result<CursorHarnessStatus> {
        // 收敛副作用与 `set_mode()` 的切换互斥（见 `Inner::transition`）；模式也必须在
        // 锁里读——锁外读到的是切换之前的旧值，拿它去补齐就会把刚撤掉的配置写回来。
        let (mode, ca, foreign_configuration, configured_models) = {
            let _transition = self.inner.transition.lock().await;
            let mode = self.inner.store.cursor_mode().await?;
            // 证书状态每轮现算：Windows 上枚举 Root 存储要读整个库，实测亚毫秒级。**有意不缓存**
            // ——界面正靠 ~2 秒一次的轮询等"用户在系统证书对话框里点了信任"，缓存会让 Ready 迟到，
            // 或在用户手工删掉证书后继续谎报已就绪。
            let ca = self.inner.ca.state()?;
            let backend_ready = self.inner.backend_addr.read().is_some();
            match status_convergence(mode, &ca, backend_ready, manages_settings()) {
                Convergence::Nothing => {}
                Convergence::Withdraw => {
                    // 不接管的模式：账号必须清（客户端带着假 Token 直连官方，每个请求都会
                    // 被拒），我们写的配置也必须撤——留着它 Cursor 下次启动仍然走本机代理，
                    // 而界面显示"官方"。这里无条件收敛，把并发切换、上一次撤回失败留下的
                    // 不一致在下一次刷新里抹平；`clear_proxy_settings` 自己按标记认领，别人
                    // 写的配置一根手指都不碰。
                    clear_injected_account().await?;
                    settings::clear_proxy_settings()?;
                }
                Convergence::TakeOver => {
                    // 读状态只能把配置补齐，**不许结束进程**：这个方法每几秒被前端调用
                    // 一次，而结束进程是不可逆的（未保存的编辑会丢）。谁在什么时候可以杀
                    // Cursor，见 apply_takeover 的说明。
                    self.apply_takeover(mode, TakeoverRequest::STATUS_REFRESH)
                        .await?;
                }
            }
            let foreign_configuration = settings::proxy_configuration_is_foreign()?;
            // 这里每个状态刷新都全表读一次模型，只为 `.len()`（`ModelConfig` 里还带着明文
            // api_key）。计数方法应当在 `store/models.rs`——`store/**` 不归本模块，交接给
            // store 所有者：加 `pub async fn model_count(&self) -> Result<usize>`（`SELECT COUNT(*)`），
            // 这边换成一行调用即可。
            let configured_models = self.inner.store.models().await?.len();
            (mode, ca, foreign_configuration, configured_models)
        };
        let enabled_models = configured_models;
        // 代理锁只用来取"在不在跑、地址是什么"这两个值。后面每一步都不需要它：读
        // `settings.json`（`settings_match`）、读客户端库（`account_state`）、枚举证书存储
        // （`ca.state`，上面已经做完了）。而状态刷新是每几秒一次的热路径——之前整段都在锁里，
        // 客户端库忙的时候 `proxy_port()`、`set_mode()` 的接管、`disable()` 的停代理全都要
        // 排队等一次文件读。
        let (proxy_running, proxy_url) = {
            let proxy = self.inner.proxy.lock().await;
            (proxy.running(), proxy.url())
        };
        let settings_applied = proxy_url
            .as_deref()
            .map(settings::settings_match)
            .transpose()?
            .unwrap_or(false);
        let integration = match (proxy_running, settings_applied) {
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
    ///
    /// 副作用先行、模式最后落库：中途失败（CA 没就绪、代理端口起不来、settings.json
    /// 读不动）时库里仍是上一个模式，界面不会留下一张开着的开关和一条没被接管的链路。
    /// 反过来先写库的话，报错之后每一次状态刷新都会拿着新模式重试，用户只能自己再切
    /// 一次才能对齐——这正是"看起来正常、实际没生效"。
    pub async fn set_mode(&self, mode: CursorMode) -> Result<CursorHarnessStatus> {
        {
            // 副作用整段互斥（见 `Inner::transition`）：读状态的补齐可能与这次切换并发，
            // 交错会把配置与库里的模式写成两份互相矛盾的记录。
            let _transition = self.inner.transition.lock().await;
            let previous = self.inner.store.cursor_mode().await?;
            if mode.takes_over() {
                // 从"官方"跨回来的那一步是用户按下模式卡片的结果，也是唯一可以让
                // Cursor 重新读取代理配置的方式（它只在启动时读一次）。
                self.apply_takeover(
                    mode,
                    TakeoverRequest::user_choice(previous == CursorMode::Official),
                )
                .await?;
            } else {
                self.enter_official(previous).await?;
            }
            self.inner.store.set_cursor_mode(mode).await?;
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
        reconcile_account(CursorMode::Official, true).await?;
        self.inner.proxy.lock().await.stop().await;
        Ok(())
    }

    /// 让 Cursor 的配置指向本机代理，必要时结束正在运行的 Cursor。
    ///
    /// Cursor 只在启动时读取代理配置，所以要让它立刻生效就得重启它。结束进程是不可逆
    /// 的（未保存的编辑会丢），因此**只有用户明确选择模式时才允许**：`TakeoverRequest`
    /// 里那个 `may_terminate_cursor` 只有 `set_mode` 从"官方"切进接管模式时才是 true，
    /// 那一步前面有确认对话框。
    ///
    /// 以前读状态也走这条路（`status()` 每几秒被前端调一次）。后果是：同机上另一个
    /// 同类软件（Cursor BYOK 与本项目同源，写的是同一份 settings.json、同样的五个键）
    /// 写回它自己的代理配置之后，我们下一次刷新就会 `taskkill /F /T /IM Cursor.exe`，
    /// 把用户正在编辑的编辑器连同对方那条链路一起打断。现在读状态只补写配置，不动进程。
    ///
    /// `mode` 由调用方给出，而不是在这里读库：`set_mode` 必须能在模式落库之前先把
    /// 副作用做完（见那里的说明）。
    async fn apply_takeover(&self, mode: CursorMode, request: TakeoverRequest) -> Result<()> {
        reject_isolated_integration()?;
        if !matches!(self.inner.ca.state()?, CaState::Ready) {
            return Err(Error::Config(
                "initialize and trust the CA before enabling Cursor".into(),
            ));
        }
        // 另一个软件在管 Cursor 的代理配置：读状态时先生手——不写、不启动代理、不碰进程。
        // 两个软件轮流覆盖同一份配置，只会让用户两条链路都不稳定；要不要抢过来，由用户
        // 在那个开关上决定（那时才走下面这段）。
        if !request.may_terminate_cursor && settings::proxy_configuration_is_foreign()? {
            return Ok(());
        }
        let backend_addr = self
            .inner
            .backend_addr
            .read()
            .ok_or_else(|| Error::Config("desktop management server is not ready".into()))?;
        // 代理锁只保护"读一次状态"与"启动/停止"这两小段。中间每一步都是 IO——读
        // `settings.json`（`settings_match`）、读 SQLite 里的端口/标签、结束 Cursor、写回代理
        // 配置（`apply_cursor_configuration` 里还有 SQLite）——握着代理锁做这些，会让并发的
        // `proxy_port()`、状态刷新的取状态段、`disable()` 的停代理全都排队等一次文件写。
        // 接管/撤回本身已经由 `transition` 锁串行化（见 `Inner::transition`），所以不需要用
        // 代理锁来防重入；`ProxyRuntime::start` 自己也会在锁内复用已经跑着的实例。
        let (proxy_running, proxy_url) = {
            let proxy = self.inner.proxy.lock().await;
            (proxy.running(), proxy.url())
        };
        let settings_applied = proxy_url
            .as_deref()
            .map(settings::settings_match)
            .transpose()?
            .unwrap_or(false);
        if request.should_terminate_cursor(settings_applied) {
            process::terminate_cursor().await?;
        }
        if proxy_running {
            if let Some(url) = proxy_url {
                apply_cursor_configuration(&url, mode, request).await?;
            }
            return Ok(());
        }
        let ca = self.inner.ca.load()?;
        let requested_port = self.inner.store.port_settings().await?.proxy_port;
        *self.inner.tab_mode.write() = self.inner.store.tab_settings().await?.mode;
        let (url, actual_port) = {
            let mut proxy = self.inner.proxy.lock().await;
            proxy
                .start(
                    backend_addr,
                    ca,
                    requested_port,
                    self.inner.tab_mode.clone(),
                )
                .await?
        };
        // 往下都是 IO；要回滚就自己再拿一次代理锁（`stop()` 不假设调用方持锁）。
        if let Err(error) = self.inner.store.set_proxy_port(actual_port).await {
            self.inner.proxy.lock().await.stop().await;
            return Err(error);
        }
        if let Err(error) = apply_cursor_configuration(&url, mode, request).await {
            self.inner.proxy.lock().await.stop().await;
            return Err(error);
        }
        Ok(())
    }

    /// 撤回本应用对客户端的接管：撤掉写进 Cursor 的代理配置、清掉我们注入的本地
    /// 账号、停掉本机代理。
    ///
    /// 账号必须跟着配置一起走。配置撤掉之后本机代理就没了，客户端却还带着我们注入
    /// 的那串 Token 直连厂商——每个请求都会被拒，用户看到的是"登录着但什么都不通"。
    /// 撤干净之后客户端要么用他自己的账号，要么回到登录界面，两种都是能继续用的状态。
    pub async fn disable(&self) -> Result<()> {
        // 与读状态的补齐互斥：退出时撤掉的东西不能被一次并发刷新又补回来。
        let _transition = self.inner.transition.lock().await;
        if manages_settings() {
            // 先撤账号：即使下面写配置失败（文件被占、解析不了），也不会留下
            // "假账号 + 没有代理"这个最坏组合。
            clear_injected_account().await?;
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

/// 我们是否在管用户机器上的 Cursor 文件（`settings.json`、`state.vscdb`）。
///
/// 隔离数据目录（`HAXSD_BYOK_DATA_DIR`，测试与并行开发用）下这些文件的路径仍然指向真实
/// 的用户目录，所以整条接管/撤回路径必须跳过——不然测试会去改开发者自己的 Cursor。
fn manages_settings() -> bool {
    std::env::var_os("HAXSD_BYOK_DATA_DIR").is_none()
}

/// 读状态时该做的收敛动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Convergence {
    /// 什么都不用做。
    Nothing,
    /// 按当前模式补齐接管配置（不结束进程）。
    TakeOver,
    /// 撤掉我们写的接管控件（账号 + 代理配置）。
    Withdraw,
}

/// 读状态该收敛什么。
///
/// 接管模式只在 CA 就绪、后端地址已知时才补齐：缺任何一项，写出去的配置都指向一个还不
/// 存在的代理，比不写更糟。不接管的模式一律撤回——`clear_proxy_settings` 自己按标记认领，
/// 别人写的配置一根手指都不碰，所以"每次都调"是安全的，也正是它让并发切换后的不一致能
/// 在下一次刷新里自愈。隔离数据目录下什么都不做。
fn status_convergence(
    mode: CursorMode,
    ca: &CaState,
    backend_ready: bool,
    manages_settings: bool,
) -> Convergence {
    if !manages_settings {
        return Convergence::Nothing;
    }
    if mode.takes_over() {
        if matches!(ca, CaState::Ready) && backend_ready {
            Convergence::TakeOver
        } else {
            Convergence::Nothing
        }
    } else {
        Convergence::Withdraw
    }
}

/// 服务端起来时要不要按当前模式收敛一次接管（见 `set_backend_addr`）。
///
/// 没有运行时（测试里直接构造 harness）时不起任务：收敛只是补一条启动路径，不该让记录
/// 地址这个动作 panic。
fn should_converge_at_startup(manages_settings: bool, runtime_available: bool) -> bool {
    manages_settings && runtime_available
}

/// 一次接管收敛是谁要求的。
///
/// 两条来路对"账号注入失败"和"能不能结束 Cursor"的口径都不一样，用一个结构体把两件事
/// 摆明，免得调用点上出现两个读不懂的 bool。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TakeoverRequest {
    /// 是否允许结束正在运行的 Cursor。
    may_terminate_cursor: bool,
    /// 账号注入失败是否让这次调用失败。
    account_failure_is_fatal: bool,
}

impl TakeoverRequest {
    /// 读状态时的补齐：**绝不结束进程**，账号注入失败只记日志。
    ///
    /// 与清除路径同一个口径（`clear_injected_account` 一直吞错）：状态快照是用户查明
    /// "为什么没生效"的唯一入口，不能因为一次可以下一轮再试的注入失败就整个失败——那样
    /// 界面连"现在到底是什么状态"都读不到，模式卡片也切不动。
    const STATUS_REFRESH: Self = Self {
        may_terminate_cursor: false,
        account_failure_is_fatal: false,
    };

    /// 用户在界面上选定的模式：注入是本地模式的前置条件，失败要让这次切换失败——否则
    /// 用户拿到的是"看起来切过去了、其实什么都通不了"。
    fn user_choice(may_terminate_cursor: bool) -> Self {
        Self {
            may_terminate_cursor,
            account_failure_is_fatal: true,
        }
    }

    /// 是否该在这之前结束 Cursor：用户明确选的，而且配置还不是我们的。
    ///
    /// 配置已经指向本机时再杀一次没有任何收益，只有代价（未保存的编辑会丢）。
    fn should_terminate_cursor(self, settings_already_ours: bool) -> bool {
        self.may_terminate_cursor && !settings_already_ours
    }
}

/// 账号策略：只有"本地"模式需要、也只允许它留下本地账号；另外两个模式一发现
/// 我们写过的那串就清掉——官方模式要把 Cursor 交还给用户自己的账号，混合模式
/// 要求真的登录，都不能让一个假账号挡住登录入口。
///
/// 清不掉（数据库被客户端占着等）不能挡路：模式已经记下了，下一次状态刷新还会再试。
/// 注入失败按 `account_failure_is_fatal` 分口径：用户明确切换模式时要报出来（本地模式
/// 靠这个账号才通），读状态补齐时只记日志——见 `TakeoverRequest::STATUS_REFRESH`。
async fn reconcile_account(mode: CursorMode, account_failure_is_fatal: bool) -> Result<()> {
    if mode.injects_local_account() {
        return match account::inject_if_missing().await {
            Ok(_) => Ok(()),
            Err(error) if account_failure_is_fatal => Err(error),
            Err(error) => {
                tracing::warn!(
                    error = %crate::diagnostics::error_chain(&error),
                    "cannot inject the local Cursor account; the rest of the takeover stays"
                );
                Ok(())
            }
        };
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

async fn apply_cursor_configuration(
    proxy_url: &str,
    mode: CursorMode,
    request: TakeoverRequest,
) -> Result<()> {
    reconcile_account(mode, request.account_failure_is_fatal).await?;
    settings::write_proxy_settings(proxy_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CursorMode;

    /// 结束 Cursor 的两个条件缺一不可：用户**明确**选过（确认对话框里写明未保存内容会
    /// 丢），且配置还不是我们的。
    #[test]
    fn only_an_explicit_choice_may_terminate_cursor() {
        let refresh = TakeoverRequest::STATUS_REFRESH;
        // 刷新状态时绝不能结束进程——同机上另一个同类软件写回它自己的配置后，
        // 每几秒一次的刷新会把用户正在编辑的 Cursor 杀掉。
        assert!(!refresh.should_terminate_cursor(false));
        assert!(!refresh.should_terminate_cursor(true));
        // 用户按开关并确认过：配置还不是我们的，结束 Cursor 让它重新读取。
        assert!(TakeoverRequest::user_choice(true).should_terminate_cursor(false));
        // 配置已经指向本机，再杀一次只有代价。
        assert!(!TakeoverRequest::user_choice(true).should_terminate_cursor(true));
        // 用户选了模式，但不是"从官方跨回来"的那一步：也不许结束进程。
        assert!(!TakeoverRequest::user_choice(false).should_terminate_cursor(false));
    }

    /// 账号注入失败的口径：用户明确切换模式时要报出来（本地模式靠这个账号才通），
    /// 读状态补齐时只记日志——状态快照是用户查明原因的唯一入口，不能整个失败。
    #[test]
    fn only_the_users_choice_makes_an_account_failure_fatal() {
        assert!(!TakeoverRequest::STATUS_REFRESH.account_failure_is_fatal);
        assert!(TakeoverRequest::user_choice(false).account_failure_is_fatal);
        assert!(TakeoverRequest::user_choice(true).account_failure_is_fatal);
    }

    /// 读状态的收敛方向：接管模式补齐、其它模式撤回、隔离目录不动。
    ///
    /// 撤回那一支是 F4 的自愈面：并发切换或上一次撤回失败会留下"库里是官方、文件里还
    /// 指着本机代理"的残局，下一次刷新必须把它抹平（`clear_proxy_settings` 按标记认领，
    /// 别人写的配置不碰，所以每次刷新都调是安全的）。
    #[test]
    fn a_status_refresh_converges_each_mode_in_one_direction() {
        assert_eq!(
            status_convergence(CursorMode::Local, &CaState::Ready, true, true),
            Convergence::TakeOver
        );
        // CA 还没信任 / 后端地址还不知道：写出去的配置会指向一个不存在的代理。
        assert_eq!(
            status_convergence(CursorMode::Local, &CaState::Untrusted, true, true),
            Convergence::Nothing
        );
        assert_eq!(
            status_convergence(CursorMode::Mixed, &CaState::Ready, false, true),
            Convergence::Nothing
        );
        assert_eq!(
            status_convergence(CursorMode::Official, &CaState::Ready, true, true),
            Convergence::Withdraw
        );
        // 隔离数据目录：那里没有用户的 Cursor。
        assert_eq!(
            status_convergence(CursorMode::Official, &CaState::Ready, true, false),
            Convergence::Nothing
        );
        assert_eq!(
            status_convergence(CursorMode::Local, &CaState::Ready, true, false),
            Convergence::Nothing
        );
    }

    /// 启动收敛（静默自启的唯一入口）需要运行时与真实数据目录。
    #[test]
    fn the_startup_convergence_needs_a_runtime_and_a_real_data_directory() {
        assert!(should_converge_at_startup(true, true));
        // 测试里直接构造 harness：没有运行时，起不了任务，也不该 panic。
        assert!(!should_converge_at_startup(true, false));
        assert!(!should_converge_at_startup(false, true));
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
