//! Codex 设置和本地网关令牌；不复用其他客户端的凭据。
use super::{now_ms, Store};
use crate::{codex::CodexSettings, Error, Result};

impl Store {
    pub async fn codex_settings(&self) -> Result<CodexSettings> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT value_json FROM service_settings WHERE setting_key = 'codex_gateway'",
        )
        .fetch_optional(&self.pool)
        .await?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(Error::from))
            .unwrap_or_else(|| Ok(CodexSettings::default()))
    }

    pub async fn set_codex_settings(&self, settings: CodexSettings) -> Result<CodexSettings> {
        settings.validate()?;
        for id in &settings.model_hashes {
            let model = self
                .model(id)
                .await?
                .ok_or_else(|| Error::Config(format!("unknown Codex model: {id}")))?;
            if model.context_window_tokens.unwrap_or(0) < 4096 {
                return Err(Error::Config(format!(
                    "configure a context window of at least 4096 tokens for {}",
                    model.display_name
                )));
            }
        }
        let _write = self.writes.lock().await;
        sqlx::query("INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('codex_gateway', ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms")
            .bind(serde_json::to_string(&settings)?).bind(now_ms()).execute(&self.pool).await?;
        Ok(settings)
    }

    /// 首次生成足够随机的令牌；写协调器覆盖读-生成-写，避免并发生成两个值。
    pub async fn codex_token(&self) -> Result<String> {
        let _write = self.writes.lock().await;
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT value_json FROM service_settings WHERE setting_key = 'codex_gateway_token'",
        )
        .fetch_optional(&self.pool)
        .await?;
        if let Some(raw) = raw {
            return Ok(serde_json::from_str(&raw)?);
        }
        let token = format!(
            "haxsd-{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        sqlx::query("INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('codex_gateway_token', ?, ?)")
            .bind(serde_json::to_string(&token)?).bind(now_ms()).execute(&self.pool).await?;
        Ok(token)
    }
}
