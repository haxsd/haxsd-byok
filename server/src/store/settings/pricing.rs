//! 按模型的价格规则：类型、校验与持久化。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::store::{now_ms, Store};
use crate::Result;

use super::{LEGACY_PRICING_SETTINGS_KEY, MODEL_PRICING_SETTINGS_KEY};

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

impl Store {
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
