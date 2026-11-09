//! 按模型的能力声明：目前只有"能否接收图片"。
//!
//! 存在 `service_settings` 而不是 `model_configs` 的新列里：加列需要迁移，而 1.7.8
//! 曾发布过一个 27 号迁移、之后被回滚——已经应用过它的库再遇到任何未知版本都会在
//! 启动时 `VersionMissing`。KV 键不涉及迁移，老库升级零操作。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::ModelConfig;
use crate::store::{now_ms, Store};
use crate::Result;

use super::MODEL_CAPABILITIES_SETTINGS_KEY;

/// 一个模型偏离缺省值的能力。缺省（全部字段缺席）就是"与以前一样"。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct ModelCapabilities {
    pub supports_images: bool,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            supports_images: true,
        }
    }
}

/// 只记录与缺省值不同的模型；键是 `model_hash`。
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ModelCapabilitySettings {
    pub models: BTreeMap<String, ModelCapabilities>,
}

impl ModelCapabilitySettings {
    pub fn apply(&self, model: &mut ModelConfig) {
        let capabilities = self
            .models
            .get(&model.model_hash)
            .copied()
            .unwrap_or_default();
        model.supports_images = capabilities.supports_images;
    }
}

impl Store {
    /// 读不出来（缺失、旧版残留、外部写坏）都按空表处理：能力退回缺省值，
    /// 与以前写死的目录一致，而不是让模型列表整体报错。
    pub async fn model_capabilities(&self) -> Result<ModelCapabilitySettings> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(MODEL_CAPABILITIES_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        Ok(value
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default())
    }

    /// 设置一个模型能否接收图片。设回缺省值时删掉这一项，表里只留真正关掉的模型。
    pub async fn set_model_supports_images(&self, model_hash: &str, supports: bool) -> Result<()> {
        let _write = self.writes.lock().await;
        let mut settings = self.model_capabilities().await?;
        let capabilities = ModelCapabilities {
            supports_images: supports,
        };
        if capabilities == ModelCapabilities::default() {
            if settings.models.remove(model_hash).is_none() {
                return Ok(());
            }
        } else {
            settings.models.insert(model_hash.to_owned(), capabilities);
        }
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(MODEL_CAPABILITIES_SETTINGS_KEY)
        .bind(serde_json::to_string(&settings)?)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
