//! Owns reusable outbound HTTP clients configured from persisted proxy settings.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::{
    store::{ProxyMode, ProxySettingsSecret, Store},
    Error, Result,
};

const LOCAL_NO_PROXY: &str = "localhost,127.0.0.0/8,::1";

/// 系统代理配置的轮询间隔。
///
/// 默认模式下的客户端把"用哪个代理"钉在构建那一刻（见 `client_builder`），而用户
/// 完全可能在应用运行期间开关代理。间隔取得比人类察觉的延迟短、又比一次注册表读
/// 贵得多——一次读只有几个微秒。
const SYSTEM_PROXY_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// "系统代理连不上"之后绕开它直连的时长。
///
/// 到期就放回系统代理试一次：代理回来了就正常用，仍然不通就再兜一轮。用时间而不是
/// 主动探测：要判"系统代理到底是哪个地址"得复刻 reqwest 的整套判定（环境变量、注册表、
/// 例外表），判错一次就白发一批请求；而到期重试的代价只是每轮一次失败请求。
const SYSTEM_PROXY_BYPASS: Duration = Duration::from_secs(5 * 60);

/// 系统代理是否刚刚被判定为不可用，以及这个判定何时过期。
///
/// 它是机器的事实而不是某个客户端的事实，所以放进程级：所有出网客户端（含一次性构建
/// 的）都据此决定这次是走系统代理还是直连。
static SYSTEM_PROXY_BYPASS_UNTIL: parking_lot::Mutex<Option<std::time::Instant>> =
    parking_lot::Mutex::new(None);

fn bypassing_system_proxy() -> bool {
    SYSTEM_PROXY_BYPASS_UNTIL
        .lock()
        .is_some_and(|until| std::time::Instant::now() < until)
}

/// 出网此刻是否在绕开系统代理直连。设置页拿它说明与配置不一致的运行时状态。
pub fn system_proxy_bypassed() -> bool {
    bypassing_system_proxy()
}

/// 出网路由的代次：每一次"缓存按新配置失效"都 +1（[`NetworkClients::invalidate`] 是唯一入口）。
///
/// 客户端缓存是按实例记的（`ClientCache`），所以自己握着长生命周期实例的调用方收不到
/// 别人的 `invalidate` 通知：它记的是"建实例那一刻"的代理设置，用户在一次 run 中途换
/// 代理时，下一次出网还会走旧路由。代次给这类调用方一个共同的判据——变了就重建实例
/// （见 `cursor::tools` 里搜索那份客户端的用法）。计数只用于比较，不需要跨线程发布
/// 别的数据，所以 `Relaxed` 就够。
static ROUTE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 当前出网路由的代次。见 [`ROUTE_GENERATION`]。
pub fn route_generation() -> u64 {
    ROUTE_GENERATION.load(Ordering::Relaxed)
}

#[derive(Clone)]
pub struct NetworkClients {
    store: Store,
    cache: Arc<RwLock<ClientCache>>,
}

#[derive(Default)]
struct ClientCache {
    default: Option<reqwest::Client>,
    cursor: Option<reqwest::Client>,
    provider: Option<(Duration, reqwest::Client)>,
    devin: Option<reqwest::Client>,
    /// 构建这批客户端时的出网路由；用来判断缓存是否已经过期。
    route: Option<RouteSnapshot>,
}

/// 构建出网客户端时选定的路由：模式、自定义地址、系统代理指纹与兜底状态。
///
/// 四者合起来回答"当时请求会发到哪里"。任何一项与当前配置不同，缓存里的客户端
/// 就不再代表现在的选择：默认模式下关掉系统代理、自定义模式下设置被别的路径改掉、
/// 或者系统代理刚被判定连不上而改为直连，都会命中这个判据。
#[derive(Clone, PartialEq, Eq)]
struct RouteSnapshot {
    mode: ProxyMode,
    address: String,
    /// 只有默认模式会读系统代理，其余模式留空。
    system_fingerprint: Option<String>,
    /// 默认模式下系统代理不可用，出网临时绕开它直连。
    bypass_system_proxy: bool,
}

impl RouteSnapshot {
    fn of(settings: &ProxySettingsSecret) -> Self {
        let uses_system_proxy = settings.mode == ProxyMode::Default;
        Self {
            mode: settings.mode,
            address: settings.address.clone(),
            system_fingerprint: uses_system_proxy.then(system_proxy_fingerprint),
            bypass_system_proxy: uses_system_proxy && bypassing_system_proxy(),
        }
    }
}

impl NetworkClients {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            cache: Arc::new(RwLock::new(ClientCache::default())),
        }
    }

    pub async fn default_client(&self) -> Result<reqwest::Client> {
        if let Some(client) = self.cache.read().await.default.clone() {
            return Ok(client);
        }
        let mut cache = self.cache.write().await;
        if let Some(client) = cache.default.clone() {
            return Ok(client);
        }
        let settings = self.store.proxy_settings_secret().await?;
        let client = settings_builder(&settings)?
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .build()?;
        cache.default = Some(client.clone());
        cache.route = Some(RouteSnapshot::of(&settings));
        Ok(client)
    }

    pub async fn cursor_client(&self) -> Result<reqwest::Client> {
        if let Some(client) = self.cache.read().await.cursor.clone() {
            return Ok(client);
        }
        let mut cache = self.cache.write().await;
        if let Some(client) = cache.cursor.clone() {
            return Ok(client);
        }
        let settings = self.store.proxy_settings_secret().await?;
        // 官方转发是流式的(含 BidiAppend/RunSSE 长连接),所以这里不给总时长上界,只给
        // "连不上"和"连上后一个字节都不回"两种故障一个上界:没有它时 TCP 被中间盒接住
        // 却不回话,请求会永远挂着,连错误日志都不会打一条。
        let client = settings_builder(&settings)?
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CURSOR_CONNECT_TIMEOUT)
            .read_timeout(CURSOR_IDLE_TIMEOUT)
            .build()?;
        cache.cursor = Some(client.clone());
        cache.route = Some(RouteSnapshot::of(&settings));
        Ok(client)
    }

    pub async fn provider_client(&self, timeout: Duration) -> Result<reqwest::Client> {
        if let Some((_, client)) = self
            .cache
            .read()
            .await
            .provider
            .as_ref()
            .filter(|(cached_timeout, _)| *cached_timeout == timeout)
        {
            return Ok(client.clone());
        }
        let mut cache = self.cache.write().await;
        if let Some((_, client)) = cache
            .provider
            .as_ref()
            .filter(|(cached_timeout, _)| *cached_timeout == timeout)
        {
            return Ok(client.clone());
        }
        let settings = self.store.proxy_settings_secret().await?;
        let client = settings_builder(&settings)?.timeout(timeout).build()?;
        cache.provider = Some((timeout, client.clone()));
        cache.route = Some(RouteSnapshot::of(&settings));
        Ok(client)
    }

    /// Devin 网关的上游请求。响应体要按原样转发，所以这个客户端关掉全部自动解压；
    /// 其余（TLS、出网代理）与其它客户端完全一致，跟着代理设置一起重建。
    ///
    /// 上界与 `cursor_client` 同一套取值，理由也一样：Devin 转发是长驻出网，
    /// `connect_timeout` 管"连不上"，`read_timeout` 是**每次读**的空闲上界（不是总时长），
    /// 所以正常的流式转发不会被掐断，只有"连上之后一个字节都不回"才会断开。没有这两条时，
    /// 中间盒接住 TCP 却不回话就会永远挂着，连一条错误日志都不会打。
    pub async fn devin_client(&self) -> Result<reqwest::Client> {
        if let Some(client) = self.cache.read().await.devin.clone() {
            return Ok(client);
        }
        let mut cache = self.cache.write().await;
        if let Some(client) = cache.devin.clone() {
            return Ok(client);
        }
        let settings = self.store.proxy_settings_secret().await?;
        let client = settings_builder(&settings)?
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(CURSOR_CONNECT_TIMEOUT)
            .read_timeout(CURSOR_IDLE_TIMEOUT)
            .build()?;
        cache.devin = Some(client.clone());
        cache.route = Some(RouteSnapshot::of(&settings));
        Ok(client)
    }

    pub async fn invalidate(&self) {
        // 按实例记客户端的调用方（搜索/抓取）靠这个计数知道自己那份该换了：它没有别的
        // 途径被通知到。三条件都在这里汇合——保存设置、系统代理变化、连不上系统代理
        // 改直连，全都经由本方法。
        ROUTE_GENERATION.fetch_add(1, Ordering::Relaxed);
        *self.cache.write().await = ClientCache::default();
    }

    /// 出网设置刚被用户保存：撤掉"系统代理连不上"的兜底判定，再丢掉缓存的客户端。
    ///
    /// 两步必须一起做。`invalidate` 只保证下一次请求按新配置重建客户端；兜底判定是
    /// 进程级的，不跟着缓存走——用户从别的模式切回"跟随系统代理"时，上一轮留下的
    /// 窗口还在，出网会继续直连最长五分钟（公司网络里直连等于全部失败），而设置页
    /// 显示的是"已保存"。保存设置就是"按我现在的配置再试一次"。
    pub async fn settings_saved(&self) {
        self.clear_system_proxy_bypass().await;
        self.invalidate().await;
    }

    /// 缓存里的出网路由是否已经不对应当前配置。
    ///
    /// 默认模式下系统代理的开关、端口、例外都在指纹里；自定义模式下地址由设置
    /// 决定。保存设置时已经失效过缓存，这条判据兜的是没走保存路径的变化——最典型
    /// 的就是用户中途关掉 Clash。
    pub async fn outbound_route_changed(&self) -> bool {
        let Some(cached) = self.cache.read().await.route.clone() else {
            return false;
        };
        match self.store.proxy_settings_secret().await {
            Ok(settings) => cached != RouteSnapshot::of(&settings),
            Err(error) => {
                tracing::warn!(%error, "cannot read proxy settings; keeping cached outbound clients");
                false
            }
        }
    }

    /// 只在缓存已过期时重建指定的出网客户端：出网请求遇到连接类失败后调用。
    ///
    /// "已过期"包含缓存为空的情况：刚被 `bypass_system_proxy` 判过直连兜底时缓存就是
    /// 空的，而调用方手里那份客户端仍然指向刚被判定连不上的系统代理——不重建的话，
    /// 重试只会把同一个失败再打一遍，兜底判定要等下一次请求才生效。
    ///
    /// 返回 `None` 表示重建也改变不了结果（配置没变且缓存还在），调用方应当照旧上报失败。
    async fn rebuild_cached_client(&self, cached: &CachedClient) -> Option<reqwest::Client> {
        if self.cache.read().await.route.is_some() && !self.outbound_route_changed().await {
            return None;
        }
        self.invalidate().await;
        let rebuilt = match cached {
            CachedClient::Provider(timeout) => self.provider_client(*timeout).await,
            CachedClient::Cursor => self.cursor_client().await,
            CachedClient::Devin => self.devin_client().await,
        };
        match rebuilt {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::warn!(%error, "failed to rebuild the outbound client after a connection failure");
                None
            }
        }
    }

    /// 默认模式下系统代理那一跳连不上：往后一段时间里绕开它直连。
    ///
    /// 返回 `true` 表示这次调用真的改变了路由（缓存已失效），调用方可以按新配置重建
    /// 客户端并重试一次。Clash 这类工具更新时会先停掉核心，端口还在、系统代理还开着，
    /// 但没人监听——没有这条兜底，用户的每一次对话都会以 502 结束。
    pub async fn bypass_system_proxy(&self) -> bool {
        self.bypass_system_proxy_if(configured_system_proxy).await
    }

    /// 兜底判定本体。
    ///
    /// "连不上系统代理"必须是**这一跳**的事实，而 `is_connect()` 分不清代理与目标：
    /// DNS 解析失败、端口无人监听、TLS 握手失败都算连接失败，于是"某个模型 base_url
    /// 打错、内网网关关机、本地 Ollama 没开"与"系统代理停了"在代码里长得一模一样。
    /// 所以先确认这台机器上真的配了系统代理：没有代理就没有"绕开代理"这回事，把
    /// 某个目标的连通性问题写成机器级事实，只会让全进程白白直连五分钟。
    ///
    /// `system_proxy` 是唯一的外部读数，参数化只为了测试能给定"这台机器配没配代理"。
    async fn bypass_system_proxy_if<F>(&self, system_proxy: F) -> bool
    where
        F: Fn() -> Option<String>,
    {
        // 只有"跟随系统代理"这一种模式会栽在系统代理上；直连或自定义模式下连接失败
        // 跟系统代理无关，别把判定记到它头上（用户之后切回默认模式会被无辜绕过）。
        match self.store.proxy_settings_secret().await {
            Ok(settings) if settings.mode == ProxyMode::Default => {}
            Ok(_) => return false,
            Err(error) => {
                tracing::warn!(
                    error = %crate::diagnostics::error_chain(&error),
                    "cannot read proxy settings; not bypassing the system proxy"
                );
                return false;
            }
        }
        if system_proxy().is_none() {
            tracing::debug!(
                "no system proxy is configured; a connection failure cannot be blamed on it"
            );
            return false;
        }
        {
            // 作用域内只碰内存：parking_lot 的守卫不是 `Send`，跨 await 会传染到所有
            // 调用方（provider 的流必须是 `Send`）。
            let mut until = SYSTEM_PROXY_BYPASS_UNTIL.lock();
            if until.is_some_and(|deadline| std::time::Instant::now() < deadline) {
                return false;
            }
            *until = Some(std::time::Instant::now() + SYSTEM_PROXY_BYPASS);
        }
        self.invalidate().await;
        tracing::warn!(
            seconds = SYSTEM_PROXY_BYPASS.as_secs(),
            "system proxy is unreachable; sending requests directly until it recovers"
        );
        true
    }

    /// 兜底窗口到期：放回系统代理再试一次。
    ///
    /// 失败就再兜一轮，成功就一直用系统代理。用时间而不是探测，理由见 `SYSTEM_PROXY_BYPASS`。
    pub async fn expire_system_proxy_bypass(&self) -> bool {
        {
            let mut until = SYSTEM_PROXY_BYPASS_UNTIL.lock();
            match *until {
                Some(deadline) if std::time::Instant::now() >= deadline => {
                    *until = None;
                }
                _ => return false,
            }
        }
        self.invalidate().await;
        tracing::info!("system proxy bypass expired; trying the configured proxy again");
        true
    }

    /// 系统代理配置刚变过：撤掉兜底判定，按新配置先试一次系统代理。
    ///
    /// 与 `expire_system_proxy_bypass` 分开：这条不看窗口，因为判断的依据（那个代理
    /// 地址）本身已经换了。
    pub async fn clear_system_proxy_bypass(&self) -> bool {
        if SYSTEM_PROXY_BYPASS_UNTIL.lock().take().is_none() {
            return false;
        }
        self.invalidate().await;
        tracing::info!("system proxy configuration changed; trying it again");
        true
    }
}

/// 出网句柄背后要按当前配置重建的缓存项。
///
/// 缓存里不止一种客户端（provider 还按请求超时各留一份），重建必须回到同一项，
/// 否则重试会悄悄换上一份不属于它的连接池。
#[derive(Clone)]
enum CachedClient {
    /// provider 请求用的客户端，按请求超时区分。
    Provider(Duration),
    /// Cursor 官方转发用的客户端。
    Cursor,
    /// Devin 网关转发用的客户端。
    Devin,
}

/// 一次出网请求用的客户端，外加"失败后按当前配置重建"的能力。
///
/// 关掉代理之后，缓存里的客户端仍然指向已经没人监听的代理端口，请求会立刻连接
/// 失败。持有这个句柄的调用点可以换一个按当前配置重建的客户端重试一次，而不是把
/// 用户这一轮对话直接判死。
#[derive(Clone)]
pub struct OutboundClient {
    client: reqwest::Client,
    recovery: Option<(NetworkClients, CachedClient)>,
}

impl OutboundClient {
    /// 固定客户端：不随设置变化重建。用于没有 store 的路径与测试。
    pub fn fixed(client: reqwest::Client) -> Self {
        Self {
            client,
            recovery: None,
        }
    }

    /// provider 请求用的客户端；`timeout` 必须与取客户端时用的一致，重建才会命中
    /// 同一个缓存项。
    pub async fn provider(clients: NetworkClients, timeout: Duration) -> Result<Self> {
        let client = clients.provider_client(timeout).await?;
        Ok(Self {
            client,
            recovery: Some((clients, CachedClient::Provider(timeout))),
        })
    }

    /// Cursor 官方转发用的客户端，恢复语义与 provider 路径完全一致。
    ///
    /// 系统代理那一跳断掉（Clash 更新时先停核心）时，官方请求以前会一路失败到用户
    /// 重启代理或重启本应用，因为没有任何路径会重建它。
    pub async fn cursor(clients: NetworkClients) -> Result<Self> {
        let client = clients.cursor_client().await?;
        Ok(Self {
            client,
            recovery: Some((clients, CachedClient::Cursor)),
        })
    }

    /// Devin 网关转发用的客户端，恢复语义与另外两条路径一致。
    ///
    /// Devin 的转发是长驻的，而"用哪个代理"在构建时就钉住了；系统代理那一跳断掉时
    /// 注册表与环境变量都没变，看门狗不会重建，于是纯 Devin 用户只能一路 502 到手动
    /// 保存一次出网设置或重启应用。
    pub async fn devin(clients: NetworkClients) -> Result<Self> {
        let client = clients.devin_client().await?;
        Ok(Self {
            client,
            recovery: Some((clients, CachedClient::Devin)),
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// 出网失败后的兜底：按当前配置重建客户端返回新客户端，供调用方重试一次。
    ///
    /// 两条路：配置真的变了（用户中途关了代理）就按新配置重建；配置没变却是连接类
    /// 失败，那就是系统代理那一跳断了，改为直连再重建。返回 `None` 表示失败照旧上报。
    ///
    /// 注意这里允许超时（连接被中间盒接住却不回话的代理属于这一类）：重建的意义是
    /// 刷新缓存里那条路由，让下一次请求不必再撞同一堵墙。**要不要重发由调用方决定**——
    /// `upstream::attempt` 只对"确定没送到上游"的连接类失败重发，请求级超时不重发。
    pub async fn rebuild_after(&self, error: &reqwest::Error) -> Option<reqwest::Client> {
        let (clients, cached) = self.recovery.as_ref()?;
        if !error.is_connect() && !error.is_timeout() {
            return None;
        }
        if let Some(client) = clients.rebuild_cached_client(cached).await {
            return Some(client);
        }
        if error.is_connect() && clients.bypass_system_proxy().await {
            return clients.rebuild_cached_client(cached).await;
        }
        None
    }
}

/// 本机此刻配置的系统代理地址；没配就返回 `None`。
///
/// 与 [`system_proxy_fingerprint`] 共用同一次读取（环境变量 + Windows 注册表），
/// 所以不存在第二份"系统代理到底是哪个地址"的判定。它回答的是"用户其实有一个
/// 代理可以用吗"：插件运行时下载失败时，界面靠它告诉用户该把出网切到哪个模式。
///
/// 判定顺序与 reqwest 一致：先环境变量（`HTTPS_PROXY` 等），再系统设置。
pub(crate) fn configured_system_proxy() -> Option<String> {
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    #[cfg(windows)]
    {
        let settings = windows_proxy_settings()?;
        if settings.enabled != 0 && !settings.server.trim().is_empty() {
            Some(settings.server)
        } else {
            None
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 默认模式下出网走哪个代理由系统配置决定，而这份配置在应用运行期间会变
/// （关掉 Clash、换端口、改例外）。reqwest 只在构建客户端时读一次，所以这里
/// 用一个指纹把它记下来：变了就丢掉缓存的客户端，让下一次请求按新配置重建。
///
/// 指纹覆盖所有会改变 reqwest 判定的输入：环境变量（所有平台）与 Windows 注册表
/// 的 `Internet Settings`。它只用来比较是否变化，格式本身没有含义。
/// 诊断快照也会带上它：出现"出网突然不通"时，先看这一步有没有变。
///
/// 每个值只留形状（是否设置、长度、短哈希）：代理地址里带 `user:pass@` 的企业代理
/// 很常见，而这份指纹会进 `diagnostics-*.log`——那个文件的用途就是让用户贴给别人看。
/// 原值没有任何诊断价值，比较语义却完全不变。
pub(crate) fn system_proxy_fingerprint() -> String {
    let mut parts = Vec::new();
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        parts.push(format!(
            "{name}={}",
            fingerprint_value(std::env::var(name).ok().as_deref())
        ));
    }
    #[cfg(windows)]
    parts.push(windows_registry_fingerprint());
    parts.join("\n")
}

/// 指纹里一个值的形状：`unset` / `set(长度,短哈希)`。
fn fingerprint_value(value: Option<&str>) -> String {
    match value {
        None => "unset".to_owned(),
        Some(value) => format!("set({},{})", value.len(), short_hash(value)),
    }
}

/// 值的短哈希：指纹只回答"变没变"，8 个十六进制字符足够，也短到能放进一行日志。
fn short_hash(value: &str) -> String {
    use sha2::Digest;

    let digest = sha2::Sha256::digest(value.as_bytes());
    hex::encode(&digest[..4])
}

/// Windows 的系统代理就在注册表里，reqwest 的 system-proxy 特性同样只读这三项。
#[cfg(windows)]
struct WindowsProxySettings {
    enabled: u32,
    server: String,
    bypass: String,
}

#[cfg(windows)]
fn windows_proxy_settings() -> Option<WindowsProxySettings> {
    const INTERNET_SETTINGS: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings";
    let settings = windows_registry::CURRENT_USER
        .open(INTERNET_SETTINGS)
        .ok()?;
    Some(WindowsProxySettings {
        enabled: settings.get_u32("ProxyEnable").unwrap_or(0),
        server: settings.get_string("ProxyServer").unwrap_or_default(),
        bypass: settings.get_string("ProxyOverride").unwrap_or_default(),
    })
}

#[cfg(windows)]
fn windows_registry_fingerprint() -> String {
    match windows_proxy_settings() {
        Some(settings) => format!(
            "registry={}|server={}|bypass={}",
            settings.enabled,
            fingerprint_value(Some(&settings.server)),
            fingerprint_value(Some(&settings.bypass))
        ),
        None => "registry=<unreadable>".into(),
    }
}

/// 盯着系统代理配置，变了就重建出网客户端。
///
/// 没有它时"关掉代理"的代价是重启应用：缓存的客户端会继续把请求发往一个已经没人
/// 监听的代理端口，而没有任何路径会重新读一次系统配置。
pub async fn watch_system_proxy(clients: NetworkClients, shutdown: CancellationToken) {
    watch_system_proxy_with(
        clients,
        shutdown,
        SYSTEM_PROXY_POLL_INTERVAL,
        system_proxy_fingerprint,
    )
    .await
}

async fn watch_system_proxy_with<F>(
    clients: NetworkClients,
    shutdown: CancellationToken,
    interval: Duration,
    fingerprint: F,
) where
    F: Fn() -> String,
{
    let cancelled = shutdown.cancelled();
    tokio::pin!(cancelled);
    let mut previous = fingerprint();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // interval 的第一次 tick 立即返回；先等满一个周期再开始比对。
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = &mut cancelled => return,
        }
        // 直连兜底只维持一个窗口，到点就放回系统代理试一次。
        clients.expire_system_proxy_bypass().await;
        let current = fingerprint();
        if current == previous {
            continue;
        }
        previous = current;
        // 系统代理地址刚换过：上一次"连不上"的判定不再代表现在，先撤掉再重建。
        clients.clear_system_proxy_bypass().await;
        // 只有"跟随系统代理"的缓存会用到系统配置；自定义与直连模式下缓存的客户端与
        // 系统代理无关，重建只会白白丢掉连接池（那两种模式的变化由保存设置失效缓存）。
        match clients.store.proxy_settings_secret().await {
            Ok(settings) if settings.mode != ProxyMode::Default => continue,
            Ok(_) => {}
            Err(error) => {
                // 读不出来时按"默认模式"处理：重建的成本远低于把用户继续钉在一个
                // 已经消失的代理端口上。
                tracing::warn!(
                    %error,
                    "proxy settings unreadable after a system proxy change; rebuilding outbound clients"
                );
            }
        }
        clients.invalidate().await;
        tracing::info!("system proxy configuration changed; rebuilding outbound clients");
    }
}

/// 出网代理的三个来源，异步与阻塞两套客户端共用同一份判定。
///
/// `reqwest::Proxy` 自身有 256 字节，直接放在变体里会让整个枚举跟着变大（clippy 的
/// `large_enum_variant`）；这个值每次构建客户端才产生一次，装箱的代价可以忽略。
enum OutboundProxy {
    /// reqwest 的默认行为：构建客户端时读环境变量与系统配置（Windows 注册表）。
    System,
    Custom(Box<reqwest::Proxy>),
    Direct,
}

fn outbound_proxy(settings: &ProxySettingsSecret) -> Result<OutboundProxy> {
    Ok(match settings.mode {
        ProxyMode::Default => OutboundProxy::System,
        ProxyMode::Custom => OutboundProxy::Custom(Box::new(custom_proxy(settings)?)),
        ProxyMode::Direct => OutboundProxy::Direct,
    })
}

/// 控制面请求（模型发现等）的时间上界。"连得上却不回话"的 base_url 很常见——占位域名、
/// 内网网关黑洞——没有上界时这条请求会在服务端一直挂着。流式路径不走这个客户端：
/// 官方转发与 Devin 透传要保持长连接，provider 请求自带预算。
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 官方转发的建连上界：连不上要走既有的"绕开系统代理重试一次"逻辑，而不是干等内核超时。
const CURSOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 官方转发的空闲上界：沿用 provider 流式路径同一份预算（`provider_stream_idle_timeout`
/// 的默认值）。一次对话可以跑很久，但"一个字节都不回"必须是可断开的故障。
const CURSOR_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub async fn client_builder(store: &Store) -> Result<reqwest::ClientBuilder> {
    let settings = store.proxy_settings_secret().await?;
    settings_builder(&settings)
}

/// 由一份代理设置构建请求客户端。缓存构建与 `client_builder` 共用这条路径，
/// 保证"记进缓存的指纹"和"客户端实际走的路由"来自同一次读取。
fn settings_builder(settings: &ProxySettingsSecret) -> Result<reqwest::ClientBuilder> {
    // Use the platform TLS stack for compatibility with provider gateways that
    // only offer legacy TLS 1.2 cipher suites unsupported by rustls.
    let builder = reqwest::Client::builder()
        .use_native_tls()
        .redirect(same_origin_redirect());
    Ok(match outbound_proxy(settings)? {
        // 系统代理刚被判定连不上：这一次直接出网，别把请求打进没人监听的端口。
        OutboundProxy::System if bypassing_system_proxy() => builder.no_proxy(),
        OutboundProxy::System => builder,
        OutboundProxy::Custom(proxy) => builder.proxy(*proxy),
        OutboundProxy::Direct => builder.no_proxy(),
    })
}

pub async fn client(store: &Store) -> Result<reqwest::Client> {
    Ok(client_builder(store).await?.build()?)
}

pub async fn blocking_client_builder(store: &Store) -> Result<reqwest::blocking::ClientBuilder> {
    let settings = store.proxy_settings_secret().await?;
    let builder = reqwest::blocking::Client::builder()
        .use_native_tls()
        .redirect(same_origin_redirect());
    Ok(match outbound_proxy(&settings)? {
        OutboundProxy::System if bypassing_system_proxy() => builder.no_proxy(),
        OutboundProxy::System => builder,
        OutboundProxy::Custom(proxy) => builder.proxy(*proxy),
        OutboundProxy::Direct => builder.no_proxy(),
    })
}

/// 出网请求的重定向策略：同源放行，跨源停止。
///
/// reqwest 默认跟随最多 10 跳，而且在跳转时保留自定义头——`x-api-key`、Anthropic 的
/// `x-api-key`、用户为网关配的自定义头都在里面。上游只要回一个 302 指向别的主机，
/// 密钥就跟着交出去了；模型厂商的正文里本来就可能有可控字段（网关回显、错误页）。
/// 同源（同 scheme + host + port）跳转仍然放行：上游内部的路径规整（`/v1` → `/v1/`）
/// 是常见且安全的。`Policy::none()` 的调用方（Cursor 官方转发）比这更严，不受影响。
fn same_origin_redirect() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| match attempt.previous().last() {
        Some(previous) if !same_origin(previous, attempt.url()) => {
            tracing::warn!(
                from = %previous,
                to = %attempt.url(),
                "refusing a cross-origin redirect from an upstream"
            );
            attempt.stop()
        }
        _ => attempt.follow(),
    })
}

fn same_origin(previous: &reqwest::Url, next: &reqwest::Url) -> bool {
    previous.scheme() == next.scheme()
        && previous.host_str() == next.host_str()
        && previous.port_or_known_default() == next.port_or_known_default()
}

fn custom_proxy(settings: &ProxySettingsSecret) -> Result<reqwest::Proxy> {
    let mut proxy = reqwest::Proxy::all(&settings.address)?
        .no_proxy(reqwest::NoProxy::from_string(LOCAL_NO_PROXY));
    if settings.auth_enabled {
        proxy = proxy.basic_auth(&settings.username, &settings.password);
    }
    Ok(proxy)
}

pub fn reject_self_proxy(address: &str, local_proxy_port: u16) -> Result<()> {
    if local_proxy_port == 0 {
        return Ok(());
    }
    let url = url::Url::parse(address)
        .map_err(|error| Error::Config(format!("invalid proxy address: {error}")))?;
    if url.port_or_known_default() == Some(local_proxy_port) && url_host_is_loopback(&url) {
        return Err(Error::Config(
            "proxy address cannot point to the haxsd byok local proxy".into(),
        ));
    }
    Ok(())
}

fn url_host_is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => {
            host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        }
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::store::ProxyMode;

    fn custom_settings(address: String) -> ProxySettingsSecret {
        ProxySettingsSecret {
            mode: ProxyMode::Custom,
            address,
            auth_enabled: false,
            username: String::new(),
            password: String::new(),
        }
    }

    async fn test_store() -> Store {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        Store::connect(&url).await.unwrap()
    }

    /// 缓存里已经建好的客户端个数；测试用它观察失效与否。
    async fn cached_clients(clients: &NetworkClients) -> usize {
        let cache = clients.cache.read().await;
        [
            cache.default.is_some(),
            cache.cursor.is_some(),
            cache.provider.is_some(),
            cache.devin.is_some(),
        ]
        .iter()
        .filter(|cached| **cached)
        .count()
    }

    async fn wait_until<F, Fut>(mut condition: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        tokio::time::timeout(Duration::from_secs(5), async move {
            while !condition().await {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("condition was not reached before the timeout");
    }

    fn system_proxy_store_mode(mode: ProxyMode) -> crate::store::ProxySettingsInput {
        crate::store::ProxySettingsInput {
            mode,
            address: "http://127.0.0.1:7890".into(),
            auth_enabled: false,
            username: String::new(),
            password: None,
        }
    }

    /// "这台机器配了系统代理"；`configured_system_proxy` 的真实读数由机器决定，
    /// 用例只关心判定本身。
    fn configured_proxy() -> Option<String> {
        Some("http://127.0.0.1:7890".into())
    }

    fn proxy_settings_input(mode: ProxyMode, address: String) -> crate::store::ProxySettingsInput {
        crate::store::ProxySettingsInput {
            mode,
            address,
            auth_enabled: false,
            username: String::new(),
            password: None,
        }
    }

    /// 绑定一个端口再放掉：地址确定，但上面肯定没人监听。
    async fn dead_address() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        address.to_string()
    }

    /// 最小 HTTP 代理桩：接受一条连接、读掉请求、回一个固定的 200。
    async fn proxy_stub() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = connections.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            }
        });
        (format!("http://{address}"), connections)
    }

    /// 连得上却不回话的上游（占位域名、内网黑洞）不能让模型发现在服务端无界挂住。
    /// 建好 store 之后才暂停时钟：sqlx 的连接池超时也走 tokio 定时器，早暂停会让它
    /// 连不上库。暂停后虚拟时间会自动跳到定时器，用例不必真等 30 秒。
    #[tokio::test]
    async fn the_default_client_gives_up_when_the_upstream_never_answers() {
        let store = test_store().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // 只收连接、永不回话：这正是"超时"要处理的那种上游。
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let clients = NetworkClients::new(store);
        let client = clients.default_client().await.unwrap();
        tokio::time::pause();

        let started = std::time::Instant::now();
        // 兜底上界比客户端自己的上界更晚，且是"暂停的虚拟时钟"上更晚：定时器一坏，
        // 虚拟时间会走到这里并干净地失败，而不是让整条用例永远挂着（没有定时器时
        // 暂停的时钟不前进，挂住才是默认结局）。
        let guard_grace = DEFAULT_REQUEST_TIMEOUT + Duration::from_secs(5);
        let result = tokio::select! {
            result = client.get(format!("http://{address}/v1/models")).send() => result,
            _ = tokio::time::sleep(guard_grace) => panic!(
                "客户端自己的 {DEFAULT_REQUEST_TIMEOUT:?} 上界没生效：虚拟时钟已走过 {guard_grace:?}"
            ),
        };

        assert!(result.is_err(), "不回话的上游必须以失败结束,而不是一直挂着");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "上界由客户端自己给,不该真的等满: {:?}",
            started.elapsed()
        );
    }

    /// 官方转发是流式的,不能给总时长上界;但"连上之后一个字节都不回"必须能断开——
    /// 中间盒接住 TCP 却不回话时,没有上界就是永远挂着,连一条错误日志都不会打。
    #[tokio::test]
    async fn the_cursor_client_gives_up_when_the_upstream_stays_silent() {
        let store = test_store().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let clients = NetworkClients::new(store);
        let client = clients.cursor_client().await.unwrap();
        tokio::time::pause();

        let started = std::time::Instant::now();
        // 同 `the_default_client_gives_up_when_the_upstream_never_answers`：兜底上界比
        // 客户端自己的空闲上界更晚，定时器一坏就是干净失败而不是挂住。
        let guard_grace = CURSOR_IDLE_TIMEOUT + Duration::from_secs(5);
        let result = tokio::select! {
            result = client
                .post(format!("http://{address}/aiserver.v1.AiService/BidiAppend"))
                .body("x")
                .send() => result,
            _ = tokio::time::sleep(guard_grace) => panic!(
                "客户端自己的 {CURSOR_IDLE_TIMEOUT:?} 空闲上界没生效：虚拟时钟已走过 {guard_grace:?}"
            ),
        };

        assert!(result.is_err(), "静默的上游必须被断开,而不是永远挂着");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "上界由客户端自己给,不该真的等满: {:?}",
            started.elapsed()
        );
    }

    /// Devin 转发同样是长驻出网：系统代理那一跳死了要能重建（gateway 侧），而
    /// "连上之后一个字节都不回"必须能自己断开，否则中间盒接住 TCP 就是永远挂着。
    #[tokio::test]
    async fn the_devin_client_gives_up_when_the_upstream_stays_silent() {
        let store = test_store().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let clients = NetworkClients::new(store);
        let client = clients.devin_client().await.unwrap();
        tokio::time::pause();

        let guard_grace = CURSOR_IDLE_TIMEOUT + Duration::from_secs(5);
        let result = tokio::select! {
            result = client
                .post(format!("http://{address}/v1/chat/completions"))
                .body("x")
                .send() => result,
            _ = tokio::time::sleep(guard_grace) => panic!(
                "Devin 客户端的 {CURSOR_IDLE_TIMEOUT:?} 空闲上界没生效：虚拟时钟已走过 {guard_grace:?}"
            ),
        };

        assert!(result.is_err(), "静默的上游必须被断开,而不是永远挂着");
    }

    /// 按原样转发的第二个不变量：Devin 客户端**不协商压缩**。
    ///
    /// 响应体在网关那层是被直接转发的，自动解压会把模型看到的字节改掉（网关自己的
    /// 注释说明了这一点）。这个客户端每次重建（出网路由变化后 `rebuild_after` 走的就是
    /// 重新构建）都必须保留这条，所以这里用"上游收到的 accept-encoding"直接断言，
    /// 并拿 default_client 做对照——它照常协商 gzip。
    #[tokio::test]
    async fn the_devin_client_never_negotiates_compression() {
        use std::sync::Mutex;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0_u8; 4096];
                let read = socket.read(&mut request).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
                let encoding = text
                    .lines()
                    .find_map(|line| line.strip_prefix("accept-encoding:"))
                    .map(|value| value.trim().to_owned())
                    .unwrap_or_default();
                recorded.lock().unwrap().push(encoding);
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                    )
                    .await;
            }
        });
        let store = test_store().await;
        let clients = NetworkClients::new(store.clone());

        clients
            .devin_client()
            .await
            .unwrap()
            .get(format!("http://{address}/v1/chat/completions"))
            .send()
            .await
            .unwrap();
        clients
            .default_client()
            .await
            .unwrap()
            .get(format!("http://{address}/v1/models"))
            .send()
            .await
            .unwrap();

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "两次请求都要被记下来");
        assert!(
            !seen[0].contains("gzip") && !seen[0].contains("deflate"),
            "Devin 客户端不许协商压缩: {:?}",
            seen[0]
        );
        assert!(
            seen[1].contains("gzip"),
            "对照组 default_client 照常协商压缩: {:?}",
            seen[1]
        );
    }

    /// 三条失效路径都必须推进代次：保存设置、系统代理变化、连不上系统代理改直连。
    ///
    /// 自己按实例记客户端的调用方（搜索/抓取，见 `cursor::tools::SearchSource`）只认这个
    /// 判据，漏一条就等于那条路径上换了代理也不会生效。三条都经由 `invalidate`，所以这条
    /// 用例同时钉住"别把 +1 挪出 `invalidate`"。
    #[tokio::test]
    async fn every_outbound_route_invalidation_bumps_the_generation() {
        let store = test_store().await;
        store
            .set_proxy_settings(system_proxy_store_mode(ProxyMode::Default))
            .await
            .unwrap();
        let clients = NetworkClients::new(store);

        let before = route_generation();
        clients.settings_saved().await;
        assert!(
            route_generation() > before,
            "保存设置必须推进代次（否则搜索会继续走旧代理）"
        );

        let before = route_generation();
        assert!(
            clients.bypass_system_proxy_if(configured_proxy).await,
            "先判定系统代理连不上，才能看这条路径有没有推进代次"
        );
        assert!(route_generation() > before, "直连兜底必须推进代次");

        let before = route_generation();
        assert!(
            clients.clear_system_proxy_bypass().await,
            "上一行留下的兜底窗口要在这里撤掉"
        );
        assert!(route_generation() > before, "系统代理变化必须推进代次");
    }

    /// 系统代理那一跳断了（Clash 更新时先停核心就是这种）：出网临时改为直连，
    /// 窗口到期后再放回系统代理试一次；用户中途保存出网设置时立刻放回。
    ///
    /// 走 `bypass_system_proxy_if` 并给定"这台机器配了系统代理"：兜底判定现在还要看
    /// 系统代理是否真的存在，而那份读数是机器的（环境变量 + Windows 注册表），
    /// 用例不该取决于跑测试的机器上有没有代理。
    #[tokio::test]
    async fn an_unreachable_system_proxy_bypasses_itself_until_the_window_expires() {
        let store = test_store().await;
        store
            .set_proxy_settings(system_proxy_store_mode(ProxyMode::Default))
            .await
            .unwrap();
        let clients = NetworkClients::new(store);
        clients.default_client().await.unwrap();

        assert!(
            clients.bypass_system_proxy_if(configured_proxy).await,
            "the first judgement switches the route"
        );
        assert_eq!(
            cached_clients(&clients).await,
            0,
            "proxied clients are dropped"
        );
        assert!(
            !clients.bypass_system_proxy_if(configured_proxy).await,
            "a second failure inside the window keeps the route"
        );

        // 窗口没到：不放回系统代理。
        assert!(!clients.expire_system_proxy_bypass().await);
        // 把窗口推到过去，等价于"五分钟到了"。
        *SYSTEM_PROXY_BYPASS_UNTIL.lock() =
            Some(std::time::Instant::now() - Duration::from_secs(1));
        assert!(clients.expire_system_proxy_bypass().await);
        assert!(!bypassing_system_proxy());

        // 用户保存出网设置（例如从直连切回"跟随系统代理"）：上一轮"连不上"的判定
        // 必须立刻作废，否则最长五分钟里请求仍在直连，而设置页显示的是"已保存"。
        assert!(
            clients.bypass_system_proxy_if(configured_proxy).await,
            "再次失败重新兜底"
        );
        clients.default_client().await.unwrap();
        clients.settings_saved().await;
        assert!(!system_proxy_bypassed(), "保存设置后必须先按系统代理试一次");
        assert_eq!(
            cached_clients(&clients).await,
            0,
            "按新配置重建：旧客户端不能留"
        );
    }

    /// 连接失败不等于"系统代理连不上"。
    ///
    /// `is_connect()` 覆盖连接器的全部失败——DNS 解析不了、目标端口没人监听、TLS 握手
    /// 失败都算——所以"某个模型的 base_url 打错、本地 Ollama 没开"和"系统代理停了"在
    /// 代码里长得一模一样。这台机器根本没配系统代理时，把后者写进进程级状态只会让
    /// 全进程白白直连五分钟（公司网络里等于全部 502）。
    #[tokio::test]
    async fn a_connection_failure_without_a_configured_system_proxy_keeps_the_route() {
        let store = test_store().await;
        store
            .set_proxy_settings(system_proxy_store_mode(ProxyMode::Default))
            .await
            .unwrap();
        let clients = NetworkClients::new(store);
        clients.default_client().await.unwrap();

        assert!(
            !clients.bypass_system_proxy_if(|| None).await,
            "没有系统代理可绕开时不许改路由"
        );
        assert_eq!(
            cached_clients(&clients).await,
            1,
            "路由没变就不该丢掉缓存的客户端"
        );
    }

    /// 指纹只回答"变没变"，所以每个值只留形状：代理地址里的凭据（企业代理常见的
    /// `http://user:pass@proxy:8080`）会随诊断文件被贴出去，绝不能进这个串。
    #[test]
    fn a_fingerprint_part_keeps_only_the_shape_of_a_credentialed_value() {
        let credentialed = "http://user:secret@proxy.corp:8080";
        let part = fingerprint_value(Some(credentialed));

        assert!(!part.contains("secret"), "{part}");
        assert!(!part.contains("user"), "{part}");
        assert!(!part.contains("proxy.corp"), "{part}");
        assert_eq!(
            part,
            format!("set({},{})", credentialed.len(), short_hash(credentialed))
        );

        // 指纹的全部用途就是 `!=` 比较：同一个值必须稳定，改一个字符必须变。
        assert_eq!(part, fingerprint_value(Some(credentialed)));
        assert_ne!(
            part,
            fingerprint_value(Some("http://user:secret@proxy.corp:8081"))
        );
        assert_ne!(
            part,
            fingerprint_value(Some("http://user:other@proxy.corp:8080"))
        );
        assert_eq!(fingerprint_value(None), "unset");
        assert_ne!(fingerprint_value(None), fingerprint_value(Some("")));
    }

    /// Devin 转发以前没有任何自愈路径：它手里的客户端停在启动那一刻，系统代理那一跳
    /// 死掉时（Clash 停核心，注册表与环境变量都没变）看门狗不会重建，纯 Devin 用户
    /// 只能一路 502 到手动保存一次出网设置或重启应用。`OutboundClient::devin` 现在
    /// 与 provider / 官方转发走同一套"连接失败就按当前配置重建"的语义。
    #[tokio::test]
    async fn the_devin_client_is_rebuilt_after_a_connection_failure() {
        let store = test_store().await;
        store
            .set_proxy_settings(proxy_settings_input(
                ProxyMode::Custom,
                format!("http://{}", dead_address().await),
            ))
            .await
            .unwrap();
        let clients = NetworkClients::new(store.clone());
        let outbound = OutboundClient::devin(clients.clone()).await.unwrap();

        let error = outbound
            .client()
            .get("http://devin.invalid/v1/chat/completions")
            .send()
            .await
            .expect_err("指向没人监听的代理端口必须连接失败");
        assert!(error.is_connect(), "失败必须来自连接: {error:?}");

        // 代理核心重启后地址变了：重建必须回到 Devin 这一项（关掉全部自动解压的那份）。
        let (proxy, connections) = proxy_stub().await;
        store
            .set_proxy_settings(proxy_settings_input(ProxyMode::Custom, proxy))
            .await
            .unwrap();
        let rebuilt = outbound
            .rebuild_after(&error)
            .await
            .expect("路由变了必须给出重建后的客户端");

        let response = rebuilt
            .get("http://devin.invalid/v1/chat/completions")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            connections.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "重建后的客户端必须发到新代理"
        );
        assert!(
            !clients.outbound_route_changed().await,
            "缓存必须已经对应新配置"
        );
    }

    #[tokio::test]
    async fn a_system_proxy_change_drops_cached_clients_once() {
        let clients = NetworkClients::new(test_store().await);
        clients.default_client().await.unwrap();
        assert_eq!(cached_clients(&clients).await, 1);

        let fingerprint = Arc::new(std::sync::Mutex::new("before".to_string()));
        let shutdown = CancellationToken::new();
        let watcher = tokio::spawn(watch_system_proxy_with(
            clients.clone(),
            shutdown.clone(),
            Duration::from_millis(5),
            {
                let fingerprint = fingerprint.clone();
                move || fingerprint.lock().unwrap().clone()
            },
        ));

        // 指纹没变时一个客户端都不该被丢掉：每轮都重建会把连接池耗光。
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(cached_clients(&clients).await, 1);

        *fingerprint.lock().unwrap() = "after".into();
        wait_until(|| async { cached_clients(&clients).await == 0 }).await;

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), watcher)
            .await
            .expect("watcher must stop when the shutdown token is cancelled")
            .unwrap();
    }

    #[tokio::test]
    async fn a_system_proxy_change_keeps_clients_of_other_modes() {
        let store = test_store().await;
        store
            .set_proxy_settings(system_proxy_store_mode(ProxyMode::Custom))
            .await
            .unwrap();
        let clients = NetworkClients::new(store);
        clients.default_client().await.unwrap();

        let fingerprint = Arc::new(std::sync::Mutex::new("before".to_string()));
        let shutdown = CancellationToken::new();
        let watcher = tokio::spawn(watch_system_proxy_with(
            clients.clone(),
            shutdown.clone(),
            Duration::from_millis(5),
            {
                let fingerprint = fingerprint.clone();
                move || fingerprint.lock().unwrap().clone()
            },
        ));

        *fingerprint.lock().unwrap() = "after".into();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            cached_clients(&clients).await,
            1,
            "a custom proxy address does not depend on the system proxy"
        );

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), watcher).await;
    }

    #[tokio::test]
    async fn a_route_snapshot_notices_system_proxy_and_setting_changes() {
        let store = test_store().await;
        let clients = NetworkClients::new(store.clone());
        assert!(!clients.outbound_route_changed().await);

        clients.default_client().await.unwrap();
        assert!(!clients.outbound_route_changed().await);

        // 保存设置之前缓存就代表别的路由：这正是"没走保存路径的变化"。
        store
            .set_proxy_settings(system_proxy_store_mode(ProxyMode::Custom))
            .await
            .unwrap();
        assert!(clients.outbound_route_changed().await);

        clients.invalidate().await;
        clients.default_client().await.unwrap();
        assert!(!clients.outbound_route_changed().await);
    }

    #[test]
    fn a_route_snapshot_carries_the_system_fingerprint_only_in_default_mode() {
        assert!(RouteSnapshot::of(&ProxySettingsSecret::default())
            .system_fingerprint
            .is_some());
        assert!(
            RouteSnapshot::of(&custom_settings("http://127.0.0.1:7890".into()))
                .system_fingerprint
                .is_none()
        );
    }

    #[test]
    fn rejects_only_own_loopback_proxy_port() {
        for address in [
            "http://localhost:15721",
            "http://localhost.:15721",
            "http://127.0.0.2:15721",
            "http://[::1]:15721",
        ] {
            assert!(reject_self_proxy(address, 15721).is_err(), "{address}");
        }
        assert!(reject_self_proxy("http://127.0.0.1:7890", 15721).is_ok());
        assert!(reject_self_proxy("http://192.168.1.2:15721", 15721).is_ok());
        assert!(reject_self_proxy("http://127.0.0.1:15721", 0).is_ok());
    }

    #[tokio::test]
    async fn custom_proxy_bypasses_loopback_destinations() {
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let target = tokio::spawn(async move {
            let (mut socket, _) = target_listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let settings = custom_settings(format!("http://{proxy_address}"));
        let client = reqwest::Client::builder()
            .proxy(custom_proxy(&settings).unwrap())
            .build()
            .unwrap();

        let body = client
            .get(format!("http://{target_address}"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert_eq!(body, "ok");
        target.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), proxy_listener.accept())
                .await
                .is_err(),
            "loopback destination unexpectedly reached the configured proxy"
        );
    }

    /// 脚本化的最小 HTTP 桩：第 n 条连接回脚本里的第 n 条应答（越界就重复最后一条），
    /// 写完即关连接，于是连接数就等于"这个源被访问了几次"。
    async fn scripted_stub(script: Vec<Vec<u8>>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = connections.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let index = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = script
                    .get(index)
                    .or(script.last())
                    .cloned()
                    .unwrap_or_default();
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await;
                let _ = socket.write_all(&response).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}"), connections)
    }

    /// 跨源重定向必须停下来：请求头里带着 `x-api-key` 与用户自定义头，跟随一次跳转
    /// 就等于把它们交给另一个源。停下来的表现是调用方拿到那个 3xx，而不是被跟过去。
    #[tokio::test]
    async fn a_cross_origin_redirect_is_not_followed() {
        let (target, target_connections) = scripted_stub(vec![
            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".to_vec(),
        ])
        .await;
        let (redirector, redirector_connections) = scripted_stub(vec![format!(
            "HTTP/1.1 302 Found\r\nlocation: {target}/elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        )
        .into_bytes()])
        .await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(same_origin_redirect())
            .build()
            .unwrap();

        let response = client
            .get(format!("{redirector}/start"))
            .header("x-api-key", "secret")
            .send()
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            reqwest::StatusCode::FOUND,
            "跨源跳转必须停下，把 3xx 原样交给调用方"
        );
        assert_eq!(
            target_connections.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "另一个源一个字节都不该收到（密钥就在请求头里）"
        );
        assert_eq!(
            redirector_connections.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    /// 同源跳转照旧跟随：上游内部的路径规整（`/start` → `/next`）是常见且安全的。
    #[tokio::test]
    async fn a_same_origin_redirect_is_still_followed() {
        let (origin, connections) = scripted_stub(vec![
            b"HTTP/1.1 302 Found\r\nlocation: /next\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".to_vec(),
        ])
        .await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(same_origin_redirect())
            .build()
            .unwrap();

        let response = client.get(format!("{origin}/start")).send().await.unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");
        assert_eq!(
            connections.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "同源跳转必须被跟随一次"
        );
    }

    /// 端口不同就是另一个源：`127.0.0.1:8080` 与 `127.0.0.1:8081` 之间没有"同主机"的
    /// 豁免，凭据不能跟着过去。
    #[test]
    fn a_different_port_is_a_different_origin() {
        let previous = reqwest::Url::parse("https://api.example.com/v1/chat").unwrap();
        assert!(same_origin(
            &previous,
            &reqwest::Url::parse("https://api.example.com/v1/chat/").unwrap()
        ));
        assert!(same_origin(
            &previous,
            &reqwest::Url::parse("https://api.example.com:443/other").unwrap()
        ));
        assert!(!same_origin(
            &previous,
            &reqwest::Url::parse("https://api.example.com:8443/other").unwrap()
        ));
        assert!(!same_origin(
            &previous,
            &reqwest::Url::parse("http://api.example.com/v1/chat").unwrap()
        ));
        assert!(!same_origin(
            &previous,
            &reqwest::Url::parse("https://evil.example.com/v1/chat").unwrap()
        ));
    }
}
