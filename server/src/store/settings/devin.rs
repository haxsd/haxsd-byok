//! Devin 设置的解析缓存与持久化。

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

use crate::store::{now_ms, Store};
use crate::Result;

use super::DEVIN_SETTINGS_KEY;

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
    pub(super) parses: AtomicU64,
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
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms")
            .bind(DEVIN_SETTINGS_KEY)
            .bind(value_json)
            .bind(now_ms())
            .execute(&mut *transaction)
            .await?;
        if !settings.enabled {
            sqlx::query("DELETE FROM devin_session_models")
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        // 写进去之后才 +1：先加会让并发读把"还没落库的旧行"标在新版本上。
        self.devin_settings_cache.bump();
        Ok(settings)
    }
}
