//! External OpenAI/Anthropic-compatible API settings.

use serde::{Deserialize, Serialize};

use crate::{
    store::{now_ms, Store},
    Result,
};

const EXTERNAL_API_SETTINGS_KEY: &str = "external_api";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExternalApiSettings {
    pub enabled: bool,
    /// Bearer token required by every external request. Empty while disabled.
    pub api_key: String,
}

impl Store {
    pub async fn external_api_settings(&self) -> Result<ExternalApiSettings> {
        let row: Option<String> =
            sqlx::query_scalar("SELECT value_json FROM service_settings WHERE setting_key = ?")
                .bind(EXTERNAL_API_SETTINGS_KEY)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row
            .map(|json| serde_json::from_str(&json))
            .transpose()?
            .unwrap_or_default())
    }

    pub async fn set_external_api_settings(
        &self,
        settings: &ExternalApiSettings,
    ) -> Result<ExternalApiSettings> {
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) \
             ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(EXTERNAL_API_SETTINGS_KEY)
        .bind(serde_json::to_string(settings)?)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(settings.clone())
    }
}
