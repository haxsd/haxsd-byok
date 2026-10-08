//! Implements the read-only "which copy is running" endpoint.
use axum::Json;
use serde::Serialize;

use crate::{config, diagnostics, Result};

/// 运行副本的自证信息：更新卡片用它说明"现在跑的是哪一份、装在哪、数据在哪"。
///
/// 本机可以同时存在多份安装（实测过 `D:\...` 与 `%LOCALAPPDATA%\...` 两份），而更新器
/// 只装到卸载注册表记录的那一份。看不到"跑的是哪一份"时，「更新了却还是旧版本」和
/// 「更新装到别处去了」在界面上长得一模一样。
#[derive(Serialize)]
pub struct AppInfo {
    pub version: String,
    /// 正在运行的这一个可执行文件：更新要生效，安装器必须落到它上面。
    pub executable_path: String,
    pub data_dir: String,
}

pub async fn get() -> Result<Json<AppInfo>> {
    Ok(Json(AppInfo {
        // 桌面外壳启动时写进来的版本；单独跑 cursor-server 时读不到。
        version: diagnostics::app_version().unwrap_or("unknown").to_owned(),
        executable_path: std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("unavailable: {error}")),
        data_dir: config::managed_data_dir()?.display().to_string(),
    }))
}
