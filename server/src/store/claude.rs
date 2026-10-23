//! Claude Code 网关设置与本地令牌；令牌与设置分开存放，不随设置序列化。
use super::{now_ms, Store};
use crate::{anthropic::ClaudeSettings, Error, Result};

impl Store {
    pub async fn claude_settings(&self) -> Result<ClaudeSettings> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT value_json FROM service_settings WHERE setting_key = 'claude_gateway'",
        )
        .fetch_optional(&self.pool)
        .await?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(Error::from))
            .unwrap_or_else(|| Ok(ClaudeSettings::default()))
    }

    pub async fn set_claude_settings(&self, settings: ClaudeSettings) -> Result<ClaudeSettings> {
        settings.validate()?;
        for (label, id) in settings.model_references() {
            if self.model(id).await?.is_none() {
                return Err(Error::Config(format!("unknown Claude {label} model: {id}")));
            }
        }
        let _write = self.writes.lock().await;
        sqlx::query("INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('claude_gateway', ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms")
            .bind(serde_json::to_string(&settings)?).bind(now_ms()).execute(&self.pool).await?;
        Ok(settings)
    }

    /// 首次生成足够随机的令牌；写协调器覆盖读-生成-写，避免并发生成两个值。
    ///
    /// 与 Codex 网关同一做法：令牌只生成一次、保存在 `service_settings` 里，
    /// 每个请求读取，不在设置 JSON 中回传。
    ///
    /// 轮换走 [`Store::rotate_claude_token`]（网关会同时刷新内存缓存），不是删行：
    /// 网关在内存里缓存令牌以免每个请求都取全局写锁，删行不会让缓存里的旧令牌失效。
    pub async fn claude_token(&self) -> Result<String> {
        let _write = self.writes.lock().await;
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT value_json FROM service_settings WHERE setting_key = 'claude_gateway_token'",
        )
        .fetch_optional(&self.pool)
        .await?;
        if let Some(raw) = raw {
            return Ok(serde_json::from_str(&raw)?);
        }
        let token = new_claude_token();
        sqlx::query("INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('claude_gateway_token', ?, ?)")
            .bind(serde_json::to_string(&token)?).bind(now_ms()).execute(&self.pool).await?;
        Ok(token)
    }

    /// 生成并保存一个新令牌，返回它；旧令牌在网关刷新缓存后立即失效。
    pub async fn rotate_claude_token(&self) -> Result<String> {
        let _write = self.writes.lock().await;
        let token = new_claude_token();
        sqlx::query("INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('claude_gateway_token', ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms")
            .bind(serde_json::to_string(&token)?).bind(now_ms()).execute(&self.pool).await?;
        Ok(token)
    }
}

fn new_claude_token() -> String {
    String::from("haxsd-")
        + &uuid::Uuid::new_v4().simple().to_string()
        + &uuid::Uuid::new_v4().simple().to_string()
}
