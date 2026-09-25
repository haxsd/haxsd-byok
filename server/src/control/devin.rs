//! Devin router settings and explicit host integration endpoints.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};
use serde::Deserialize;

use crate::{
    devin::{
        gateway::DevinListening,
        host_detect,
        host_patch::{self, DevinPorts, PatchReceipt, PatchStatus},
        host_status as host_status_module, DevinSettings,
    },
    Error, Result,
};

use super::{ControlService, DevinStatus};

pub async fn get(State(service): State<ControlService>) -> Result<Json<DevinSettings>> {
    Ok(Json(service.devin_settings().await?))
}

#[derive(Clone)]
pub struct DevinStatusState {
    pub service: ControlService,
    pub listening: DevinListening,
}

pub async fn status(State(state): State<Arc<DevinStatusState>>) -> Result<Json<DevinStatus>> {
    let listening = state.listening.is_listening();
    Ok(Json(state.service.devin_status(listening).await?))
}

pub async fn update(
    State(service): State<ControlService>,
    Json(settings): Json<DevinSettings>,
) -> Result<Json<DevinSettings>> {
    Ok(Json(service.set_devin_settings(settings).await?))
}

#[derive(Debug, Deserialize)]
pub struct HostPathQuery {
    /// Omitted means "find the installation yourself".
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HostPatchInput {
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HostRestoreInput {
    pub receipt: PatchReceipt,
}

/// A missing or blank path resolves the installation automatically, so the page
/// works without the user pasting an absolute path.
fn resolve_host_path(input: Option<&str>) -> Result<PathBuf> {
    if let Some(value) = input.map(str::trim).filter(|value| !value.is_empty()) {
        return explicit_path(value);
    }
    let detected = host_detect::detect();
    match detected.path() {
        Some(path) => Ok(path.to_path_buf()),
        None => Err(Error::Config(detected.explanation())),
    }
}

pub async fn host_status(Query(query): Query<HostPathQuery>) -> Result<Json<PatchStatus>> {
    let path = resolve_host_path(query.path.as_deref())?;
    Ok(Json(host_status_module::status(&path)?))
}

pub async fn host_apply(
    State(service): State<ControlService>,
    Json(input): Json<HostPatchInput>,
) -> Result<Json<PatchReceipt>> {
    let settings = service.devin_settings().await?;
    if !settings.enabled {
        return Err(Error::Config(
            "enabling the Devin gateway is required before applying a host patch".into(),
        ));
    }
    let path = resolve_host_path(input.path.as_deref())?;
    let ports = DevinPorts {
        api_port: settings.api_port,
        inference_port: settings.inference_port,
        local_api_port: settings.local_api_port,
    };
    Ok(Json(host_patch::apply(&path, ports)?))
}

pub async fn host_restore(Json(input): Json<HostRestoreInput>) -> Result<Json<serde_json::Value>> {
    host_patch::restore(&input.receipt)?;
    Ok(Json(serde_json::json!({"restored": true})))
}

/// A pasted path may be the host file itself or any directory of the installation.
/// Users paste the directory they see in the file manager, and the relative part
/// below it is fixed, so refusing directories only produces avoidable failures —
/// reading a directory is what surfaced as `os error 5`.
fn explicit_path(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value.trim());
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(Error::Config(
            "Devin host file path must be absolute".into(),
        ));
    }
    if path.is_file() {
        return Ok(path);
    }
    let inside = path.join(host_detect::HOST_RELATIVE);
    if inside.is_file() {
        return Ok(inside);
    }
    Err(Error::Config(format!(
        "{} is not a Devin host file; expected the file, or a directory containing {}",
        path.display(),
        host_detect::HOST_RELATIVE
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 安装目录是用户看得见、也最可能粘贴的东西；补上固定的相对路径比要求他
    /// 记住 `<install>\resources\app\...\extension.js` 更合理，文件路径本身也必须
    /// 继续可用。
    #[test]
    fn a_pasted_install_directory_resolves_to_the_host_file_inside_it() {
        let directory = tempfile::tempdir().unwrap();
        let install = directory.path().join("Windsurf");
        let host = install.join(host_detect::HOST_RELATIVE);
        std::fs::create_dir_all(host.parent().unwrap()).unwrap();
        std::fs::write(&host, b"// host").unwrap();

        assert_eq!(
            explicit_path(install.to_str().unwrap()).unwrap(),
            host,
            "a directory must resolve to the host file below it"
        );
        assert_eq!(
            explicit_path(host.to_str().unwrap()).unwrap(),
            host,
            "the host file itself must stay accepted"
        );
    }

    #[test]
    fn a_directory_without_a_host_file_says_what_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let error = explicit_path(directory.path().to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("extension.js"), "{error}");
    }

    #[test]
    fn a_relative_path_is_refused() {
        assert!(explicit_path("Windsurf").is_err());
    }
}
