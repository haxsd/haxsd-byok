//! Devin router settings and user-settings takeover endpoints.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use super::{ports::ensure_devin_ports_usable, ControlService, DevinStatus};
use crate::{
    devin::{
        gateway::{ensure_host_ports_listening, DevinBoundPorts, DevinListening},
        host_config::{self, DevinHostPorts, HostReceipt, HostStatus as SettingsHostStatus},
        host_detect,
        host_patch::{self, GateReceipt, GateStatus},
        model_uids as model_uid_table, DevinSettings,
    },
    store::Store,
    Error, Result,
};

pub async fn get(State(service): State<ControlService>) -> Result<Json<DevinSettings>> {
    Ok(Json(service.devin_settings().await?))
}

#[derive(Clone)]
pub struct DevinStatusState {
    pub service: ControlService,
    pub listening: DevinListening,
}

pub async fn status(State(state): State<Arc<DevinStatusState>>) -> Result<Json<DevinStatus>> {
    let bound_ports = state.listening.bound_ports();
    Ok(Json(state.service.devin_status(bound_ports).await?))
}

pub async fn update(
    State(service): State<ControlService>,
    Json(settings): Json<DevinSettings>,
) -> Result<Json<DevinSettings>> {
    Ok(Json(service.set_devin_settings(settings).await?))
}

/// 保存 Devin 设置：校验与端口占用检查都跑在写库之前，被拒绝时库里保持原值。
///
/// 网关只在启动时绑一次端口，没有重试与回退，所以"端口现在就被占着"必须在保存这
/// 一步拦下——否则界面只会显示"已保存、未监听、待重启"，重启也救不回来。
pub(super) async fn save_settings(
    store: &Store,
    settings: DevinSettings,
    gateway_ports: Option<DevinBoundPorts>,
) -> Result<DevinSettings> {
    let settings = settings.normalized();
    settings.validate_for_use()?;
    ensure_devin_ports_usable(store, &settings, gateway_ports).await?;
    store.set_devin_settings(settings).await
}

#[derive(Debug, Deserialize)]
pub struct HostPathInput {
    /// Omitted means "find the Devin data directory yourself".
    #[serde(default)]
    pub path: Option<String>,
    /// Installation directory or the `extension.js` file itself. Omitted means
    /// "detect the installed client".
    #[serde(default)]
    pub install_root: Option<String>,
}

/// 设置文件与扩展补丁缺一不可：设置给出本机端点，补丁让打包版 Devin 真正读取它们。
#[derive(Debug, Serialize)]
pub struct HostStatus {
    #[serde(flatten)]
    pub settings: host_config::HostStatus,
    pub extension: GateStatus,
}

#[derive(Debug, Serialize)]
pub struct HostApplyResult {
    pub settings: HostReceipt,
    pub extension: GateReceipt,
}

fn resolve_settings_path(input: Option<&str>) -> Result<PathBuf> {
    if let Some(value) = input.map(str::trim).filter(|value| !value.is_empty()) {
        return explicit_settings_path(value);
    }
    host_detect::settings_path().map_err(Error::Config)
}

fn resolve_extension_path(input: Option<&str>) -> Result<Option<PathBuf>> {
    if let Some(value) = input.map(str::trim).filter(|value| !value.is_empty()) {
        return host_detect::extension_path_from_input(value)
            .map(Some)
            .map_err(Error::Config);
    }
    Ok(detected_extension_path())
}

fn explicit_settings_path(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value.trim());
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(Error::Config("Devin settings path must be absolute".into()));
    }
    if path.is_file() {
        if path.file_name().is_some_and(|name| name == "settings.json") {
            return Ok(path);
        }
        return Err(Error::Config(format!(
            "{} is not a Devin settings.json file; leave the path empty to detect Devin, or \
             select the Devin data directory",
            path.display()
        )));
    }
    if path.is_dir() {
        if path.file_name().is_some_and(|name| name == "User") {
            return Ok(path.join("settings.json"));
        }
        return Ok(path.join("User").join("settings.json"));
    }
    if path.file_name().is_some_and(|name| name == "settings.json")
        && path.parent().is_some_and(Path::is_dir)
    {
        return Ok(path);
    }
    Err(Error::Config(format!(
        "{} is not a Devin settings file or data directory; leave the path empty to detect \
         Devin, or select the directory that contains User\\settings.json",
        path.display()
    )))
}

async fn on_blocking_pool<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| Error::Config(format!("Devin host task did not finish: {error}")))?
}

/// 写设置前确认网关已经在最终的 API 与推理端口上监听。否则客户端会在下一次启动时
/// 全部打到没人应答的端口，而界面只显示"已保存、待重启"。
fn host_config_precheck(
    settings: &DevinSettings,
    bound: Option<DevinBoundPorts>,
) -> Result<DevinHostPorts> {
    if !settings.enabled {
        return Err(Error::Config(
            "enable the Devin gateway before configuring Devin".into(),
        ));
    }
    let ports = DevinHostPorts {
        api_port: settings.api_port,
        inference_port: settings.inference_port,
    };
    ensure_host_ports_listening(ports, bound)?;
    Ok(ports)
}

pub async fn host_status(Query(input): Query<HostPathInput>) -> Result<Json<HostStatus>> {
    let settings_input = input.path;
    let install_input = input.install_root;
    Ok(Json(
        on_blocking_pool(move || {
            let path = resolve_settings_path(settings_input.as_deref())?;
            let extension = resolve_extension_path(install_input.as_deref())?;
            inspect_host(&path, extension.as_deref())
        })
        .await?,
    ))
}

fn detected_extension_path() -> Option<PathBuf> {
    host_detect::detect().path().map(Path::to_owned)
}

fn inspect_host(path: &Path, extension_path: Option<&Path>) -> Result<HostStatus> {
    Ok(HostStatus {
        settings: host_config::inspect(path)?,
        extension: host_patch::inspect(extension_path),
    })
}

fn apply_host(
    path: &Path,
    extension_path: Option<&Path>,
    ports: DevinHostPorts,
) -> Result<HostApplyResult> {
    let settings = host_config::inspect(path)?;
    if !settings.compatible {
        return Err(Error::Config(settings.message));
    }
    if !settings.unconfigured {
        return Err(Error::Config(format!(
            "Devin settings at {} already point somewhere else; restore them before applying \
             this configuration",
            path.display()
        )));
    }

    let extension_path = extension_path.ok_or_else(|| {
        Error::Config(
            "Devin extension.js not found; start Devin once or reinstall it before enabling this \
             integration"
                .into(),
        )
    })?;
    let extension = host_patch::inspect(Some(extension_path));
    if !extension.clean {
        let refusal = if extension.message.is_empty() {
            format!(
                "Devin extension at {} already has this application's settings gate; restore the \
                 clean extension first",
                extension_path.display()
            )
        } else {
            extension.message
        };
        return Err(Error::Config(refusal));
    }

    let extension_receipt = host_patch::apply(extension_path)?;
    match host_config::apply(path, ports) {
        Ok(settings_receipt) => Ok(HostApplyResult {
            settings: settings_receipt,
            extension: extension_receipt,
        }),
        Err(error) => match host_patch::restore(&extension_receipt) {
            Ok(()) => Err(error),
            Err(rollback) => Err(Error::Config(format!(
                "{error}; rolling back the Devin extension gate also failed: {rollback}"
            ))),
        },
    }
}

fn restore_host(path: &Path, extension_path: Option<&Path>) -> Result<()> {
    let settings = host_config::inspect(path)?;
    let extension = host_patch::inspect(extension_path);
    let settings_receipt = settings.restore_receipt.clone();
    let extension_receipt = extension.restore_receipt.clone();
    if settings_receipt.is_none() && extension_receipt.is_none() {
        return Err(Error::Config(format!(
            "{}; extension: {}",
            restore_refusal_message(path, &settings),
            extension_fallback_reason(&extension)
        )));
    }

    let mut failures = Vec::new();
    if let Some(receipt) = extension_receipt {
        if let Err(error) = host_patch::restore(&receipt) {
            failures.push(format!("extension: {error}"));
        }
    }
    if let Some(receipt) = settings_receipt {
        if let Err(error) = host_config::restore(&receipt) {
            failures.push(format!("settings: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "Devin restore incomplete: {}",
            failures.join("; ")
        )))
    }
}

fn extension_fallback_reason(status: &GateStatus) -> String {
    if !status.message.is_empty() {
        return status.message.clone();
    }
    status
        .path
        .as_deref()
        .map(|path| {
            format!(
                "no usable extension restore receipt for {}; reinstall Devin 3.11.216 if the \
                 extension was changed manually",
                path.display()
            )
        })
        .unwrap_or_else(|| "Devin extension.js was not found".into())
}

#[derive(Debug, Serialize)]
pub struct ModelUidsResponse {
    pub path: PathBuf,
    pub choices: Vec<model_uid_table::ModelChoice>,
}

/// The identifiers Devin itself will request. The extension bundle remains the
/// source even though its endpoint patch is no longer used.
pub async fn model_uids(Query(input): Query<HostPathInput>) -> Result<Json<ModelUidsResponse>> {
    let install_input = input.install_root;
    Ok(Json(
        on_blocking_pool(move || {
            let path = resolve_extension_path(install_input.as_deref())?
                .ok_or_else(|| Error::Config(host_detect::detect().explanation()))?;
            let byok = model_uid_table::read(&path)?;
            Ok(ModelUidsResponse {
                path: path.to_owned(),
                choices: model_uid_table::choices(&byok),
            })
        })
        .await?,
    ))
}

pub async fn host_apply(
    State(service): State<ControlService>,
    Json(input): Json<HostPathInput>,
) -> Result<Json<HostApplyResult>> {
    let settings = service.devin_settings().await?;
    let ports = host_config_precheck(&settings, service.devin_bound_ports().bound_ports())?;
    ensure_devin_closed().await?;
    let settings_input = input.path;
    let install_input = input.install_root;
    Ok(Json(
        on_blocking_pool(move || {
            let path = resolve_settings_path(settings_input.as_deref())?;
            let extension = resolve_extension_path(install_input.as_deref())?;
            apply_host(&path, extension.as_deref(), ports)
        })
        .await?,
    ))
}

/// 恢复只用 apply 时落盘的收据。进程重启后页面拿不到旧收据，仍然可以恢复。
pub async fn host_restore(Json(input): Json<HostPathInput>) -> Result<Json<serde_json::Value>> {
    ensure_devin_closed().await?;
    let settings_input = input.path;
    let install_input = input.install_root;
    Ok(Json(
        on_blocking_pool(move || {
            let path = resolve_settings_path(settings_input.as_deref())?;
            let extension = resolve_extension_path(install_input.as_deref())?;
            restore_host(&path, extension.as_deref())?;
            Ok(serde_json::json!({"restored": true}))
        })
        .await?,
    ))
}

async fn ensure_devin_closed() -> Result<()> {
    if devin_is_running().await? {
        return Err(Error::Config(
            "Devin is still running; quit Devin completely before changing its settings. The file \
             was left unchanged"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
async fn devin_is_running() -> Result<bool> {
    let output = tokio::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Devin.exe", "/FO", "CSV", "/NH"])
        .output()
        .await
        .map_err(|error| {
            Error::Config(format!(
                "cannot check whether Devin is running: {error}; quit Devin and try again"
            ))
        })?;
    if !output.status.success() {
        return Err(Error::Config(format!(
            "cannot check whether Devin is running: tasklist exited with {}",
            output.status
        )));
    }
    Ok(tasklist_contains_devin(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[cfg(windows)]
fn tasklist_contains_devin(output: &str) -> bool {
    output.lines().any(|line| {
        line.split(',')
            .next()
            .map(|name| name.trim().trim_matches('"'))
            .is_some_and(|name| name.eq_ignore_ascii_case("Devin.exe"))
    })
}

#[cfg(unix)]
async fn devin_is_running() -> Result<bool> {
    let status = tokio::process::Command::new("pgrep")
        .args(["-x", "Devin"])
        .status()
        .await
        .map_err(|error| {
            Error::Config(format!(
                "cannot check whether Devin is running: {error}; quit Devin and try again"
            ))
        })?;
    Ok(status.success())
}

fn restore_refusal_message(path: &Path, status: &SettingsHostStatus) -> String {
    let backup = status.backup_path.display();
    let hint = if status.backup_available {
        format!(
            "copy the kept previous file {backup} over {} after closing Devin",
            path.display()
        )
    } else if status.configured {
        format!(
            "remove {} and {} from the file after closing Devin",
            host_config::API_SERVER_KEY,
            host_config::INFERENCE_API_SERVER_KEY
        )
    } else {
        "nothing was configured through this application for it".into()
    };
    format!(
        "no usable Devin settings restore receipt for {} ({hint}); the file was left unchanged; \
         backup: {backup}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn host_io_runs_on_a_blocking_thread() {
        let async_thread = std::thread::current().id();
        let io_thread = on_blocking_pool(|| Ok(std::thread::current().id()))
            .await
            .unwrap();
        assert_ne!(io_thread, async_thread);
    }

    #[test]
    fn configuration_requires_an_enabled_gateway_on_the_same_ports() {
        let error = host_config_precheck(&DevinSettings::default(), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("enable the Devin gateway"), "{error}");

        let enabled = DevinSettings {
            enabled: true,
            ..DevinSettings::default()
        };
        let error = host_config_precheck(&enabled, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not listening"), "{error}");

        let listening = DevinBoundPorts {
            api: crate::devin::DEFAULT_DEVIN_API_PORT,
            inference: crate::devin::DEFAULT_DEVIN_INFERENCE_PORT,
            local_api: crate::devin::DEFAULT_DEVIN_LOCAL_API_PORT,
        };
        assert_eq!(
            host_config_precheck(&enabled, Some(listening)).unwrap(),
            DevinHostPorts {
                api_port: crate::devin::DEFAULT_DEVIN_API_PORT,
                inference_port: crate::devin::DEFAULT_DEVIN_INFERENCE_PORT,
            }
        );
    }

    #[test]
    fn status_reads_a_real_settings_file_without_touching_an_installed_client() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        std::fs::write(&path, "{\n}\n").unwrap();
        host_config::apply(
            &path,
            DevinHostPorts {
                api_port: 43_110,
                inference_port: 43_111,
            },
        )
        .unwrap();

        let status = inspect_host(&path, None).unwrap();
        assert!(status.settings.configured);
        assert!(status.settings.restore_receipt.is_some());
        assert!(status.extension.path.is_none());
    }

    #[test]
    fn a_pasted_data_directory_resolves_to_user_settings() {
        let directory = tempfile::tempdir().unwrap();
        let user = directory.path().join("User");
        std::fs::create_dir(&user).unwrap();
        let expected = user.join("settings.json");

        assert_eq!(
            explicit_settings_path(directory.path().to_str().unwrap()).unwrap(),
            expected
        );
        assert_eq!(
            explicit_settings_path(user.to_str().unwrap()).unwrap(),
            expected
        );
        std::fs::write(&expected, "{}").unwrap();
        assert_eq!(
            explicit_settings_path(expected.to_str().unwrap()).unwrap(),
            expected
        );
    }

    #[cfg(windows)]
    #[test]
    fn tasklist_csv_is_parsed_without_matching_the_no_tasks_message() {
        assert!(tasklist_contains_devin(
            r#""Devin.exe","1234","Console","1","100,000 K""#,
        ));
        assert!(!tasklist_contains_devin(
            "INFO: No tasks are running which match the specified criteria.\r\n"
        ));
        assert!(!tasklist_contains_devin(
            r#""DevinHelper.exe","1234","Console","1","10 K""#,
        ));
    }

    #[test]
    fn restore_needs_only_the_settings_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let original = "{\n  // keep\n}\n";
        std::fs::write(&path, original).unwrap();
        host_config::apply(
            &path,
            DevinHostPorts {
                api_port: 43_110,
                inference_port: 43_111,
            },
        )
        .unwrap();

        restore_host(&path, None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn restore_without_a_receipt_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        std::fs::write(&path, "{\n}\n").unwrap();

        let error = restore_host(&path, None).unwrap_err().to_string();
        assert!(error.contains("restore receipt"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\n}\n");
    }
}
