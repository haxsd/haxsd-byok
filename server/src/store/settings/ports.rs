//! 端口、Cursor 来源模式与 TAB 设置。

use serde::{Deserialize, Serialize};

use crate::store::{now_ms, Store};
use crate::Result;

use super::{
    CURSOR_MODE_KEY, LEGACY_CURSOR_TAKEOVER_KEY, PORT_SETTINGS_KEY, PUBLIC_TAB_SERVICE_URL,
    TAB_SETTINGS_KEY,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PortSettings {
    pub proxy_port: u16,
    pub service_port: u16,
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

impl TabSettings {
    pub fn service_url(&self) -> Option<&str> {
        match self.mode {
            TabMode::Public => Some(PUBLIC_TAB_SERVICE_URL),
            TabMode::Direct => None,
            TabMode::Custom => Some(&self.address),
        }
    }
}

impl Store {
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
