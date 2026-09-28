//! Owns reusable outbound HTTP clients configured from persisted proxy settings.

use std::{sync::Arc, time::Duration};

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

/// 构建出网客户端时选定的路由：模式、自定义地址、以及默认模式下的系统代理指纹。
///
/// 三者合起来回答"当时请求会发到哪里"。任何一项与当前配置不同，缓存里的客户端
/// 就不再代表现在的选择：默认模式下关掉系统代理、自定义模式下设置被别的路径改掉，
/// 都会命中这个判据。
#[derive(Clone, PartialEq, Eq)]
struct RouteSnapshot {
    mode: ProxyMode,
    address: String,
    /// 只有默认模式会读系统代理，其余模式留空。
    system_fingerprint: Option<String>,
}

impl RouteSnapshot {
    fn of(settings: &ProxySettingsSecret) -> Self {
        Self {
            mode: settings.mode,
            address: settings.address.clone(),
            system_fingerprint: (settings.mode == ProxyMode::Default)
                .then(system_proxy_fingerprint),
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
        let client = settings_builder(&settings)?.build()?;
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
        let client = settings_builder(&settings)?
            .redirect(reqwest::redirect::Policy::none())
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
            .build()?;
        cache.devin = Some(client.clone());
        cache.route = Some(RouteSnapshot::of(&settings));
        Ok(client)
    }

    pub async fn invalidate(&self) {
        *self.cache.write().await = ClientCache::default();
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

    /// 只在缓存已过期时重建 provider 客户端：出网请求遇到连接类失败后调用。
    ///
    /// 返回 `None` 表示重建也改变不了结果（配置没变，或当前不是默认模式），
    /// 调用方应当照旧上报失败。
    pub async fn rebuild_provider_client(&self, timeout: Duration) -> Option<reqwest::Client> {
        if !self.outbound_route_changed().await {
            return None;
        }
        self.invalidate().await;
        match self.provider_client(timeout).await {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::warn!(%error, "failed to rebuild the outbound client after a connection failure");
                None
            }
        }
    }
}

/// 一次出网请求用的客户端，外加"失败后按当前配置重建"的能力。
///
/// 关掉代理之后，缓存里的客户端仍然指向已经没人监听的代理端口，请求会立刻连接
/// 失败。持有这个句柄的调用点可以换一个按当前配置重建的客户端重试一次，而不是把
/// 用户这一轮对话直接判死。
#[derive(Clone)]
pub struct OutboundClient {
    client: reqwest::Client,
    recovery: Option<(NetworkClients, Duration)>,
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
            recovery: Some((clients, timeout)),
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// 出网失败后的兜底：只有"连接类失败 + 缓存已经过期"才会重建并返回新客户端，
    /// 供调用方重试一次。返回 `None` 表示失败照旧上报。
    pub async fn rebuild_after(&self, error: &reqwest::Error) -> Option<reqwest::Client> {
        let (clients, timeout) = self.recovery.as_ref()?;
        if !error.is_connect() && !error.is_timeout() {
            return None;
        }
        clients.rebuild_provider_client(*timeout).await
    }
}

/// 默认模式下出网走哪个代理由系统配置决定，而这份配置在应用运行期间会变
/// （关掉 Clash、换端口、改例外）。reqwest 只在构建客户端时读一次，所以这里
/// 用一个指纹把它记下来：变了就丢掉缓存的客户端，让下一次请求按新配置重建。
///
/// 指纹覆盖所有会改变 reqwest 判定的输入：环境变量（所有平台）与 Windows 注册表
/// 的 `Internet Settings`。它只用来比较是否变化，格式本身没有含义。
/// 诊断快照也会带上它：出现"出网突然不通"时，先看这一步有没有变。
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
            std::env::var(name).unwrap_or_default()
        ));
    }
    #[cfg(windows)]
    parts.push(windows_registry_fingerprint());
    parts.join("\n")
}

/// Windows 的系统代理就在注册表里，reqwest 的 system-proxy 特性同样只读这三项。
#[cfg(windows)]
fn windows_registry_fingerprint() -> String {
    const INTERNET_SETTINGS: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings";
    let Ok(settings) = windows_registry::CURRENT_USER.open(INTERNET_SETTINGS) else {
        return "registry=<unreadable>".into();
    };
    format!(
        "registry={}|{}|{}",
        settings.get_u32("ProxyEnable").unwrap_or(0),
        settings.get_string("ProxyServer").unwrap_or_default(),
        settings.get_string("ProxyOverride").unwrap_or_default(),
    )
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
        let current = fingerprint();
        if current == previous {
            continue;
        }
        previous = current;
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

pub async fn client_builder(store: &Store) -> Result<reqwest::ClientBuilder> {
    let settings = store.proxy_settings_secret().await?;
    settings_builder(&settings)
}

/// 由一份代理设置构建请求客户端。缓存构建与 `client_builder` 共用这条路径，
/// 保证"记进缓存的指纹"和"客户端实际走的路由"来自同一次读取。
fn settings_builder(settings: &ProxySettingsSecret) -> Result<reqwest::ClientBuilder> {
    // Use the platform TLS stack for compatibility with provider gateways that
    // only offer legacy TLS 1.2 cipher suites unsupported by rustls.
    let builder = reqwest::Client::builder().use_native_tls();
    Ok(match outbound_proxy(settings)? {
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
    let builder = reqwest::blocking::Client::builder().use_native_tls();
    Ok(match outbound_proxy(&settings)? {
        OutboundProxy::System => builder,
        OutboundProxy::Custom(proxy) => builder.proxy(*proxy),
        OutboundProxy::Direct => builder.no_proxy(),
    })
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
}
