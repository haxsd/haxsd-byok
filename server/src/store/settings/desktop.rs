//! 桌面设置：界面偏好、升级记录与按字段合并。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::store::{now_ms, Store};
use crate::Result;

use super::DESKTOP_SETTINGS_KEY;

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
    pub devin_install_root: Option<String>,
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
            devin_install_root: None,
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
    #[serde(default, deserialize_with = "double_option")]
    pub devin_install_root: Option<Option<String>>,
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
        if let Some(devin_install_root) = self.devin_install_root {
            settings.devin_install_root = devin_install_root;
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

impl Store {
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
}
