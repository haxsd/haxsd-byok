//! Devin router settings and explicit host integration endpoints.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use super::{ports::ensure_devin_ports_usable, ControlService, DevinStatus};
use crate::{
    devin::{
        gateway::{ensure_host_ports_listening, DevinBoundPorts, DevinListening},
        host_detect,
        host_patch::{self, DevinPorts, PatchReceipt, PatchStatus},
        host_status as host_status_module, model_uids as model_uid_table, DevinSettings,
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
    let listening = state.listening.is_listening();
    Ok(Json(state.service.devin_status(listening).await?))
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
    settings.validate()?;
    ensure_devin_ports_usable(store, &settings, gateway_ports).await?;
    store.set_devin_settings(settings).await
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
#[serde(deny_unknown_fields)]
pub struct HostRestoreInput {
    /// Omitted means "find the installation yourself".
    #[serde(default)]
    pub path: Option<String>,
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

/// 宿主文件的探测与读写都是同步 I/O（最坏要扫 3000 个目录、读写几十 MB 的宿主
/// 文件），而这里是 async handler。把它放进 `spawn_blocking`：不占 tokio 的 worker，
/// 同一 runtime 上的推理流量与管理接口继续跑。
///
/// 闭包必须是 `Send + 'static`，"顺手在里面 await 一下"是编译不过的。
async fn on_blocking_pool<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| Error::Config(format!("Devin host task did not finish: {error}")))?
}

/// 应用宿主补丁之前的准入：启用状态，以及网关是否真的在设置里那三个端口上监听。
/// 抽成函数是为了能直接断言"没在监听就拒绝"，不必为了这一条起一个真的 ControlService。
fn host_apply_precheck(
    settings: &DevinSettings,
    bound: Option<DevinBoundPorts>,
) -> Result<DevinPorts> {
    if !settings.enabled {
        return Err(Error::Config(
            "enabling the Devin gateway is required before applying a host patch".into(),
        ));
    }
    let ports = DevinPorts {
        api_port: settings.api_port,
        inference_port: settings.inference_port,
        local_api_port: settings.local_api_port,
    };
    ensure_host_ports_listening(ports, bound)?;
    Ok(ports)
}

pub async fn host_status(Query(query): Query<HostPathQuery>) -> Result<Json<PatchStatus>> {
    let input = query.path;
    Ok(Json(
        on_blocking_pool(move || {
            let path = resolve_host_path(input.as_deref())?;
            host_status_module::status(&path)
        })
        .await?,
    ))
}

#[derive(Debug, Serialize)]
pub struct ModelUidsResponse {
    pub path: PathBuf,
    /// 选择器里的模型（名字 + 客户端随后请求的标识），界面直接拿它做选项。
    pub choices: Vec<model_uid_table::ModelChoice>,
}

/// The identifiers Devin itself will ask for. Without this the page can only
/// invite the user to invent one, and an invented UID is never requested.
pub async fn model_uids(Query(query): Query<HostPathQuery>) -> Result<Json<ModelUidsResponse>> {
    let input = query.path;
    Ok(Json(
        on_blocking_pool(move || {
            let path = resolve_host_path(input.as_deref())?;
            let byok = model_uid_table::read(&path)?;
            Ok(ModelUidsResponse {
                path,
                choices: model_uid_table::choices(&byok),
            })
        })
        .await?,
    ))
}

pub async fn host_apply(
    State(service): State<ControlService>,
    Json(input): Json<HostPatchInput>,
) -> Result<Json<PatchReceipt>> {
    let settings = service.devin_settings().await?;
    let ports = host_apply_precheck(&settings, service.devin_bound_ports().bound_ports())?;
    let input = input.path;
    Ok(Json(
        on_blocking_pool(move || {
            let path = resolve_host_path(input.as_deref())?;
            host_patch::apply(&path, ports)
        })
        .await?,
    ))
}

/// 恢复只用 apply 时落盘的原始收据：调用方（页面）拿不到、也不该拿收据，
/// 进程重启后这一点尤其重要。收据缺失或被改动时拒绝并说明下一步。
pub async fn host_restore(Json(input): Json<HostRestoreInput>) -> Result<Json<serde_json::Value>> {
    let input = input.path;
    Ok(Json(
        on_blocking_pool(move || {
            let path = resolve_host_path(input.as_deref())?;
            let status = host_status_module::status(&path)?;
            let refusal = restore_refusal_message(&path, &status);
            let receipt = status
                .restore_receipt
                .ok_or_else(|| Error::Config(refusal))?;
            host_patch::restore(&receipt)?;
            Ok(serde_json::json!({"restored": true}))
        })
        .await?,
    ))
}

/// 收据不可用时的说法。
///
/// 三种情况分开说，每一种都带两样东西：句末固定的 `backup: <path>`（界面
/// `apps/desktop` 的 `harnessErrorText.ts` 按它把路径留在提示里），以及用户下一步该做
/// 什么——宿主文件被改过但备份还在就"把备份复制回去"，备份也没了就"重装 Devin 客户端"，
/// 本来不是本应用打的补丁就没有可恢复的东西。复核 12-devin/P3-2：以前这段指引挂在
/// 收据校验之后才到达的分支上，实际走不到。
fn restore_refusal_message(path: &Path, status: &PatchStatus) -> String {
    let backup = status.backup_path.display();
    let hint = if status.patched && status.backup_available {
        format!(
            "copy the kept clean version {backup} over {} and restart the Devin client",
            path.display()
        )
    } else if status.patched {
        format!(
            "the backup {backup} is unavailable or does not match this installation; restore the \
             clean version by reinstalling the Devin client"
        )
    } else {
        "nothing was patched through this app for it".into()
    };
    format!(
        "no usable Devin host restore receipt for {} ({hint}); the file was left unchanged; \
         backup: {backup}",
        path.display()
    )
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
    // 这一句直接进界面提示：说清"这里没有找到"，以及两条能走的下一步——留空自动
    // 探测，或改成安装目录。安装目录内部的相对路径（`resources\app\...`）只对读代码
    // 的人有意义，对用户只是噪声。
    //
    // "is not a Devin host file" 是界面的匹配片段（`apps/desktop` 的
    // `harnessErrorText.ts` 按它把这句话换成中文），改文案要连着那张表一起改。
    Err(Error::Config(format!(
        "{} is not a Devin host file; leave the path empty to detect the installation \
         automatically, or select the installation directory that contains extension.js",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 宿主读写必须在阻塞线程池里跑：留在 async worker 上时，一次最坏 3000 个目录
    /// 的探测或几十 MB 的宿主文件读写会把同一 runtime 上的推理流量一起拖住。
    #[tokio::test]
    async fn host_io_runs_on_a_blocking_thread_instead_of_the_async_one() {
        let async_thread = std::thread::current().id();
        let io_thread = on_blocking_pool(|| Ok(std::thread::current().id()))
            .await
            .unwrap();
        assert_ne!(
            io_thread, async_thread,
            "同步宿主 I/O 不能跑在 async worker 上"
        );
    }

    /// 补丁把客户端指向设置里那三个端口，所以在写宿主文件之前必须两道都过：网关已启用，
    /// 且真的在那三个端口上监听。没在监听时拒绝——否则客户端从下一个请求开始全部打空。
    #[test]
    fn host_apply_is_refused_before_the_host_file_is_touched_when_the_gateway_is_not_ready() {
        let error = host_apply_precheck(&DevinSettings::default(), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("enabling the Devin gateway is required"),
            "unexpected message: {error}"
        );

        let enabled = DevinSettings {
            enabled: true,
            ..DevinSettings::default()
        };
        let error = host_apply_precheck(&enabled, None).unwrap_err().to_string();
        assert!(
            error.contains("not listening"),
            "unexpected message: {error}"
        );

        let drifted = DevinBoundPorts {
            api: crate::devin::DEFAULT_DEVIN_API_PORT + 10,
            inference: crate::devin::DEFAULT_DEVIN_INFERENCE_PORT,
            local_api: crate::devin::DEFAULT_DEVIN_LOCAL_API_PORT,
        };
        let error = host_apply_precheck(&enabled, Some(drifted))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("listening on") && error.contains("restart haxsd byok"),
            "端口漂移时必须拒绝并说清怎么办: {error}"
        );

        let listening = DevinBoundPorts {
            api: crate::devin::DEFAULT_DEVIN_API_PORT,
            inference: crate::devin::DEFAULT_DEVIN_INFERENCE_PORT,
            local_api: crate::devin::DEFAULT_DEVIN_LOCAL_API_PORT,
        };
        assert_eq!(
            host_apply_precheck(&enabled, Some(listening)).unwrap(),
            DevinPorts {
                api_port: crate::devin::DEFAULT_DEVIN_API_PORT,
                inference_port: crate::devin::DEFAULT_DEVIN_INFERENCE_PORT,
                local_api_port: crate::devin::DEFAULT_DEVIN_LOCAL_API_PORT,
            }
        );
    }

    /// 走一遍带 `spawn_blocking` 的 handler：状态查询照常给出宿主文件的状态。
    #[tokio::test]
    async fn host_status_reports_the_patch_state_from_the_blocking_path() {
        let directory = tempfile::tempdir().unwrap();
        let host = directory.path().join("extension.js");
        std::fs::write(&host, host_patch::test_fixture::HOST_FILE).unwrap();
        host_patch::apply(&host, host_patch::test_fixture::ports()).unwrap();

        let response = host_status(Query(HostPathQuery {
            path: Some(host.to_str().unwrap().to_owned()),
        }))
        .await
        .unwrap();
        assert!(response.0.patched, "状态必须来自真实的宿主文件");
        assert!(response.0.restore_receipt.is_some());
    }

    /// 只改保存这一步：端口被别的程序占着时拒绝，并保证库里没有留下半份设置。
    /// 这是用户视角的入口——`PUT /__byok-api__/api/devin/settings` 走的就是它。
    #[tokio::test]
    async fn a_taken_port_rejects_the_save_and_leaves_the_stored_settings_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();

        let error = save_settings(
            &store,
            DevinSettings {
                enabled: true,
                api_port: occupied_port,
                ..DevinSettings::default()
            },
            None,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(
            error.contains("another program") && error.contains(&occupied_port.to_string()),
            "the refusal must name the occupier and the port: {error}"
        );
        assert_eq!(
            store.devin_settings().await.unwrap(),
            DevinSettings::default(),
            "a refused save must not leave a half-written row behind"
        );
    }

    /// 端口空闲时保存照常成功，并且回读到的就是刚存的那份。
    #[tokio::test]
    async fn free_ports_are_saved_and_read_back() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let mut ports = Vec::new();
        for _ in 0..3 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            ports.push(listener.local_addr().unwrap().port());
            drop(listener);
        }
        let settings = DevinSettings {
            enabled: true,
            api_port: ports[0],
            inference_port: ports[1],
            local_api_port: ports[2],
            ..DevinSettings::default()
        };

        assert_eq!(
            save_settings(&store, settings.clone(), None).await.unwrap(),
            settings
        );
        assert_eq!(store.devin_settings().await.unwrap(), settings);
    }

    #[test]
    fn obsolete_restore_receipt_is_rejected_instead_of_selecting_another_host() {
        assert!(
            serde_json::from_value::<HostRestoreInput>(serde_json::json!({
                "receipt": { "path": "D:/other-installation/extension.js" }
            }))
            .is_err()
        );
    }

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

    /// 复核 12-devin/P3-2：还原指引必须落在真正会失败的路径上。控制接口这边——收据
    /// 不可用（宿主文件被改过、备份丢失）时，说法里要同时有备份路径与下一步动作。
    #[test]
    fn a_refused_restore_names_the_backup_and_the_next_step() {
        fn status(backup_available: bool) -> PatchStatus {
            PatchStatus {
                path: PathBuf::from("C:/Devin/extension.js"),
                compatible: true,
                clean: false,
                patched: true,
                parts: Default::default(),
                ports: None,
                backup_path: PathBuf::from("C:/Devin/extension.js.orig"),
                backup_available,
                current_sha256: String::new(),
                backup_sha256: None,
                restore_receipt: None,
                message: String::new(),
            }
        }

        let usable = status(true);
        let message = restore_refusal_message(&usable.path, &usable);
        assert!(
            message.contains("extension.js.orig"),
            "要给出备份路径: {message}"
        );
        assert!(message.contains("copy"), "要给出下一步动作: {message}");
        assert!(message.contains("backup:"), "界面按它取路径: {message}");

        let missing = status(false);
        let message = restore_refusal_message(&missing.path, &missing);
        assert!(
            message.contains("extension.js.orig"),
            "备份没了也要指认它: {message}"
        );
        assert!(
            message.contains("reinstall"),
            "备份也不可用时要给重装这一步: {message}"
        );
        assert!(message.contains("backup:"), "界面按它取路径: {message}");
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

    /// 恢复只凭宿主路径：页面持有的 apply 返回值在进程重启后就没了。
    #[tokio::test]
    async fn a_restore_request_needs_only_the_host_path() {
        let directory = tempfile::tempdir().unwrap();
        let host = directory.path().join("extension.js");
        std::fs::write(&host, host_patch::test_fixture::HOST_FILE).unwrap();
        host_patch::apply(&host, host_patch::test_fixture::ports()).unwrap();

        let input: HostRestoreInput =
            serde_json::from_value(serde_json::json!({ "path": host.to_str().unwrap() }))
                .expect("a restore request must be satisfiable with the host path alone");
        let _ = host_restore(Json(input)).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(&host).unwrap(),
            host_patch::test_fixture::HOST_FILE
        );
    }

    /// 没有可用收据时拒绝恢复，并说明下一步，而不是从当前哈希猜造一份。
    #[tokio::test]
    async fn a_restore_request_without_a_usable_receipt_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let host = directory.path().join("extension.js");
        std::fs::write(&host, host_patch::test_fixture::HOST_FILE).unwrap();

        let input: HostRestoreInput =
            serde_json::from_value(serde_json::json!({ "path": host.to_str().unwrap() })).unwrap();
        let error = host_restore(Json(input)).await.unwrap_err();

        assert!(
            error.to_string().contains("receipt"),
            "unexpected message: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&host).unwrap(),
            host_patch::test_fixture::HOST_FILE
        );
    }

    /// 补丁之后被改过的宿主文件不能恢复，用户改动原样保留。
    #[tokio::test]
    async fn a_restore_request_refuses_a_host_file_changed_after_the_patch() {
        let directory = tempfile::tempdir().unwrap();
        let host = directory.path().join("extension.js");
        std::fs::write(&host, host_patch::test_fixture::HOST_FILE).unwrap();
        host_patch::apply(&host, host_patch::test_fixture::ports()).unwrap();
        let edited = format!("{}// user edit\n", std::fs::read_to_string(&host).unwrap());
        std::fs::write(&host, &edited).unwrap();

        let input: HostRestoreInput =
            serde_json::from_value(serde_json::json!({ "path": host.to_str().unwrap() })).unwrap();
        let error = host_restore(Json(input)).await.unwrap_err();

        assert!(
            error.to_string().contains("receipt"),
            "unexpected message: {error}"
        );
        assert_eq!(std::fs::read_to_string(&host).unwrap(), edited);
    }
}
