//! Persists application settings.

mod capabilities;
mod commit;
mod compaction;
mod desktop;
mod devin;
mod ports;
mod pricing;
mod proxy;

pub use capabilities::{ModelCapabilities, ModelCapabilitySettings};
pub use commit::{CommitPromptLocale, CommitSettings};
pub use compaction::CompactionSettings;
pub use desktop::{DesktopSettings, DesktopSettingsPatch, PendingUpdate};
pub(crate) use devin::DevinSettingsCache;

pub(crate) use ports::migrate_cursor_mode;
pub use ports::{CursorMode, PortSettings, TabMode, TabSettings};
pub use pricing::{
    CurrencyPricing, ModelTokenPricing, PricingMode, TokenPrice, TokenPricingSettings,
};
#[cfg(test)]
pub(crate) use proxy::read_proxy_settings;
pub(crate) use proxy::ProxySettingsSecret;
pub use proxy::{ProxyMode, ProxySettings, ProxySettingsInput};

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
/// 每个模型偏离缺省值的能力声明（目前只有图片输入），见 `capabilities.rs`。
const MODEL_CAPABILITIES_SETTINGS_KEY: &str = "model_capabilities";
/// 旧版的全套全局价。只读键已不存在；唯一剩下的用途是用户成功保存第一条
/// 按模型规则后把它删掉，见 `Store::set_model_pricing`。
const LEGACY_PRICING_SETTINGS_KEY: &str = "token_pricing";
const DEVIN_SETTINGS_KEY: &str = "devin_router";
/// Context-compaction policy: opt-out, independent summary model, output cap.
const COMPACTION_SETTINGS_KEY: &str = "compaction";
/// Upper bound on the summarize call's output cap. A summary competes with the
/// retained turns for the window, so a cap above this is a configuration error,
/// not a request the provider should be asked to accept.
const SUMMARY_OUTPUT_TOKENS_MAX: u64 = 32_768;
/// Upper bound on the absolute retained-tail cap. The percentage already scales
/// with the window; this only stops a typo from disabling the retention limit.
const MAX_RETAINED_TOKENS_MAX: u64 = 1_000_000;
/// The reserve and the retention share the same window; their effective sum has
/// to leave room for the prompt and for the next turn to happen at all.
const RESERVE_RETENTION_PERCENT_MAX: u64 = 80;
/// Shipped defaults, mirrored by `run::compaction`'s divisors. The getters below
/// and the cross-field check use these so "unset" has one meaning.
const RESERVE_PERCENT_DEFAULT: u64 = 10;
const RETENTION_PERCENT_DEFAULT: u64 = 20;

/// Embedded default system prompts for commit message generation.
pub const DEFAULT_COMMIT_PROMPT_ZH_CN: &str =
    include_str!("../../../prompt/cursor/commit/zh-CN.md");
pub const DEFAULT_COMMIT_PROMPT_EN_US: &str =
    include_str!("../../../prompt/cursor/commit/en-US.md");

pub const PUBLIC_TAB_SERVICE_URL: &str = "https://tab.leokun.cn";

#[cfg(test)]
use crate::store::Store;

#[cfg(test)]
mod tests;
