//! Codex 接入边界；原版客户端保有任务与工具执行循环。
pub mod bridge;
pub mod catalog;
pub mod config;
pub mod gateway;
mod upstream;

use crate::{Error, Result};
pub use config::ConfigFieldConflict;
pub use gateway::CodexGateway;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct CodexSettings {
    pub enabled: bool,
    pub port: u16,
    pub home: String,
    pub model_hashes: Vec<String>,
    pub default_model_hash: Option<String>,
}

impl Default for CodexSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 43_120,
            home: String::new(),
            model_hashes: Vec::new(),
            default_model_hash: None,
        }
    }
}

impl CodexSettings {
    pub fn validate(&self) -> Result<()> {
        if self.port == 0 {
            return Err(Error::Config("Codex port must be non-zero".into()));
        }
        if !self.home.is_empty() && !std::path::Path::new(&self.home).is_absolute() {
            return Err(Error::Config("Codex home must be an absolute path".into()));
        }
        let mut unique = HashSet::new();
        if self
            .model_hashes
            .iter()
            .any(|id| id.trim().is_empty() || !unique.insert(id))
        {
            return Err(Error::Config(
                "Codex model selections must be non-empty and unique".into(),
            ));
        }
        if let Some(default) = &self.default_model_hash {
            if !self.model_hashes.contains(default) {
                return Err(Error::Config("Codex default model must be selected".into()));
            }
        }
        if self.enabled && (self.model_hashes.is_empty() || self.default_model_hash.is_none()) {
            return Err(Error::Config(
                "select a default Codex model before enabling the gateway".into(),
            ));
        }
        if self.enabled && self.home.is_empty() {
            // 默认 home 解析不出来（系统没有可用的用户主目录）必须在启用之前就挡住：
            // 请求路径上再失败只能回一个含糊的 5xx，用户不知道要显式填一个 home。
            config::default_home().map_err(|_| {
                Error::Config("cannot resolve the Codex home directory; set it explicitly".into())
            })?;
        }
        Ok(())
    }

    pub fn resolved_home(&self) -> Result<std::path::PathBuf> {
        if self.home.is_empty() {
            config::default_home()
        } else {
            Ok(self.home.clone().into())
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CodexStatus {
    pub enabled: bool,
    pub listening: bool,
    pub port: u16,
    pub configured: bool,
    pub config_conflict: bool,
    /// 有接管凭证：即使受管字段被外部改过，撤销也能把它们还回去，界面据此允许撤销。
    pub restorable: bool,
    /// 与接管快照不一致的受管字段；界面据此说清"哪一项被改成了什么"。
    pub conflicts: Vec<ConfigFieldConflict>,
    pub home: String,
    pub calls: usize,
    pub last_error: Option<String>,
}
