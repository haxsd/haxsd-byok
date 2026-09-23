//! Persists application settings.
use serde::{Deserialize, Deserializer, Serialize};

use crate::Result;

use super::{now_ms, Store};

const PORT_SETTINGS_KEY: &str = "network_ports";
const PROXY_SETTINGS_KEY: &str = "outbound_proxy";
const TAB_SETTINGS_KEY: &str = "cursor_tab";
const DESKTOP_SETTINGS_KEY: &str = "desktop_lifecycle";
const COMMIT_SETTINGS_KEY: &str = "commit_settings";
const CURSOR_TAKEOVER_ENABLED_KEY: &str = "cursor_takeover_enabled";
const PRICING_SETTINGS_KEY: &str = "token_pricing";
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
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TokenPrice {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_read_per_million: f64,
    pub cache_write_per_million: f64,
}

impl TokenPrice {
    /// DeepSeek-V4.1-Flash 高峰时段官方单价（元 / 百万 token）。
    pub const fn peak_cny() -> Self {
        Self {
            input_per_million: 2.0,
            output_per_million: 8.0,
            cache_read_per_million: 0.04,
            cache_write_per_million: 0.0,
        }
    }

    /// DeepSeek-V4.1-Flash 低谷时段官方单价（元），为高峰价的一半。
    pub const fn off_peak_cny() -> Self {
        Self {
            input_per_million: 1.0,
            output_per_million: 4.0,
            cache_read_per_million: 0.02,
            cache_write_per_million: 0.0,
        }
    }

    /// DeepSeek-V4.1-Flash 高峰时段官方单价（美元 / 百万 token）。
    pub const fn peak_usd() -> Self {
        Self {
            input_per_million: 0.3,
            output_per_million: 1.2,
            cache_read_per_million: 0.006,
            cache_write_per_million: 0.0,
        }
    }

    /// DeepSeek-V4.1-Flash 低谷时段官方单价（美元），为高峰价的一半。
    pub const fn off_peak_usd() -> Self {
        Self {
            input_per_million: 0.15,
            output_per_million: 0.6,
            cache_read_per_million: 0.003,
            cache_write_per_million: 0.0,
        }
    }
}

impl Default for TokenPrice {
    fn default() -> Self {
        Self::peak_cny()
    }
}

/// 首页「价值估算」使用的计价模式。
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingMode {
    /// 全时段使用同一组单价。
    Fixed,
    /// 按高峰 / 低谷时段分别计价（默认）。
    #[default]
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
    /// 人民币价格，简体中文界面使用。
    pub const fn cny() -> Self {
        Self {
            fixed: TokenPrice::peak_cny(),
            peak: TokenPrice::peak_cny(),
            off_peak: TokenPrice::off_peak_cny(),
        }
    }

    /// 美元价格，英文界面使用。
    pub const fn usd() -> Self {
        Self {
            fixed: TokenPrice::peak_usd(),
            peak: TokenPrice::peak_usd(),
            off_peak: TokenPrice::off_peak_usd(),
        }
    }
}

impl Default for CurrencyPricing {
    fn default() -> Self {
        Self::cny()
    }
}

/// 首页「价值估算」的价格设置。
///
/// 界面语言决定使用哪一套价格：简体中文用人民币，英文用美元。
/// 两种币种各自维护完整的价格表，切换语言不会互相影响。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TokenPricingSettings {
    pub mode: PricingMode,
    /// 人民币价格。
    pub cny: CurrencyPricing,
    /// 美元价格。
    pub usd: CurrencyPricing,
}

impl Default for TokenPricingSettings {
    fn default() -> Self {
        Self {
            mode: PricingMode::PeakOffPeak,
            cny: CurrencyPricing::cny(),
            usd: CurrencyPricing::usd(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    #[default]
    Default,
    Custom,
}

impl ProxyMode {
    pub fn is_custom(self) -> bool {
        self == Self::Custom
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

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DesktopSettings {
    #[serde(default)]
    pub silent_start: bool,
    #[serde(default = "default_true")]
    pub show_dock_icon: bool,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            silent_start: false,
            show_dock_icon: true,
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

impl Store {
    pub async fn devin_settings(&self) -> Result<crate::devin::DevinSettings> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(DEVIN_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        let settings = value
            .map(|value| {
                serde_json::from_str::<crate::devin::DevinSettings>(&value)
                    .map_err(crate::Error::Json)
            })
            .unwrap_or_else(|| Ok(crate::devin::DevinSettings::default()))?;
        settings.validate()?;
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
        Ok(settings)
    }

    /// Whether this product should take Cursor over.
    ///
    /// Absent means no. Taking over rewrites the user's Cursor configuration and
    /// force-terminates the running editor, so it has to be something they asked
    /// for. This used to default to yes, which meant a fresh install that merely
    /// trusted the CA took Cursor over before the user ever touched the switch —
    /// and on a machine where another product also drives Cursor, that is not a
    /// decision this app gets to make on its own.
    pub(crate) async fn cursor_takeover_enabled(&self) -> Result<bool> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(CURSOR_TAKEOVER_ENABLED_KEY)
        .fetch_optional(&self.pool)
        .await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or(Ok(false))
    }

    pub(crate) async fn set_cursor_takeover_enabled(&self, enabled: bool) -> Result<()> {
        let value_json = serde_json::to_string(&enabled)?;
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(CURSOR_TAKEOVER_ENABLED_KEY)
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
        let value_json = serde_json::to_string(&settings)?;
        let _write = self.writes.lock().await;
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

    pub async fn set_service_port(&self, port: u16) -> Result<()> {
        let mut settings = self.port_settings().await?;
        settings.service_port = port;
        self.set_port_settings(settings).await
    }

    pub async fn set_proxy_port(&self, port: u16) -> Result<()> {
        let mut settings = self.port_settings().await?;
        settings.proxy_port = port;
        self.set_port_settings(settings).await
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

    pub async fn set_desktop_settings(&self, settings: DesktopSettings) -> Result<()> {
        let value_json = serde_json::to_string(&settings)?;
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(DESKTOP_SETTINGS_KEY)
        .bind(value_json)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
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

    pub async fn pricing_settings(&self) -> Result<TokenPricingSettings> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(PRICING_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(TokenPricingSettings::default()))
    }

    pub async fn set_pricing_settings(
        &self,
        settings: TokenPricingSettings,
    ) -> Result<TokenPricingSettings> {
        let value_json = serde_json::to_string(&settings)?;
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(PRICING_SETTINGS_KEY)
        .bind(value_json)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        read_proxy_settings, CommitPromptLocale, CommitSettings, CurrencyPricing, PricingMode,
        ProxyMode, ProxySettingsInput, ProxySettingsSecret, Store, TokenPrice,
        TokenPricingSettings, DEFAULT_COMMIT_PROMPT_EN_US, DEFAULT_COMMIT_PROMPT_ZH_CN,
        PROXY_SETTINGS_KEY,
    };
    use crate::devin::{DevinModelBinding, DevinSettings};

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

    #[test]
    fn an_unreadable_proxy_row_reads_as_no_proxy() {
        assert!(serde_json::from_str::<ProxySettingsSecret>(LEGACY_PROXY_ROW).is_err());

        let settings = read_proxy_settings(LEGACY_PROXY_ROW);
        assert_eq!(settings.mode, ProxyMode::Default);
        assert!(settings.address.is_empty());
        assert!(!settings.auth_enabled);
    }

    #[tokio::test]
    async fn taking_cursor_over_requires_an_explicit_choice() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();

        // A database that has never recorded a choice must not take Cursor over:
        // doing so rewrites the user's Cursor configuration and force-terminates
        // their editor.
        assert!(!store.cursor_takeover_enabled().await.unwrap());

        store.set_cursor_takeover_enabled(true).await.unwrap();
        assert!(store.cursor_takeover_enabled().await.unwrap());

        store.set_cursor_takeover_enabled(false).await.unwrap();
        assert!(!store.cursor_takeover_enabled().await.unwrap());
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

    #[tokio::test]
    async fn token_pricing_settings_persists_and_reads_back() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();

        assert_eq!(
            store.pricing_settings().await.unwrap(),
            TokenPricingSettings::default()
        );

        let custom = TokenPricingSettings {
            mode: PricingMode::Fixed,
            cny: CurrencyPricing {
                fixed: TokenPrice {
                    input_per_million: 3.0,
                    output_per_million: 15.0,
                    cache_read_per_million: 0.3,
                    cache_write_per_million: 3.75,
                },
                ..CurrencyPricing::cny()
            },
            ..TokenPricingSettings::default()
        };
        let saved = store.set_pricing_settings(custom).await.unwrap();
        assert_eq!(saved, custom);

        assert_eq!(store.pricing_settings().await.unwrap(), custom);
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

    /// 旧版本只存了四个单价字段，读取时必须能平滑降级到新的默认值，
    /// 而不是解析失败导致首页报错。
    #[test]
    fn legacy_pricing_row_falls_back_to_defaults() {
        let settings: TokenPricingSettings =
            serde_json::from_str(r#"{"input_per_million":5.0,"output_per_million":25.0,"cache_read_per_million":0.5,"cache_write_per_million":6.25}"#)
                .unwrap();
        assert_eq!(settings, TokenPricingSettings::default());
    }

    /// 默认价格应当是 DeepSeek-V4.1-Flash 的官方价，且人民币与美元各自独立。
    #[test]
    fn default_pricing_holds_the_official_rates_for_both_currencies() {
        let settings = TokenPricingSettings::default();
        assert_eq!(settings.mode, PricingMode::PeakOffPeak);

        let cny = settings.cny;
        assert_eq!(cny.peak.input_per_million, 2.0);
        assert_eq!(cny.peak.output_per_million, 8.0);
        assert_eq!(cny.peak.cache_read_per_million, 0.04);
        assert_eq!(cny.off_peak.input_per_million, 1.0);
        assert_eq!(cny.off_peak.output_per_million, 4.0);
        assert_eq!(cny.off_peak.cache_read_per_million, 0.02);

        let usd = settings.usd;
        assert_eq!(usd.peak.input_per_million, 0.3);
        assert_eq!(usd.peak.output_per_million, 1.2);
        assert_eq!(usd.peak.cache_read_per_million, 0.006);
        assert_eq!(usd.off_peak.input_per_million, 0.15);
        assert_eq!(usd.off_peak.output_per_million, 0.6);
        assert_eq!(usd.off_peak.cache_read_per_million, 0.003);

        // DeepSeek 不单独收取缓存写入费用。
        for price in [cny.peak, cny.off_peak, usd.peak, usd.off_peak] {
            assert_eq!(price.cache_write_per_million, 0.0);
        }
    }
}
