//! Persists application settings.
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use serde::{Deserialize, Deserializer, Serialize};

use crate::Result;

use super::{now_ms, Store};

const PORT_SETTINGS_KEY: &str = "network_ports";
const PROXY_SETTINGS_KEY: &str = "outbound_proxy";
const TAB_SETTINGS_KEY: &str = "cursor_tab";
const DESKTOP_SETTINGS_KEY: &str = "desktop_lifecycle";
const COMMIT_SETTINGS_KEY: &str = "commit_settings";
const CURSOR_MODE_KEY: &str = "cursor_mode";
/// 旧版把"是否接管"存成一个布尔键。模式化之后它只剩一次性搬移的价值：
/// 读过一次就换算成 `cursor_mode` 并删掉，不再有第二处读它。
const LEGACY_CURSOR_TAKEOVER_KEY: &str = "cursor_takeover_enabled";
/// 每个模型的单价规则表。
const MODEL_PRICING_SETTINGS_KEY: &str = "model_token_pricing";
/// 旧版的全套全局价。只读键已不存在；唯一剩下的用途是用户成功保存第一条
/// 按模型规则后把它删掉，见 `Store::set_model_pricing`。
const LEGACY_PRICING_SETTINGS_KEY: &str = "token_pricing";
const DEVIN_SETTINGS_KEY: &str = "devin_router";

/// Embedded default system prompts for commit message generation.
pub const DEFAULT_COMMIT_PROMPT_ZH_CN: &str = include_str!("../../prompt/cursor/commit/zh-CN.md");
pub const DEFAULT_COMMIT_PROMPT_EN_US: &str = include_str!("../../prompt/cursor/commit/en-US.md");

pub const PUBLIC_TAB_SERVICE_URL: &str = "https://tab.leokun.cn";

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PortSettings {
    pub proxy_port: u16,
    pub service_port: u16,
}

/// 一组 Token 单价（每百万 token）。
///
/// `input_per_million` 指**缓存未命中**的输入，`cache_read_per_million` 指
/// **缓存命中**的输入；这是 DeepSeek 与 Claude 共用的计费口径。
/// 缺省价是 0：价格只能由用户为一个具体模型写明，代码不预填任何厂商的价。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TokenPrice {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_read_per_million: f64,
    pub cache_write_per_million: f64,
}

impl TokenPrice {
    /// 全 0：还没定价时的显式值。
    pub const fn zero() -> Self {
        Self {
            input_per_million: 0.0,
            output_per_million: 0.0,
            cache_read_per_million: 0.0,
            cache_write_per_million: 0.0,
        }
    }
}

impl Default for TokenPrice {
    fn default() -> Self {
        Self::zero()
    }
}

/// 首页「价值估算」使用的计价模式。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingMode {
    /// 全时段使用同一组单价。
    Fixed,
    /// 按高峰 / 低谷时段分别计价。
    PeakOffPeak,
}

/// 单一币种下的整套价格。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct CurrencyPricing {
    /// 「固定单价」模式使用的价格。
    pub fixed: TokenPrice,
    /// 高峰时段价格。
    pub peak: TokenPrice,
    /// 低谷时段价格。
    pub off_peak: TokenPrice,
}

impl CurrencyPricing {
    /// 全 0：还没定价时的显式值。
    pub const fn zero() -> Self {
        Self {
            fixed: TokenPrice::zero(),
            peak: TokenPrice::zero(),
            off_peak: TokenPrice::zero(),
        }
    }
}

impl Default for CurrencyPricing {
    fn default() -> Self {
        Self::zero()
    }
}

/// 一个模型的价格规则。两个币种各自维护完整的价格，界面语言决定用哪一套；
/// `None` 表示这个币种还没定价——它不是"价格为零"，未定价的用量不计入金额。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ModelTokenPricing {
    pub mode: PricingMode,
    /// 人民币价格。
    pub cny: Option<CurrencyPricing>,
    /// 美元价格。
    pub usd: Option<CurrencyPricing>,
}

impl Default for ModelTokenPricing {
    /// 新建模型的初始草稿：固定单价，两个币种都还没定价。
    fn default() -> Self {
        Self {
            mode: PricingMode::Fixed,
            cny: None,
            usd: None,
        }
    }
}

impl ModelTokenPricing {
    /// 单价必须是有限、非负的数字：NaN 或负价会让首页算出负成本，
    /// 也会让序列化出的 JSON 出现 `null`。
    fn validate(&self) -> Result<()> {
        for (currency, pricing) in [("cny", self.cny), ("usd", self.usd)] {
            let Some(pricing) = pricing else { continue };
            for (slot, price) in [
                ("fixed", pricing.fixed),
                ("peak", pricing.peak),
                ("off_peak", pricing.off_peak),
            ] {
                for (field, value) in [
                    ("input_per_million", price.input_per_million),
                    ("output_per_million", price.output_per_million),
                    ("cache_read_per_million", price.cache_read_per_million),
                    ("cache_write_per_million", price.cache_write_per_million),
                ] {
                    if !value.is_finite() || value < 0.0 {
                        return Err(crate::Error::Config(format!(
                            "price {currency}.{slot}.{field} must be a finite, non-negative number"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// 首页「价值估算」的价格设置：按模型分开计价。
///
/// 键是模型的稳定标识（`model_hash`）。没有规则的模型就是未定价——不存在全局
/// 回退价，也不从模型名猜价格。
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TokenPricingSettings {
    pub models: BTreeMap<String, ModelTokenPricing>,
}

/// 出网时怎么走代理。三者互斥，且只有 `Custom` 会读下面的地址。
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    /// 跟随系统代理：reqwest 在构建客户端的那一刻读一次环境变量与系统配置
    /// （Windows 上是注册表 `Internet Settings`）。
    #[default]
    Default,
    /// 使用下面填写的代理地址。
    Custom,
    /// 不使用任何代理，直接出网。
    Direct,
}

impl ProxyMode {
    pub fn is_custom(self) -> bool {
        self == Self::Custom
    }
}

/// Cursor 的模型来源模式。三种模式的差别只落在四处：代理配置、账号策略、
/// 模型目录的来源、以及模型 ID 不认识时怎么处理。
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorMode {
    /// 完全交给 Cursor：不写代理配置、不注入账号、我们看不见它的请求。
    #[default]
    Official,
    /// 只用模型库里的模型：模型目录只有本机模型，不认识的模型 ID 明确拒绝。
    /// 客户端没有账号时补一个本地账号，好让 BYOK 在没有官方订阅时也能用。
    Local,
    /// 官方模型和本机模型同时可用：目录取官方目录再追加本机模型，请求按模型 ID
    /// 分流。要求客户端真的登录了官方账号，所以绝不注入本地账号。
    Mixed,
}

impl CursorMode {
    /// 是否需要把 Cursor 引到本机代理（也就是"接管"）。
    pub fn takes_over(self) -> bool {
        !matches!(self, Self::Official)
    }

    /// 是否需要让官方模型出现在客户端的模型列表里。
    pub fn includes_official_models(self) -> bool {
        !matches!(self, Self::Local)
    }

    /// 是否允许在客户端没有账号时注入本地账号。
    pub fn injects_local_account(self) -> bool {
        matches!(self, Self::Local)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TabMode {
    #[default]
    Public,
    Direct,
    Custom,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TabSettings {
    pub mode: TabMode,
    pub address: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct DesktopSettings {
    pub silent_start: bool,
    #[serde(default = "default_true")]
    pub show_dock_icon: bool,
    /// 开发者模式：界面显示内部标识与协议字段，日志按 debug 级别记录。
    ///
    /// 默认关。它只决定"显示多少信息"，不隐藏功能；日志过滤在启动早期由
    /// 数据目录里的 `dev-mode.json` 镜像决定（`config::write_dev_mode_mirror`）。
    pub developer_mode: bool,
    /// 界面偏好（前端的 `DesktopUiPreferences`）：字段**缺席** = 服务端还没有值（旧版本服务端，
    /// 或从没设置过）；`null` = 显式清空回默认。
    ///
    /// 这些以前只存在 webview 的 localStorage 里，而 localStorage 按 origin 隔离、origin 里带着
    /// 管理端口——端口一变（产品明说的行为）偏好就全读不到了。第 31 批裁定：服务端是唯一权威。
    /// 取值词汇表（`midnight` / `default-dark` / `system` / `zh-CN`…）由前端拥有，服务端只存不判。
    pub theme: Option<String>,
    pub locale: Option<String>,
    pub tutorial_read: Option<bool>,
    pub pending_update: Option<PendingUpdate>,
    pub devin_host_path: Option<String>,
    /// 折叠状态：卡片键 → 是否展开。见 [`DesktopSettingsPatch::disclosure`] 的合并语义。
    pub disclosure: BTreeMap<String, bool>,
}

/// 待核对的升级记录（前端的 `PendingUpdateRecord`）。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PendingUpdate {
    pub from: String,
    pub to: String,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            silent_start: false,
            show_dock_icon: true,
            developer_mode: false,
            theme: None,
            locale: None,
            tutorial_read: None,
            pending_update: None,
            devin_host_path: None,
            disclosure: BTreeMap::new(),
        }
    }
}

/// 桌面设置的一次按字段更新：只提交改动的字段。
///
/// 界面上的每个控件各发一次修订，携带的只有自己那几个字段。服务端在全局写锁内读回已保存的
/// 设置、合并这次修订再写回（并把保存后的整份设置回显给前端），所以两个控件先后保存时不会
/// 各自拿着同一个旧快照互相撤销——整对象读改写（前端先 GET 再 PUT）正是丢更新的来源。
///
/// 三态语义：字段**缺席** = 不改；`null` = 清空回默认；有值 = 设置。可清空的字段因此写成
/// `Option<Option<T>>`：serde 默认把 `null` 和缺席都解成 `None`，那样前端点"清空"会被当成
/// "不改"——正好相反。
#[derive(Clone, Debug, Default, Deserialize)]
pub struct DesktopSettingsPatch {
    #[serde(default)]
    pub silent_start: Option<bool>,
    #[serde(default)]
    pub show_dock_icon: Option<bool>,
    #[serde(default)]
    pub developer_mode: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    pub theme: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub locale: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub tutorial_read: Option<Option<bool>>,
    #[serde(default, deserialize_with = "double_option")]
    pub pending_update: Option<Option<PendingUpdate>>,
    #[serde(default, deserialize_with = "double_option")]
    pub devin_host_path: Option<Option<String>>,
    /// 折叠状态：**按卡片键合并**，不是整份替换——两个卡片先后折叠不会互相撤销。`null` 清空整份。
    #[serde(default, deserialize_with = "double_option")]
    pub disclosure: Option<Option<BTreeMap<String, bool>>>,
}

/// 区分"字段缺席"与"显式 `null`"：缺席 → `None`（不改），`null` → `Some(None)`（清空）。
fn double_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

impl DesktopSettingsPatch {
    fn apply_to(self, settings: &mut DesktopSettings) {
        if let Some(silent_start) = self.silent_start {
            settings.silent_start = silent_start;
        }
        if let Some(show_dock_icon) = self.show_dock_icon {
            settings.show_dock_icon = show_dock_icon;
        }
        if let Some(developer_mode) = self.developer_mode {
            settings.developer_mode = developer_mode;
        }
        if let Some(theme) = self.theme {
            settings.theme = theme;
        }
        if let Some(locale) = self.locale {
            settings.locale = locale;
        }
        if let Some(tutorial_read) = self.tutorial_read {
            settings.tutorial_read = tutorial_read;
        }
        if let Some(pending_update) = self.pending_update {
            settings.pending_update = pending_update;
        }
        if let Some(devin_host_path) = self.devin_host_path {
            settings.devin_host_path = devin_host_path;
        }
        if let Some(disclosure) = self.disclosure {
            match disclosure {
                Some(entries) => settings.disclosure.extend(entries),
                None => settings.disclosure.clear(),
            }
        }
    }
}

fn default_true() -> bool {
    true
}

impl TabSettings {
    pub fn service_url(&self) -> Option<&str> {
        match self.mode {
            TabMode::Public => Some(PUBLIC_TAB_SERVICE_URL),
            TabMode::Direct => None,
            TabMode::Custom => Some(&self.address),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum CommitPromptLocale {
    #[default]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en-US")]
    EnUs,
}

impl CommitPromptLocale {
    pub fn from_interface_language(value: &str) -> Self {
        if value.eq_ignore_ascii_case("zh-CN") {
            Self::ZhCn
        } else {
            Self::EnUs
        }
    }

    pub fn default_prompt(self) -> &'static str {
        match self {
            Self::ZhCn => DEFAULT_COMMIT_PROMPT_ZH_CN.trim(),
            Self::EnUs => DEFAULT_COMMIT_PROMPT_EN_US.trim(),
        }
    }
}

impl<'de> Deserialize<'de> for CommitPromptLocale {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from_interface_language(&value))
    }
}

/// User preferences for Git commit message generation.
///
/// Empty `model_id` means 直连: forward the original Cursor RPC unchanged.
/// A non-empty value is the stable identifier of a configured built-in or
/// plugin model, and the request is generated locally through that model.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct CommitSettings {
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub prompt_locale: CommitPromptLocale,
}

impl CommitSettings {
    pub fn is_direct(&self) -> bool {
        self.model_id.trim().is_empty()
    }

    pub fn effective_prompt(&self) -> &str {
        let trimmed = self.prompt.trim();
        if trimmed.is_empty() {
            self.prompt_locale.default_prompt()
        } else {
            trimmed
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ProxySettingsInput {
    pub mode: ProxyMode,
    pub address: String,
    pub auth_enabled: bool,
    pub username: String,
    pub password: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProxySettings {
    pub mode: ProxyMode,
    pub address: String,
    pub auth_enabled: bool,
    pub username: String,
    pub has_password: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ProxySettingsSecret {
    pub mode: ProxyMode,
    pub address: String,
    pub auth_enabled: bool,
    pub username: String,
    pub password: String,
}

/// Reads the persisted outbound proxy row, falling back to "no proxy" when it
/// no longer parses.
///
/// `mode` is a closed enum whose stored wire value has already changed once, so
/// a row written by an older build can be unreadable by this one. Propagating
/// that error would be unrecoverable rather than merely noisy: every outbound
/// client is built from this value, and `set_proxy_settings` reads the row
/// before it writes, so the settings page could neither load nor replace the
/// row that broke it.
fn read_proxy_settings(value: &str) -> ProxySettingsSecret {
    serde_json::from_str(value).unwrap_or_else(|error| {
        tracing::warn!(%error, "ignoring unreadable outbound proxy settings");
        ProxySettingsSecret::default()
    })
}

/// Devin 设置的解析缓存。
///
/// 网关每个请求都要读一次设置（[`Store::devin_settings`]），而解析与校验
/// （`serde_json::from_str` + `validate`）只需要在设置真的变了之后重做一次。缓存按**版本号**
/// 判定：`set_devin_settings` 写成功后 `+1`，读侧先取版本、命中就直接返回已解析的值。
///
/// 为什么只靠进程内版本号就够：`service_settings` 只有进程内的 `Store` 会写（没有绕过它的
/// 写路径），而实例锁保证同一份数据目录同时只有一个进程在写（`store::runs` 的同一前提）。
///
/// 读侧的顺序很要紧：**先取版本号再查库**。并发写入只会让版本号变得更新，于是这次读到的东西
/// 会被标在一个更旧的版本上、下次必然不命中（最多多解析一次，绝不会给出旧设置）；反过来先查库
/// 再取版本号，就可能把旧内容标在新版本上，缓存从此永远返回旧设置。
#[derive(Default)]
pub(crate) struct DevinSettingsCache {
    version: AtomicU64,
    cached: Mutex<Option<(u64, crate::devin::DevinSettings)>>,
    /// 真正解析过多少次；用例用它钉住"连续两次读取只解析一次"。
    parses: AtomicU64,
}

impl DevinSettingsCache {
    fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Release);
    }

    fn hit(&self, version: u64) -> Option<crate::devin::DevinSettings> {
        let cached = self
            .cached
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cached
            .as_ref()
            .filter(|(cached_version, _)| *cached_version == version)
            .map(|(_, settings)| settings.clone())
    }

    fn store(&self, version: u64, settings: crate::devin::DevinSettings) {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *cached = Some((version, settings));
    }

    fn count_parse(&self) {
        self.parses.fetch_add(1, Ordering::Relaxed);
    }
}

impl Store {
    pub async fn devin_settings(&self) -> Result<crate::devin::DevinSettings> {
        let version = self.devin_settings_cache.version();
        if let Some(settings) = self.devin_settings_cache.hit(version) {
            return Ok(settings);
        }
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(DEVIN_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        let settings = match value {
            Some(value) => {
                self.devin_settings_cache.count_parse();
                serde_json::from_str::<crate::devin::DevinSettings>(&value)
                    .map_err(crate::Error::Json)?
            }
            None => crate::devin::DevinSettings::default(),
        };
        settings.validate()?;
        self.devin_settings_cache.store(version, settings.clone());
        Ok(settings)
    }

    pub async fn set_devin_settings(
        &self,
        settings: crate::devin::DevinSettings,
    ) -> Result<crate::devin::DevinSettings> {
        settings.validate()?;
        let value_json = serde_json::to_string(&settings)?;
        let _write = self.writes.lock().await;
        sqlx::query("INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms")
            .bind(DEVIN_SETTINGS_KEY)
            .bind(value_json)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        // 写进去之后才 +1：先加会让并发读把"还没落库的旧行"标在新版本上。
        self.devin_settings_cache.bump();
        Ok(settings)
    }

    /// 当前选定的 Cursor 模型来源模式。
    ///
    /// 缺省是"官方"：接管会改写用户的 Cursor 配置并强制结束正在运行的编辑器，
    /// 只能是用户明确选过的结果——全新安装不该自己接管，在同机上还有另一个同类
    /// 软件驱动 Cursor 时更不该由这个应用替用户决定。
    pub(crate) async fn cursor_mode(&self) -> Result<CursorMode> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(CURSOR_MODE_KEY)
        .fetch_optional(&self.pool)
        .await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or(Ok(CursorMode::Official))
    }

    pub(crate) async fn set_cursor_mode(&self, mode: CursorMode) -> Result<()> {
        let value_json = serde_json::to_string(&mode)?;
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(CURSOR_MODE_KEY)
        .bind(value_json)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn proxy_settings_secret(&self) -> Result<ProxySettingsSecret> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(PROXY_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        Ok(value
            .as_deref()
            .map_or_else(ProxySettingsSecret::default, read_proxy_settings))
    }

    pub async fn proxy_settings(&self) -> Result<ProxySettings> {
        let settings = self.proxy_settings_secret().await?;
        Ok(ProxySettings {
            mode: settings.mode,
            address: settings.address,
            auth_enabled: settings.auth_enabled,
            username: settings.username,
            has_password: !settings.password.is_empty(),
        })
    }

    pub async fn set_proxy_settings(&self, input: ProxySettingsInput) -> Result<ProxySettings> {
        let existing = self.proxy_settings_secret().await?;
        let address = input.address.trim().to_owned();
        if input.mode.is_custom() {
            let parsed = url::Url::parse(&address)
                .map_err(|error| crate::Error::Config(format!("invalid proxy address: {error}")))?;
            if !matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h") {
                return Err(crate::Error::Config(
                    "proxy address must use http, https, socks5, or socks5h".into(),
                ));
            }
            reqwest::Proxy::all(&address)?;
        }
        let password = if input.auth_enabled {
            input
                .password
                .filter(|password| !password.is_empty())
                .unwrap_or(existing.password)
        } else {
            String::new()
        };
        let settings = ProxySettingsSecret {
            mode: input.mode,
            address,
            auth_enabled: input.auth_enabled,
            username: input.username.trim().to_owned(),
            password,
        };
        let value_json = serde_json::to_string(&settings)?;
        let _write = self.writes.lock().await;
        sqlx::query("INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms")
            .bind(PROXY_SETTINGS_KEY)
            .bind(value_json)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        self.proxy_settings().await
    }

    pub async fn tab_settings(&self) -> Result<TabSettings> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(TAB_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(TabSettings::default()))
    }

    pub async fn set_tab_settings(&self, mut settings: TabSettings) -> Result<TabSettings> {
        settings.address = settings.address.trim().trim_end_matches('/').to_owned();
        if settings.mode == TabMode::Custom {
            let parsed = url::Url::parse(&settings.address).map_err(|error| {
                crate::Error::Config(format!("invalid TAB service address: {error}"))
            })?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(crate::Error::Config(
                    "TAB service address must use http or https".into(),
                ));
            }
            if parsed.host_str().is_none()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                return Err(crate::Error::Config(
                    "TAB service address must be a base URL without a query or fragment".into(),
                ));
            }
        }
        let value_json = serde_json::to_string(&settings)?;
        let _write = self.writes.lock().await;
        sqlx::query("INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms")
            .bind(TAB_SETTINGS_KEY)
            .bind(value_json)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        Ok(settings)
    }

    pub async fn port_settings(&self) -> Result<PortSettings> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(PORT_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(PortSettings::default()))
    }

    pub async fn set_port_settings(&self, settings: PortSettings) -> Result<()> {
        let _write = self.writes.lock().await;
        self.write_port_settings(&settings).await
    }

    /// 只改服务端口，保留代理端口。
    ///
    /// 快路径（值没变）不碰写锁：启动早期写实际端口时可能正赶上后台回收，那一段
    /// 长时间持锁，而这一步不该排在它后面。真的需要写时，读回、合并、写回都在同一
    /// 把写锁内完成（与桌面设置、按模型价格同一套路）：这一行有两个写入方——启动时
    /// 写实际服务端口、接管启动代理时写实际代理端口——各自只改自己那一个字段，谁都
    /// 不该把对方刚写进去的值抹掉，也不该撤销用户在设置页刚保存的那份。
    pub async fn set_service_port(&self, port: u16) -> Result<()> {
        if self.port_settings().await?.service_port == port {
            return Ok(());
        }
        let _write = self.writes.lock().await;
        let mut settings = self.port_settings().await?;
        if settings.service_port == port {
            return Ok(());
        }
        settings.service_port = port;
        self.write_port_settings(&settings).await
    }

    /// 只改代理端口，保留服务端口。理由同 `set_service_port`。
    pub async fn set_proxy_port(&self, port: u16) -> Result<()> {
        // Starting on the saved port must not wait behind background collection.
        if self.port_settings().await?.proxy_port == port {
            return Ok(());
        }
        let _write = self.writes.lock().await;
        let mut settings = self.port_settings().await?;
        if settings.proxy_port == port {
            return Ok(());
        }
        settings.proxy_port = port;
        self.write_port_settings(&settings).await
    }

    /// 写入整行端口设置；调用方必须已经持有写锁。
    async fn write_port_settings(&self, settings: &PortSettings) -> Result<()> {
        let value_json = serde_json::to_string(settings)?;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(PORT_SETTINGS_KEY)
        .bind(value_json)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn desktop_settings(&self) -> Result<DesktopSettings> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(DESKTOP_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(DesktopSettings::default()))
    }

    /// 按字段更新桌面设置，返回合并后的完整设置。
    ///
    /// 读回、合并、写回都在同一把写锁内完成：两个开关同时保存时，第二个进入锁时
    /// 读到的已经是第一个写好的设置，不会把对方的字段覆盖掉。
    pub async fn update_desktop_settings(
        &self,
        patch: DesktopSettingsPatch,
    ) -> Result<DesktopSettings> {
        let settings = {
            let _write = self.writes.lock().await;
            let mut settings = self.desktop_settings().await?;
            patch.apply_to(&mut settings);
            let value_json = serde_json::to_string(&settings)?;
            sqlx::query(
                "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
            )
            .bind(DESKTOP_SETTINGS_KEY)
            .bind(value_json)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
            settings
        };
        // 日志过滤在下一次启动的数据库之前初始化，只能靠数据目录里的镜像读这个开关。
        // 镜像写不进去时设置仍然生效（界面按数据库读），所以只记一条警告。
        //
        // 写在锁外：这是同步文件 I/O，持写锁做会让所有写入方排队等一次磁盘写（写锁是整个
        // 进程唯一的写入通道，包括每个工具轮的落库）。
        if let Err(error) = crate::config::write_dev_mode_mirror(settings.developer_mode) {
            tracing::warn!(%error, "failed to mirror the developer mode setting");
        }
        Ok(settings)
    }

    pub async fn commit_settings(&self) -> Result<CommitSettings> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(COMMIT_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(CommitSettings::default()))
    }

    pub async fn set_commit_settings(&self, settings: CommitSettings) -> Result<CommitSettings> {
        let settings = CommitSettings {
            model_id: settings.model_id.trim().to_owned(),
            prompt: settings.prompt.trim().to_owned(),
            prompt_locale: settings.prompt_locale,
        };
        let value_json = serde_json::to_string(&settings)?;
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(COMMIT_SETTINGS_KEY)
        .bind(value_json)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(settings)
    }

    /// 已定价模型的规则表。缺失、空对象、或旧版残留都读成空表。
    ///
    /// 读不出来时按空表处理：这个结构里有闭合枚举（`mode`）与可空币种，旧版本或
    /// 外部工具写下的行可能读不回。把错误抛出去会让保存与删除也一起失败——它们都
    /// 先读这一行——用户就只能手工改库。与 `read_proxy_settings` 同样的取舍。
    pub async fn pricing_settings(&self) -> Result<TokenPricingSettings> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(MODEL_PRICING_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        Ok(value
            .map(|value| read_model_token_pricing(&value))
            .unwrap_or_default())
    }

    /// 保存（或覆盖）一个模型的价格规则，返回合并后的完整规则表。
    ///
    /// 读回、合并、写回都在同一把写锁内完成：两个模型各自保存时，第二个进入锁时
    /// 读到的已经是第一个写好的表，不会把对方刚保存的规则撤掉。保存成功同时清掉
    /// 旧版的全局价格键——"一套价给所有模型"的语义已被按模型规则取代。
    pub async fn set_model_pricing(
        &self,
        model_hash: &str,
        pricing: ModelTokenPricing,
    ) -> Result<TokenPricingSettings> {
        validate_model_hash(model_hash)?;
        pricing.validate()?;
        let _write = self.writes.lock().await;
        let mut settings = self.pricing_settings().await?;
        settings.models.insert(model_hash.to_owned(), pricing);
        let value_json = serde_json::to_string(&settings)?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(MODEL_PRICING_SETTINGS_KEY)
        .bind(value_json)
        .bind(now_ms())
        .execute(&mut *transaction)
        .await?;
        sqlx::query("DELETE FROM service_settings WHERE setting_key = ?")
            .bind(LEGACY_PRICING_SETTINGS_KEY)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(settings)
    }

    /// 删除一个模型的价格规则，返回合并后的完整规则表。
    ///
    /// 删除不存在的规则是空操作：不报错，也不写库。删除同样在写锁内读-改-写，
    /// 与保存一样不会覆盖另一个模型的规则。
    pub async fn remove_model_pricing(&self, model_hash: &str) -> Result<TokenPricingSettings> {
        validate_model_hash(model_hash)?;
        let _write = self.writes.lock().await;
        let mut settings = self.pricing_settings().await?;
        if settings.models.remove(model_hash).is_none() {
            return Ok(settings);
        }
        let value_json = serde_json::to_string(&settings)?;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(MODEL_PRICING_SETTINGS_KEY)
        .bind(value_json)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(settings)
    }
}

/// 读回按模型价格表；读不动就当成空表，让下一次保存能把坏行覆盖掉。
fn read_model_token_pricing(value: &str) -> TokenPricingSettings {
    serde_json::from_str(value).unwrap_or_else(|error| {
        tracing::warn!(%error, "ignoring unreadable model token pricing settings");
        TokenPricingSettings::default()
    })
}

/// 价格规则挂在稳定模型标识上，标识本身必须是干净的键：非空、无空白、无控制字符。
/// 带空白的哈希几乎总是调用方拼错了，静默 trim 会造出第二个看不见的键。
fn validate_model_hash(model_hash: &str) -> Result<()> {
    if model_hash.is_empty()
        || model_hash
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(crate::Error::Config(
            "model hash must be a non-empty identifier without whitespace".into(),
        ));
    }
    Ok(())
}

/// 把旧版的"是否接管"布尔换算成模式，然后删掉旧键。
///
/// 只在还没有 `cursor_mode` 的库上留下结果：接管过 → 本地（保住"没有官方订阅也
/// 能用"的能力），没接管 → 官方。旧键无论如何都删掉——它的语义已经被模式取代，
/// 留着只会让后来的人以为还有第二处真相。
pub(crate) async fn migrate_cursor_mode(pool: &sqlx::SqlitePool) -> Result<()> {
    let legacy = sqlx::query_scalar::<_, String>(
        "SELECT value_json FROM service_settings WHERE setting_key = ?",
    )
    .bind(LEGACY_CURSOR_TAKEOVER_KEY)
    .fetch_optional(pool)
    .await?;
    let Some(legacy) = legacy else {
        return Ok(());
    };
    let mode = if serde_json::from_str::<bool>(&legacy).unwrap_or(false) {
        CursorMode::Local
    } else {
        CursorMode::Official
    };
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO NOTHING",
    )
    .bind(CURSOR_MODE_KEY)
    .bind(serde_json::to_string(&mode)?)
    .bind(now_ms())
    .execute(&mut *transaction)
    .await?;
    sqlx::query("DELETE FROM service_settings WHERE setting_key = ?")
        .bind(LEGACY_CURSOR_TAKEOVER_KEY)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        migrate_cursor_mode, read_proxy_settings, CommitPromptLocale, CommitSettings,
        CurrencyPricing, CursorMode, DesktopSettingsPatch, ModelTokenPricing, PendingUpdate,
        PricingMode, ProxyMode, ProxySettingsInput, ProxySettingsSecret, Store, TokenPrice,
        TokenPricingSettings, DEFAULT_COMMIT_PROMPT_EN_US, DEFAULT_COMMIT_PROMPT_ZH_CN,
        LEGACY_CURSOR_TAKEOVER_KEY, MODEL_PRICING_SETTINGS_KEY, PROXY_SETTINGS_KEY,
    };
    use crate::devin::{DevinModelBinding, DevinSettings};
    use crate::store::now_ms;

    async fn temp_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        (directory, store)
    }

    /// 旧版写进 `token_pricing` 的整套全局价，一字不改地放回库里。
    async fn insert_legacy_global_row(store: &Store) {
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('token_pricing', ?, 0)",
        )
        .bind(
            r#"{"mode":"peak_off_peak","cny":{"fixed":{"input_per_million":2.0,"output_per_million":8.0,"cache_read_per_million":0.04,"cache_write_per_million":0.0},"peak":{"input_per_million":2.0,"output_per_million":8.0,"cache_read_per_million":0.04,"cache_write_per_million":0.0},"off_peak":{"input_per_million":1.0,"output_per_million":4.0,"cache_read_per_million":0.02,"cache_write_per_million":0.0}},"usd":{"fixed":{"input_per_million":0.3,"output_per_million":1.2,"cache_read_per_million":0.006,"cache_write_per_million":0.0},"peak":{"input_per_million":0.3,"output_per_million":1.2,"cache_read_per_million":0.006,"cache_write_per_million":0.0},"off_peak":{"input_per_million":0.15,"output_per_million":0.6,"cache_read_per_million":0.003,"cache_write_per_million":0.0}}}"#,
        )
        .execute(store.pool())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn unchanged_proxy_port_does_not_wait_for_storage_maintenance() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        store.set_proxy_port(9061).await.unwrap();
        let maintenance = store.writes.lock().await;
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            store.set_proxy_port(9061),
        )
        .await;
        drop(maintenance);
        assert!(
            result.is_ok(),
            "an unchanged port must not acquire the write lock"
        );
        result.unwrap().unwrap();
        store.set_proxy_port(9062).await.unwrap();
        assert_eq!(store.port_settings().await.unwrap().proxy_port, 9062);
    }

    /// 服务端有两个写入方各改一个字段（启动写服务端口、接管启动代理写代理端口）：
    /// 谁都不许把对方那半行抹掉。值没变的那条路径同样不碰写锁。
    #[tokio::test]
    async fn partial_port_updates_keep_the_other_field_and_skip_the_write_lock_when_unchanged() {
        let (_directory, store) = temp_store().await;
        store
            .set_port_settings(super::PortSettings {
                proxy_port: 41_000,
                service_port: 56_321,
            })
            .await
            .unwrap();

        // 值没变：不等待写锁（后台回收可能长时间持锁）。
        let maintenance = store.writes.lock().await;
        let unchanged = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            store.set_service_port(56321),
        )
        .await;
        drop(maintenance);
        assert!(
            unchanged.is_ok(),
            "an unchanged port must not take the lock"
        );
        unchanged.unwrap().unwrap();

        store.set_service_port(56_400).await.unwrap();
        store.set_proxy_port(41_100).await.unwrap();
        assert_eq!(
            store.port_settings().await.unwrap(),
            super::PortSettings {
                proxy_port: 41_100,
                service_port: 56_400,
            }
        );
    }

    /// The `outbound_proxy` row exactly as builds before the `system` -> `default`
    /// rename wrote it.
    const LEGACY_PROXY_ROW: &str =
        r#"{"mode":"system","address":"","auth_enabled":false,"username":"","password":""}"#;

    #[test]
    fn commit_prompt_locale_maps_unknown_interface_languages_to_english() {
        assert_eq!(
            serde_json::from_str::<CommitPromptLocale>(r#""zh-CN""#).unwrap(),
            CommitPromptLocale::ZhCn
        );
        assert_eq!(
            serde_json::from_str::<CommitPromptLocale>(r#""en-US""#).unwrap(),
            CommitPromptLocale::EnUs
        );
        assert_eq!(
            serde_json::from_str::<CommitPromptLocale>(r#""ja-JP""#).unwrap(),
            CommitPromptLocale::EnUs
        );
        assert_eq!(
            serde_json::to_string(&CommitPromptLocale::EnUs).unwrap(),
            r#""en-US""#
        );
    }

    #[test]
    fn default_commit_prompt_follows_its_saved_locale() {
        for (prompt_locale, expected) in [
            (CommitPromptLocale::ZhCn, DEFAULT_COMMIT_PROMPT_ZH_CN),
            (CommitPromptLocale::EnUs, DEFAULT_COMMIT_PROMPT_EN_US),
        ] {
            let settings = CommitSettings {
                prompt_locale,
                ..CommitSettings::default()
            };
            assert_eq!(settings.effective_prompt(), expected.trim());
        }
    }

    #[test]
    fn custom_commit_prompt_does_not_change_with_locale() {
        for prompt_locale in [CommitPromptLocale::ZhCn, CommitPromptLocale::EnUs] {
            let settings = CommitSettings {
                prompt: "custom prompt".into(),
                prompt_locale,
                ..CommitSettings::default()
            };
            assert_eq!(settings.effective_prompt(), "custom prompt");
        }
    }

    #[test]
    fn default_proxy_mode_uses_the_default_wire_value() {
        assert_eq!(ProxyMode::default(), ProxyMode::Default);
        assert_eq!(
            serde_json::to_string(&ProxyMode::default()).unwrap(),
            "\"default\""
        );
        assert_eq!(
            serde_json::from_str::<ProxyMode>("\"default\"").unwrap(),
            ProxyMode::Default
        );
        assert!(serde_json::from_str::<ProxyMode>("\"system\"").is_err());
    }

    /// 直连是一个独立模式：既不读系统代理，也不读填写的地址。
    #[test]
    fn direct_proxy_mode_round_trips_and_is_not_custom() {
        assert_eq!(
            serde_json::to_string(&ProxyMode::Direct).unwrap(),
            "\"direct\""
        );
        assert_eq!(
            serde_json::from_str::<ProxyMode>("\"direct\"").unwrap(),
            ProxyMode::Direct
        );
        assert!(!ProxyMode::Direct.is_custom());
    }

    /// 老版本只认 `default` 与 `custom`；库里出现 `direct` 时它读不出来，于是
    /// 退回默认值。这条锁住"读不出来也不许报错"：出网客户端全部由这个值构建。
    #[test]
    fn a_row_written_by_a_newer_build_reads_as_the_default() {
        let settings = read_proxy_settings(r#"{"mode":"direct","address":""}"#);

        assert_eq!(settings.mode, ProxyMode::Default);
    }

    #[test]
    fn an_unreadable_proxy_row_reads_as_no_proxy() {
        assert!(serde_json::from_str::<ProxySettingsSecret>(LEGACY_PROXY_ROW).is_err());

        let settings = read_proxy_settings(LEGACY_PROXY_ROW);
        assert_eq!(settings.mode, ProxyMode::Default);
        assert!(settings.address.is_empty());
        assert!(!settings.auth_enabled);
    }

    #[tokio::test]
    async fn a_database_without_a_choice_stays_in_official_mode() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();

        // A database that has never recorded a choice must not take Cursor over:
        // doing so rewrites the user's Cursor configuration and force-terminates
        // their editor.
        assert_eq!(store.cursor_mode().await.unwrap(), CursorMode::Official);

        store.set_cursor_mode(CursorMode::Mixed).await.unwrap();
        assert_eq!(store.cursor_mode().await.unwrap(), CursorMode::Mixed);

        store.set_cursor_mode(CursorMode::Official).await.unwrap();
        assert_eq!(store.cursor_mode().await.unwrap(), CursorMode::Official);
    }

    /// 旧版的接管布尔换算成模式：接管过 → 本地（保住"没有官方订阅也能用"），
    /// 没接管 → 官方；旧键在任何情况下都不留下。
    #[tokio::test]
    async fn the_legacy_takeover_boolean_becomes_a_mode_once() {
        for (legacy, expected) in [("true", CursorMode::Local), ("false", CursorMode::Official)] {
            let directory = tempfile::tempdir().unwrap();
            let url = format!("sqlite://{}", directory.path().join("test.db").display());
            let store = Store::connect(&url).await.unwrap();
            sqlx::query(
                "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, 0)",
            )
            .bind(LEGACY_CURSOR_TAKEOVER_KEY)
            .bind(legacy)
            .execute(store.pool())
            .await
            .unwrap();

            migrate_cursor_mode(store.pool()).await.unwrap();

            assert_eq!(store.cursor_mode().await.unwrap(), expected);
            let leftover: Option<String> =
                sqlx::query_scalar("SELECT value_json FROM service_settings WHERE setting_key = ?")
                    .bind(LEGACY_CURSOR_TAKEOVER_KEY)
                    .fetch_optional(store.pool())
                    .await
                    .unwrap();
            assert!(leftover.is_none(), "the legacy key must not survive");
        }
    }

    /// 已经记过模式的库不被旧键改写，旧键照样清掉。
    #[tokio::test]
    async fn an_existing_mode_wins_over_the_legacy_boolean() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, 0)",
        )
        .bind(LEGACY_CURSOR_TAKEOVER_KEY)
        .bind("true")
        .execute(store.pool())
        .await
        .unwrap();
        store.set_cursor_mode(CursorMode::Official).await.unwrap();

        migrate_cursor_mode(store.pool()).await.unwrap();

        assert_eq!(store.cursor_mode().await.unwrap(), CursorMode::Official);
    }

    #[tokio::test]
    async fn a_proxy_row_from_an_older_build_stays_replaceable() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, 0)",
        )
        .bind(PROXY_SETTINGS_KEY)
        .bind(LEGACY_PROXY_ROW)
        .execute(store.pool())
        .await
        .unwrap();

        // Reading must not fail: every outbound client is built from this value.
        assert_eq!(
            store.proxy_settings().await.unwrap().mode,
            ProxyMode::Default
        );

        // And the settings page must be able to overwrite the row that broke it.
        let saved = store
            .set_proxy_settings(ProxySettingsInput {
                mode: ProxyMode::Custom,
                address: "http://127.0.0.1:7890".into(),
                auth_enabled: false,
                username: String::new(),
                password: None,
            })
            .await
            .unwrap();
        assert_eq!(saved.mode, ProxyMode::Custom);
        assert_eq!(saved.address, "http://127.0.0.1:7890");
    }

    /// 桌面设置按字段保存：一个控件保存自己的字段时，不能撤销另一个控件刚保存的字段。
    #[tokio::test]
    async fn desktop_settings_update_keeps_the_fields_it_did_not_touch() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();

        store
            .update_desktop_settings(DesktopSettingsPatch {
                silent_start: Some(true),
                ..DesktopSettingsPatch::default()
            })
            .await
            .unwrap();
        let saved = store
            .update_desktop_settings(DesktopSettingsPatch {
                developer_mode: Some(true),
                ..DesktopSettingsPatch::default()
            })
            .await
            .unwrap();

        assert!(saved.silent_start, "the earlier save must survive");
        assert!(saved.developer_mode);
        assert!(
            saved.show_dock_icon,
            "a field nobody wrote keeps its default"
        );
        assert_eq!(store.desktop_settings().await.unwrap(), saved);
    }

    /// 两次按字段更新同时落库时各自生效：读取-合并-写回必须在同一把写锁内完成，
    /// 否则两边都拿着旧的整份设置，后写的那个会把对方的字段撤回去。
    #[tokio::test]
    async fn concurrent_desktop_settings_updates_keep_every_field() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();

        let (silent_start, show_dock_icon) = tokio::join!(
            store.update_desktop_settings(DesktopSettingsPatch {
                silent_start: Some(true),
                ..DesktopSettingsPatch::default()
            }),
            store.update_desktop_settings(DesktopSettingsPatch {
                show_dock_icon: Some(false),
                ..DesktopSettingsPatch::default()
            }),
        );
        silent_start.unwrap();
        show_dock_icon.unwrap();

        let settings = store.desktop_settings().await.unwrap();
        assert!(settings.silent_start);
        assert!(!settings.show_dock_icon);
    }

    /// 三个开关与界面偏好一起按字段保存：改一个不能撤销别的，PUT 回显整份设置。
    ///
    /// 前端只有看到回显才删旧的 localStorage 键（"回显确认"），所以 PUT 的返回值必须是
    /// 保存后的完整设置——这条用例同时钉住"部分更新保留其余字段"和"回显完整"。
    #[tokio::test]
    async fn desktop_settings_round_trip_and_partial_updates_keep_the_other_fields() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();

        let saved = store
            .update_desktop_settings(patch(
                r#"{"theme":"midnight","locale":"zh-CN","tutorial_read":true,
                    "pending_update":{"from":"1.0.0","to":"1.1.0"},
                    "devin_host_path":"/usr/local/bin/devin",
                    "disclosure":{"models":true,"storage":false}}"#,
            ))
            .await
            .unwrap();
        assert_eq!(saved.theme.as_deref(), Some("midnight"));
        assert_eq!(saved.locale.as_deref(), Some("zh-CN"));
        assert_eq!(saved.tutorial_read, Some(true));
        assert_eq!(
            saved.pending_update,
            Some(PendingUpdate {
                from: "1.0.0".into(),
                to: "1.1.0".into()
            })
        );
        assert_eq!(
            saved.devin_host_path.as_deref(),
            Some("/usr/local/bin/devin")
        );
        assert_eq!(saved.disclosure.get("models"), Some(&true));
        assert_eq!(
            store.desktop_settings().await.unwrap(),
            saved,
            "回显 = 落库的样子"
        );

        // 只改一个开关与一个偏好字段：其余原样保留。
        let later = store
            .update_desktop_settings(patch(r#"{"silent_start":true,"locale":"en-US"}"#))
            .await
            .unwrap();
        assert!(later.silent_start);
        assert_eq!(later.locale.as_deref(), Some("en-US"));
        assert_eq!(later.theme.as_deref(), Some("midnight"), "别的字段保持原值");
        assert_eq!(later.tutorial_read, Some(true));
        assert_eq!(later.disclosure.get("storage"), Some(&false));
        assert_eq!(store.desktop_settings().await.unwrap(), later);
    }

    /// `null` = 清空回默认，缺席 = 不改；两者必须分得开。
    ///
    /// serde 默认把 `null` 和缺席都解成 `None`，那样前端点"清空"会被当成"不改"——正好相反。
    #[tokio::test]
    async fn a_null_field_is_cleared_and_an_absent_field_is_left_alone() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        store
            .update_desktop_settings(patch(
                r#"{"theme":"midnight","locale":"zh-CN","tutorial_read":true}"#,
            ))
            .await
            .unwrap();

        let cleared = store
            .update_desktop_settings(patch(r#"{"theme":null}"#))
            .await
            .unwrap();

        assert_eq!(cleared.theme, None, "null 清空");
        assert_eq!(cleared.locale.as_deref(), Some("zh-CN"), "缺席不改");
        assert_eq!(cleared.tutorial_read, Some(true), "缺席不改");
    }

    /// `disclosure` 按卡片键合并：两个卡片先后折叠不会互相撤销；`null` 才是清空整份。
    #[tokio::test]
    async fn disclosure_merges_per_card_key_and_null_clears_everything() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();

        store
            .update_desktop_settings(patch(r#"{"disclosure":{"models":true}}"#))
            .await
            .unwrap();
        let merged = store
            .update_desktop_settings(patch(r#"{"disclosure":{"storage":false}}"#))
            .await
            .unwrap();
        assert_eq!(
            merged.disclosure.get("models"),
            Some(&true),
            "合并保留别的卡片"
        );
        assert_eq!(merged.disclosure.get("storage"), Some(&false));

        let overridden = store
            .update_desktop_settings(patch(r#"{"disclosure":{"models":false}}"#))
            .await
            .unwrap();
        assert_eq!(overridden.disclosure.get("models"), Some(&false));
        assert_eq!(overridden.disclosure.get("storage"), Some(&false));

        let cleared = store
            .update_desktop_settings(patch(r#"{"disclosure":null}"#))
            .await
            .unwrap();
        assert!(cleared.disclosure.is_empty(), "null 清空整份折叠状态");
    }

    /// 旧库（这一版之前写下的设置 JSON）读出来必须是"还没有值"，而不是解析失败。
    ///
    /// 设置是一行 JSON（`service_settings.value_json`），新增字段靠 `#[serde(default)]` 补齐：
    /// 旧行里没有 `theme` 等键 → `None`（前端据此判定"服务端没有值、用本地默认"），
    /// `disclosure` → 空表。所以这一版不需要迁移，但必须有用例证明旧行仍可读。
    #[tokio::test]
    async fn an_old_settings_row_reads_absent_ui_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();

        // 这一版之前落库的形状：只有三个开关。
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms)
             VALUES (?, ?, ?)",
        )
        .bind("desktop_lifecycle")
        .bind(r#"{"silent_start":true,"show_dock_icon":false,"developer_mode":true}"#)
        .bind(now_ms())
        .execute(store.pool())
        .await
        .unwrap();

        let settings = store.desktop_settings().await.unwrap();
        assert!(settings.silent_start && settings.developer_mode);
        assert!(!settings.show_dock_icon);
        assert_eq!(settings.theme, None);
        assert_eq!(settings.locale, None);
        assert_eq!(settings.tutorial_read, None);
        assert_eq!(settings.pending_update, None);
        assert_eq!(settings.devin_host_path, None);
        assert!(settings.disclosure.is_empty());

        // 旧行被改一个开关后写回去，界面偏好仍然是"没有值"。
        let saved = store
            .update_desktop_settings(patch(r#"{"developer_mode":false}"#))
            .await
            .unwrap();
        assert!(!saved.developer_mode && saved.silent_start);
        assert_eq!(saved.theme, None);
        assert!(saved.disclosure.is_empty());
    }

    /// 两个控件同时保存时各自生效：界面偏好也在同一把写锁里读-改-写。
    #[tokio::test]
    async fn concurrent_ui_preference_updates_keep_every_field() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();

        let (theme, locale, disclosure) = tokio::join!(
            store.update_desktop_settings(patch(r#"{"theme":"midnight"}"#)),
            store.update_desktop_settings(patch(r#"{"locale":"en-US"}"#)),
            store.update_desktop_settings(patch(r#"{"disclosure":{"models":true}}"#)),
        );
        theme.unwrap();
        locale.unwrap();
        disclosure.unwrap();

        let settings = store.desktop_settings().await.unwrap();
        assert_eq!(settings.theme.as_deref(), Some("midnight"));
        assert_eq!(settings.locale.as_deref(), Some("en-US"));
        assert_eq!(settings.disclosure.get("models"), Some(&true));
    }

    /// 把一段 PUT 正文解成修订：用例走真实的 serde 形状（缺席 / `null` / 有值）。
    fn patch(json: &str) -> DesktopSettingsPatch {
        serde_json::from_str(json).unwrap()
    }

    /// 一条只写了人民币固定单价的规则；美元没有配置，是未定价而不是 0 元。
    fn fixed_rule(input_per_million: f64) -> ModelTokenPricing {
        ModelTokenPricing {
            mode: PricingMode::Fixed,
            cny: Some(CurrencyPricing {
                fixed: TokenPrice {
                    input_per_million,
                    output_per_million: input_per_million * 4.0,
                    cache_read_per_million: 0.0,
                    cache_write_per_million: 0.0,
                },
                ..CurrencyPricing::zero()
            }),
            ..ModelTokenPricing::default()
        }
    }

    /// 测试里的规则只配了人民币，取价时直接拿这一套。
    fn cny(pricing: &ModelTokenPricing) -> CurrencyPricing {
        pricing.cny.expect("cny rule")
    }

    /// 从没存过规则的库不给任何模型定价：不存在全局价，也不预填厂商价。
    #[tokio::test]
    async fn a_database_without_rules_prices_no_model() {
        let (_directory, store) = temp_store().await;

        assert!(store.pricing_settings().await.unwrap().models.is_empty());
    }

    /// 保存一个模型的价格不会动到别的模型，读回逐字段一致。
    #[tokio::test]
    async fn saving_a_rule_for_a_second_model_keeps_the_first() {
        let (_directory, store) = temp_store().await;

        store
            .set_model_pricing("aaaa", fixed_rule(2.0))
            .await
            .unwrap();
        let saved = store
            .set_model_pricing("bbbb", fixed_rule(4.0))
            .await
            .unwrap();

        assert_eq!(saved.models.len(), 2);
        let first = &saved.models["aaaa"];
        assert_eq!(first.mode, PricingMode::Fixed);
        assert_eq!(cny(first).fixed.input_per_million, 2.0);
        assert_eq!(cny(first).fixed.output_per_million, 8.0);
        assert_eq!(cny(first).peak.input_per_million, 0.0);
        assert!(first.usd.is_none(), "没配的币种保持未定价");
        assert_eq!(cny(&saved.models["bbbb"]).fixed.input_per_million, 4.0);
        assert_eq!(store.pricing_settings().await.unwrap(), saved);
    }

    /// 两个模型各自保存时都要留下。用一条 `BEGIN IMMEDIATE` 事务把 SQLite 的写锁
    /// 攥在手里，逼出"两个保存都读到旧表、又都卡在写入口"的窗口：读回-合并-写回
    /// 不在同一把写锁内完成时，后写的那个会把对方刚存的规则撤掉。
    #[tokio::test]
    async fn concurrent_saves_of_two_models_both_survive() {
        let (_directory, store) = temp_store().await;
        let blocker = store.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();

        let first = tokio::spawn({
            let store = store.clone();
            async move { store.set_model_pricing("aaaa", fixed_rule(2.0)).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let second = tokio::spawn({
            let store = store.clone();
            async move { store.set_model_pricing("bbbb", fixed_rule(4.0)).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        blocker.rollback().await.unwrap();

        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        let saved = store.pricing_settings().await.unwrap();
        assert_eq!(saved.models.len(), 2);
        assert_eq!(cny(&saved.models["aaaa"]).fixed.input_per_million, 2.0);
        assert_eq!(cny(&saved.models["bbbb"]).fixed.input_per_million, 4.0);
    }

    /// 删掉一个模型的价格，别的模型的价格还在。
    #[tokio::test]
    async fn deleting_one_models_rule_keeps_the_other() {
        let (_directory, store) = temp_store().await;
        store
            .set_model_pricing("aaaa", fixed_rule(2.0))
            .await
            .unwrap();
        store
            .set_model_pricing("bbbb", fixed_rule(4.0))
            .await
            .unwrap();

        let remaining = store.remove_model_pricing("aaaa").await.unwrap();

        assert!(!remaining.models.contains_key("aaaa"));
        assert_eq!(cny(&remaining.models["bbbb"]).fixed.input_per_million, 4.0);
        assert_eq!(store.pricing_settings().await.unwrap(), remaining);
    }

    /// 删除一条不存在的规则是空操作：不报错，也不改动已有规则。
    #[tokio::test]
    async fn removing_a_rule_that_does_not_exist_changes_nothing() {
        let (_directory, store) = temp_store().await;
        store
            .set_model_pricing("aaaa", fixed_rule(2.0))
            .await
            .unwrap();

        let remaining = store.remove_model_pricing("cccc").await.unwrap();

        assert_eq!(remaining.models.len(), 1);
        assert_eq!(cny(&remaining.models["aaaa"]).fixed.input_per_million, 2.0);
        assert_eq!(store.pricing_settings().await.unwrap(), remaining);
    }

    /// 负价与非有限价一律拒绝，且拒绝时库里不留任何痕迹。
    #[tokio::test]
    async fn negative_and_non_finite_rates_are_rejected() {
        let (_directory, store) = temp_store().await;
        let rejected = [
            ModelTokenPricing {
                cny: Some(CurrencyPricing {
                    fixed: TokenPrice {
                        input_per_million: -0.01,
                        ..TokenPrice::zero()
                    },
                    ..CurrencyPricing::zero()
                }),
                ..ModelTokenPricing::default()
            },
            ModelTokenPricing {
                cny: Some(CurrencyPricing {
                    peak: TokenPrice {
                        output_per_million: f64::NAN,
                        ..TokenPrice::zero()
                    },
                    ..CurrencyPricing::zero()
                }),
                ..ModelTokenPricing::default()
            },
            ModelTokenPricing {
                usd: Some(CurrencyPricing {
                    off_peak: TokenPrice {
                        cache_write_per_million: f64::INFINITY,
                        ..TokenPrice::zero()
                    },
                    ..CurrencyPricing::zero()
                }),
                ..ModelTokenPricing::default()
            },
        ];

        for pricing in rejected {
            let error = store.set_model_pricing("aaaa", pricing).await.unwrap_err();
            assert!(
                matches!(error, crate::Error::Config(_)),
                "expected a configuration error, got {error}"
            );
        }
        assert!(store.pricing_settings().await.unwrap().models.is_empty());
    }

    /// 价格规则挂在稳定模型标识上，标识必须干净：空白或空的键一律拒绝
    /// （静默 trim 会造出第二个看不见的键）。
    #[tokio::test]
    async fn a_blank_model_hash_is_rejected() {
        let (_directory, store) = temp_store().await;

        for blank in ["", "   ", "aaaa\nbbbb"] {
            assert!(
                store
                    .set_model_pricing(blank, fixed_rule(1.0))
                    .await
                    .is_err(),
                "{blank:?} must be rejected"
            );
            assert!(store.remove_model_pricing(blank).await.is_err());
        }
        assert!(store.pricing_settings().await.unwrap().models.is_empty());

        // 插件模型标识带 `/` 与 `:`，是正常键。
        let plugin_model = "plugin:dev.example/codex/gpt-test";
        store
            .set_model_pricing(plugin_model, fixed_rule(1.0))
            .await
            .unwrap();
        assert!(store
            .pricing_settings()
            .await
            .unwrap()
            .models
            .contains_key(plugin_model));
    }

    /// 旧版的整套全局价绝不自动套到任何模型上——包括用户后来保存新规则时。
    #[tokio::test]
    async fn the_legacy_global_row_is_never_applied_to_a_model() {
        let (_directory, store) = temp_store().await;
        insert_legacy_global_row(&store).await;

        assert!(store.pricing_settings().await.unwrap().models.is_empty());

        let saved = store
            .set_model_pricing("aaaa", fixed_rule(2.0))
            .await
            .unwrap();
        assert_eq!(saved.models.len(), 1);
        assert_eq!(cny(&saved.models["aaaa"]).fixed.input_per_million, 2.0);
        assert_eq!(cny(&saved.models["aaaa"]).fixed.output_per_million, 8.0);
    }

    /// 用户成功保存第一条按模型规则后，旧版全局价那行被清掉：它的语义已经被取代，
    /// 留着只会让人以为还有第二处真相。
    #[tokio::test]
    async fn an_explicit_save_retires_the_legacy_global_row() {
        let (_directory, store) = temp_store().await;
        insert_legacy_global_row(&store).await;

        store
            .set_model_pricing("aaaa", fixed_rule(2.0))
            .await
            .unwrap();

        let leftover: Option<String> = sqlx::query_scalar(
            "SELECT value_json FROM service_settings WHERE setting_key = 'token_pricing'",
        )
        .fetch_optional(store.pool())
        .await
        .unwrap();
        assert!(leftover.is_none(), "the legacy global row must not survive");
        assert!(store
            .pricing_settings()
            .await
            .unwrap()
            .models
            .contains_key("aaaa"));
    }

    /// 新建模型的初始草稿是固定单价、两个币种都未定价；代码不预填任何厂商的官方价，
    /// 未定价就是未定价。
    #[test]
    fn a_new_model_draft_is_fixed_and_unpriced() {
        let draft = ModelTokenPricing::default();

        assert_eq!(draft.mode, PricingMode::Fixed);
        assert!(draft.cny.is_none());
        assert!(draft.usd.is_none());
    }

    /// 没写全的规则体不会继承任何厂商价：写了的币种照收，缺的位补 0，
    /// 没写的币种保持未定价——它不能被看成"价格为零"。
    #[test]
    fn a_partial_rule_body_leaves_the_other_currency_unpriced() {
        let draft: ModelTokenPricing = serde_json::from_str("{}").unwrap();
        assert_eq!(draft, ModelTokenPricing::default());

        let partial: ModelTokenPricing = serde_json::from_str(
            r#"{"mode":"peak_off_peak","cny":{"peak":{"input_per_million":1.5}}}"#,
        )
        .unwrap();
        assert_eq!(partial.mode, PricingMode::PeakOffPeak);
        let cny = partial.cny.expect("cny rule");
        assert_eq!(cny.peak.input_per_million, 1.5);
        assert_eq!(cny.peak.output_per_million, 0.0);
        assert_eq!(cny.fixed.input_per_million, 0.0);
        assert!(partial.usd.is_none());
    }

    /// 价格表读不出来时不能变成"改也改不了"：读成空表，下一次保存覆盖坏行。
    #[tokio::test]
    async fn an_unreadable_pricing_row_is_replaced_instead_of_blocking_saves() {
        let (_directory, store) = temp_store().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?)",
        )
        .bind(MODEL_PRICING_SETTINGS_KEY)
        .bind(r#"{"models":{"aaaa":{"mode":"hourly","cny":null}}}"#)
        .bind(now_ms())
        .execute(store.pool())
        .await
        .unwrap();

        assert!(store.pricing_settings().await.unwrap().models.is_empty());

        let saved = store
            .set_model_pricing("aaaa", fixed_rule(2.0))
            .await
            .unwrap();
        assert_eq!(saved.models.len(), 1);
        assert_eq!(cny(&saved.models["aaaa"]).fixed.input_per_million, 2.0);
        assert_eq!(store.pricing_settings().await.unwrap(), saved);
    }

    /// 旧版全局价那行的形状永远不会变成按模型规则：前端收到的仍是空表。
    #[test]
    fn a_global_row_shape_never_becomes_a_model_rule() {
        let settings: TokenPricingSettings = serde_json::from_str(
            r#"{"mode":"fixed","cny":{"fixed":{"input_per_million":5.0,"output_per_million":25.0,"cache_read_per_million":0.5,"cache_write_per_million":6.25},"peak":{},"off_peak":{}},"usd":{}}"#,
        )
        .unwrap();

        assert!(settings.models.is_empty());
    }

    /// 与前端共享的线上形状：一个 `models` 映射，键是稳定模型标识，值带
    /// `mode` 与各币种（未配置的币种是 `null`）完整的单价。
    #[test]
    fn the_wire_shape_is_a_map_of_model_rules() {
        let settings = TokenPricingSettings {
            models: BTreeMap::from([(
                "aaaa".to_owned(),
                ModelTokenPricing {
                    mode: PricingMode::Fixed,
                    cny: Some(CurrencyPricing {
                        fixed: TokenPrice {
                            input_per_million: 1.0,
                            output_per_million: 8.0,
                            cache_read_per_million: 0.04,
                            cache_write_per_million: 0.0,
                        },
                        ..CurrencyPricing::zero()
                    }),
                    ..ModelTokenPricing::default()
                },
            )]),
        };
        let wire = serde_json::json!({
            "models": {
                "aaaa": {
                    "mode": "fixed",
                    "cny": {
                        "fixed": {
                            "input_per_million": 1.0,
                            "output_per_million": 8.0,
                            "cache_read_per_million": 0.04,
                            "cache_write_per_million": 0.0
                        },
                        "peak": {
                            "input_per_million": 0.0,
                            "output_per_million": 0.0,
                            "cache_read_per_million": 0.0,
                            "cache_write_per_million": 0.0
                        },
                        "off_peak": {
                            "input_per_million": 0.0,
                            "output_per_million": 0.0,
                            "cache_read_per_million": 0.0,
                            "cache_write_per_million": 0.0
                        }
                    },
                    "usd": null
                }
            }
        });

        assert_eq!(serde_json::to_value(&settings).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<TokenPricingSettings>(wire).unwrap(),
            settings
        );
    }

    #[test]
    fn devin_defaults_are_disabled_and_use_isolated_ports() {
        let settings = DevinSettings::default();

        assert!(!settings.enabled);
        assert_eq!(settings.api_port, 43_110);
        assert_eq!(settings.inference_port, 43_111);
        assert_eq!(settings.local_api_port, 43_112);
        assert!(settings.bindings.is_empty());
    }

    #[test]
    fn devin_settings_reject_duplicate_model_uids() {
        let settings = DevinSettings {
            bindings: vec![
                DevinModelBinding::new("model-a", "hash-a"),
                DevinModelBinding::new("model-a", "hash-b"),
            ],
            ..DevinSettings::default()
        };

        let error = settings.validate().unwrap_err().to_string();
        assert!(error.contains("duplicate Devin model UID"));
    }

    /// 连续两次读取只解析一次：版本没变就直接用缓存（Devin 网关每个请求读一次设置）。
    ///
    /// 逆变检查：把版本号判据去掉（每次读都重新解析）→ 这条用例必红（解析次数变成 2）。
    #[tokio::test]
    async fn reading_devin_settings_twice_parses_once() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        store
            .set_devin_settings(DevinSettings {
                enabled: true,
                bindings: vec![DevinModelBinding::new("deven-model", "0123abcd")],
                ..DevinSettings::default()
            })
            .await
            .unwrap();
        let parses_before = store
            .devin_settings_cache
            .parses
            .load(std::sync::atomic::Ordering::Relaxed);

        let first = store.devin_settings().await.unwrap();
        let second = store.devin_settings().await.unwrap();
        assert_eq!(first, second);
        assert!(first.enabled);
        assert_eq!(
            store
                .devin_settings_cache
                .parses
                .load(std::sync::atomic::Ordering::Relaxed)
                - parses_before,
            1,
            "第二次读取必须命中缓存，不能再解析一遍"
        );

        // 写一次设置 → 版本号变 → 下一次读取必须重新解析，缓存不会给出旧设置。
        let changed = DevinSettings {
            enabled: false,
            bindings: vec![DevinModelBinding::new("deven-model", "0123abcd")],
            ..first.clone()
        };
        store.set_devin_settings(changed.clone()).await.unwrap();
        let reloaded = store.devin_settings().await.unwrap();
        assert_eq!(reloaded, changed, "写后必须读到新设置");
        assert!(!reloaded.enabled);
        assert_eq!(
            store
                .devin_settings_cache
                .parses
                .load(std::sync::atomic::Ordering::Relaxed)
                - parses_before,
            2,
            "设置变了要重新解析一次"
        );
    }

    #[tokio::test]
    async fn devin_settings_persist_without_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        let settings = DevinSettings {
            enabled: true,
            bindings: vec![DevinModelBinding::new("deven-model", "0123abcd")],
            ..DevinSettings::default()
        };

        assert_eq!(
            store.set_devin_settings(settings.clone()).await.unwrap(),
            settings
        );
        assert_eq!(store.devin_settings().await.unwrap(), settings);

        let raw = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = 'devin_router'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(!raw.contains("api_key"));
    }

    /// 旧版本的 Devin 设置行在读取时必须完整可用：绑定保持启用，路由字段取默认值，
    /// 原主哈希仍然是生效模型。保存回读不得改变这三点。
    #[tokio::test]
    async fn legacy_devin_row_keeps_its_primary_hash_across_load_and_save() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        let legacy = r#"{"enabled":true,"auth_token":"","api_port":43110,"inference_port":43111,"local_api_port":43112,"bindings":[{"model_uid":"deven-model","model_hash":"0123abcd","display_name":"Legacy","enabled":true}]}"#;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('devin_router', ?, 0)",
        )
        .bind(legacy)
        .execute(store.pool())
        .await
        .unwrap();

        let loaded = store.devin_settings().await.unwrap();
        assert!(loaded.enabled);
        let binding = &loaded.bindings[0];
        assert_eq!(binding.model_uid, "deven-model");
        assert_eq!(binding.model_hash, "0123abcd");
        assert!(binding.enabled);
        assert_eq!(binding.kind, crate::devin::DevinBindingKind::Standard);
        assert!(binding.routes.is_empty());
        assert_eq!(binding.active_route_id, None);
        assert_eq!(binding.effective_model_hash(), "0123abcd");

        // Saving through the store must not silently migrate the binding away
        // from its legacy primary hash.
        store.set_devin_settings(loaded.clone()).await.unwrap();
        let reloaded = store.devin_settings().await.unwrap();
        assert_eq!(reloaded.bindings[0].model_hash, "0123abcd");
        assert!(reloaded.bindings[0].routes.is_empty());
        assert_eq!(reloaded.bindings[0].effective_model_hash(), "0123abcd");
    }
}
