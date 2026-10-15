//! Codex 接入边界；原版客户端保有任务与工具执行循环。
pub mod bridge;
pub mod catalog;
pub mod config;
pub mod gateway;
mod upstream;

use crate::{Error, Result};
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
    pub home: String,
    pub calls: usize,
    pub last_error: Option<String>,
}
