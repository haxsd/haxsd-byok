//! Claude Desktop 的 3P profile 接管。
//!
//! Claude Desktop 在 `%LOCALAPPDATA%\Claude-3p\configLibrary` 里保存第三方推理配置：
//! `_meta.json` 是条目表与当前 `appliedId`，每个条目对应一个 `<uuid>.json` profile。
//! 本模块只追加/更新自己的条目与 profile，其它条目、profile 与未知字段原样保留。
use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Error, Result};

use super::{
    atomic_write, backup_once, content_hash, parse_json, read_optional, remove_file_if_exists,
    same_path, ConfigSource, DesktopTakeoverStatus, Family, TakeoverConflict, TakeoverState,
    DESKTOP_ENTRY_NAME, RECEIPT_VERSION,
};
use crate::anthropic::models::discovery_id;

const RECEIPT_FILE: &str = "desktop.json";
const ENTRIES_KEY: &str = "entries";
const APPLIED_ID_KEY: &str = "appliedId";

#[derive(Clone, Debug)]
pub struct DesktopPaths {
    pub local_app_data: PathBuf,
    /// `%LOCALAPPDATA%\Claude`：官方 1P 数据目录。
    pub data_dir: PathBuf,
    /// `%LOCALAPPDATA%\Claude-3p`：第三方推理数据目录。
    pub three_p_dir: PathBuf,
    /// `Claude-3p\configLibrary`。
    pub library: PathBuf,
    pub meta: PathBuf,
    /// `Claude\claude_desktop_config.json`：旧版入口配置，写前备份。
    pub legacy_config: PathBuf,
    pub installed: bool,
}

impl DesktopPaths {
    pub fn discover() -> Result<Self> {
        Ok(Self::at_local_app_data(local_app_data_root()?))
    }

    /// 注入用的构造：调用方给出 `%LOCALAPPDATA%` 根目录。
    pub fn at_local_app_data(root: PathBuf) -> Self {
        let data_dir = root.join("Claude");
        let three_p_dir = root.join("Claude-3p");
        let library = three_p_dir.join("configLibrary");
        let meta = library.join("_meta.json");
        let legacy_config = data_dir.join("claude_desktop_config.json");
        let installed = data_dir.is_dir() || three_p_dir.is_dir();
        Self {
            local_app_data: root,
            data_dir,
            three_p_dir,
            library,
            meta,
            legacy_config,
            installed,
        }
    }

    pub fn profile(&self, uuid: &str) -> PathBuf {
        self.library.join(uuid.to_owned() + ".json")
    }
}

#[derive(Clone, Debug)]
pub struct DesktopModel {
    pub model_hash: String,
    pub label: Option<String>,
    pub tier: Option<Family>,
}

#[derive(Clone, Debug)]
pub struct DesktopRoute {
    pub base_url: String,
    pub token: String,
    pub models: Vec<DesktopModel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    version: u32,
    uuid: String,
    meta_path: String,
    profile_path: String,
    /// `_meta.json` 原本有没有 `appliedId`；没有时撤销要删掉这个键。
    applied_id_present: bool,
    /// 原本的 `appliedId`（可能是 null）。
    #[serde(default)]
    original_applied_id: Option<Value>,
    applied_profile_hash: String,
    applied_meta_hash: String,
}

pub fn inspect(
    paths: &DesktopPaths,
    state_dir: &Path,
    policy: &super::ownership::DesktopPolicy,
    running: bool,
) -> Result<DesktopTakeoverStatus> {
    let config_source = if policy.machine() {
        ConfigSource::MachinePolicy
    } else if policy.user() {
        ConfigSource::UserPolicy
    } else if paths.installed {
        ConfigSource::Local
    } else {
        ConfigSource::None
    };
    let guided = policy.machine() || policy.user();
    let mut conflicts = Vec::new();
    if policy.machine() {
        conflicts.push(TakeoverConflict::new(
            "machine_policy",
            "Claude Desktop is managed by HKLM\\SOFTWARE\\Policies\\Claude; profile settings are ignored",
            true,
        ));
        if policy.user() {
            conflicts.push(TakeoverConflict::new(
                "user_policy_overridden",
                "HKCU\\SOFTWARE\\Policies\\Claude is also set, but the machine policy takes precedence",
                false,
            ));
        }
    } else if policy.user() {
        conflicts.push(TakeoverConflict::new(
            "user_policy",
            "Claude Desktop has HKCU\\SOFTWARE\\Policies\\Claude settings; remove them before applying a profile",
            true,
        ));
    }

    let receipt = match read_receipt(state_dir) {
        Ok(receipt) => receipt,
        Err(error) => {
            conflicts.push(TakeoverConflict::new(
                "receipt_unreadable",
                format!("cannot read the Claude Desktop takeover receipt: {error}"),
                true,
            ));
            None
        }
    };
    let profile_path = receipt
        .as_ref()
        .map(|receipt| receipt.profile_path.clone())
        .unwrap_or_default();
    let meta_backup = super::backup_path(&paths.meta);
    let backup_path = if meta_backup.is_file() {
        meta_backup.display().to_string()
    } else {
        String::new()
    };
    let mut profile_present = false;
    let mut profile_matches = false;
    let mut receipt_mismatch = false;
    if let Some(receipt) = &receipt {
        if !same_path(Path::new(&receipt.meta_path), &paths.meta) {
            receipt_mismatch = true;
        }
        let profile = Path::new(&receipt.profile_path);
        if profile
            .parent()
            .is_some_and(|parent| same_path(parent, &paths.library))
        {
            if let Some(text) = read_optional(profile)? {
                profile_present = true;
                profile_matches = content_hash(&text) == receipt.applied_profile_hash;
            }
        } else {
            receipt_mismatch = true;
        }
    }
    if receipt_mismatch {
        conflicts.push(TakeoverConflict::new(
            "receipt_mismatch",
            "the Claude Desktop takeover receipt belongs to another installation; undo will not touch this one",
            true,
        ));
    }

    let meta_raw = read_optional(&paths.meta)?;
    let mut meta_broken = false;
    let meta = match meta_raw.as_deref() {
        Some(text) => match parse_json(text) {
            Ok(meta) => Some(meta),
            Err(error) => {
                meta_broken = true;
                conflicts.push(TakeoverConflict::new(
                    "meta_invalid",
                    format!("cannot parse {}: {error}", paths.meta.display()),
                    true,
                ));
                None
            }
        },
        None => None,
    };
    let (entry_present, applied_id_matches) = match (&receipt, &meta) {
        (Some(receipt), Some(meta)) => (
            meta_has_entry(meta, &receipt.uuid),
            meta.get(APPLIED_ID_KEY).and_then(Value::as_str) == Some(receipt.uuid.as_str()),
        ),
        _ => (false, false),
    };
    if receipt.is_some() && !receipt_mismatch && !meta_broken {
        match &meta {
            Some(_) => {
                if !applied_id_matches {
                    conflicts.push(TakeoverConflict::new(
                        "applied_id_changed",
                        "appliedId in _meta.json no longer points at this takeover",
                        false,
                    ));
                }
                if !entry_present {
                    conflicts.push(TakeoverConflict::new(
                        "entry_missing",
                        "the configLibrary entry for this takeover is missing; apply again",
                        false,
                    ));
                }
            }
            None => conflicts.push(TakeoverConflict::new(
                "meta_missing",
                "_meta.json is missing; apply again to recreate the entry",
                false,
            )),
        }
        if !profile_present {
            conflicts.push(TakeoverConflict::new(
                "profile_missing",
                "the gateway profile is missing; apply again",
                false,
            ));
        } else if !profile_matches {
            conflicts.push(TakeoverConflict::new(
                "profile_modified",
                "the gateway profile was modified after the takeover; applying would overwrite it",
                true,
            ));
        }
    }
    if running {
        conflicts.push(TakeoverConflict::new(
            "desktop_running",
            "quit Claude Desktop before changing its configuration",
            true,
        ));
    }

    let applied = receipt.is_some()
        && !meta_broken
        && !receipt_mismatch
        && entry_present
        && applied_id_matches
        && profile_matches;
    let state = if policy.machine() {
        TakeoverState::Managed
    } else if policy.user()
        || meta_broken
        || receipt_mismatch
        || conflicts
            .iter()
            .any(|conflict| conflict.code == "profile_modified")
    {
        TakeoverState::Conflict
    } else if !paths.installed {
        TakeoverState::Missing
    } else if running {
        TakeoverState::Running
    } else if receipt.is_some() {
        if applied {
            TakeoverState::Applied
        } else {
            TakeoverState::Conflict
        }
    } else {
        TakeoverState::Ready
    };

    Ok(DesktopTakeoverStatus {
        state,
        config_source,
        profile_path,
        backup_path,
        restorable: receipt.is_some() && !receipt_mismatch && !meta_broken,
        conflicts,
        guided,
    })
}

pub fn apply(
    paths: &DesktopPaths,
    state_dir: &Path,
    policy: &super::ownership::DesktopPolicy,
    running: bool,
    route: &DesktopRoute,
) -> Result<DesktopTakeoverStatus> {
    if !paths.installed {
        return Err(Error::Config(
            "Claude Desktop is not installed; install it and sign in once before applying".into(),
        ));
    }
    if policy.machine() {
        return Err(Error::Config(
            "Claude Desktop is managed by HKLM\\SOFTWARE\\Policies\\Claude; remove the policy before applying"
                .into(),
        ));
    }
    if policy.user() {
        return Err(Error::Config(
            "Claude Desktop has HKCU\\SOFTWARE\\Policies\\Claude settings; remove them before applying"
                .into(),
        ));
    }
    if running {
        return Err(Error::Config(
            "quit Claude Desktop before applying the gateway profile".into(),
        ));
    }
    if route.models.is_empty() {
        return Err(Error::Config(
            "select at least one model for Claude Desktop".into(),
        ));
    }
    validate_route(route)?;

    let previous = read_receipt(state_dir)?;
    if let Some(receipt) = &previous {
        if !same_path(Path::new(&receipt.meta_path), &paths.meta) {
            return Err(Error::Config(format!(
                "the Claude Desktop takeover receipt belongs to {}; undo will not touch this installation",
                receipt.meta_path
            )));
        }
    }
    let meta_raw = read_optional(&paths.meta)?;
    let mut meta = match meta_raw.as_deref() {
        Some(text) => parse_json(text).map_err(|error| {
            Error::Config(format!("cannot parse {}: {error}", paths.meta.display()))
        })?,
        None => Value::Object(Map::new()),
    };
    validate_meta(&meta, &paths.meta)?;
    let uuid = previous
        .as_ref()
        .map(|receipt| receipt.uuid.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let profile_path = paths.profile(&uuid);
    if let Some(receipt) = &previous {
        if let Some(text) = read_optional(&profile_path)? {
            if content_hash(&text) != receipt.applied_profile_hash {
                return Err(Error::Config(
                    "the gateway profile was modified after the takeover; undo before applying again"
                        .into(),
                ));
            }
        }
    }
    let (applied_id_present, original_applied_id) = match &previous {
        Some(receipt) => (
            receipt.applied_id_present,
            receipt.original_applied_id.clone(),
        ),
        None => {
            let object = meta
                .as_object()
                .ok_or_else(|| Error::Config("_meta.json must be a JSON object".into()))?;
            (
                object.contains_key(APPLIED_ID_KEY),
                object.get(APPLIED_ID_KEY).cloned(),
            )
        }
    };

    // 备份只留一次：_meta.json、目标 profile、旧版入口配置。
    backup_once(&paths.meta)?;
    backup_once(&profile_path)?;
    backup_once(&paths.legacy_config)?;

    let profile = profile_value(route)?;
    let profile_text = pretty(&profile)?;
    update_meta(&mut meta, &uuid)?;
    let meta_text = pretty(&meta)?;

    fs::create_dir_all(&paths.library)?;
    let receipt = Receipt {
        version: RECEIPT_VERSION,
        uuid,
        meta_path: paths.meta.display().to_string(),
        profile_path: profile_path.display().to_string(),
        applied_id_present,
        original_applied_id,
        applied_profile_hash: content_hash(&profile_text),
        applied_meta_hash: content_hash(&meta_text),
    };
    atomic_write(
        &state_dir.join(RECEIPT_FILE),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    if read_optional(&paths.meta)? != meta_raw {
        return Err(Error::Config(
            "_meta.json changed while preparing the update".into(),
        ));
    }
    atomic_write(&profile_path, profile_text.as_bytes())?;
    if read_optional(&paths.meta)? != meta_raw {
        return Err(Error::Config(
            "_meta.json changed before replacement; restore receipt retained".into(),
        ));
    }
    atomic_write(&paths.meta, meta_text.as_bytes())?;
    inspect(paths, state_dir, policy, false)
}

pub fn undo(
    paths: &DesktopPaths,
    state_dir: &Path,
    policy: &super::ownership::DesktopPolicy,
    running: bool,
) -> Result<DesktopTakeoverStatus> {
    let Some(receipt) = read_receipt(state_dir)? else {
        return inspect(paths, state_dir, policy, running);
    };
    // 与 apply 同一把锁：收据必须属于这套安装，否则撤销会碰到别处的文件。
    if !same_path(Path::new(&receipt.meta_path), &paths.meta) {
        return Err(Error::Config(format!(
            "the Claude Desktop takeover receipt belongs to {}; undo will not touch this installation",
            receipt.meta_path
        )));
    }
    let profile = Path::new(&receipt.profile_path);
    if !profile
        .parent()
        .is_some_and(|parent| same_path(parent, &paths.library))
    {
        return Err(Error::Config(
            "the Claude Desktop takeover receipt points outside the configLibrary; undo will not touch it"
                .into(),
        ));
    }
    if running {
        return Err(Error::Config(
            "quit Claude Desktop before restoring its native configuration".into(),
        ));
    }

    let mut kept = Vec::new();
    // profile 被用户改过就不再是我们的文件：保留它和条目，只报告，不静默删除。
    if let Some(text) = read_optional(profile)? {
        if content_hash(&text) != receipt.applied_profile_hash {
            kept.push(TakeoverConflict::new(
                "undo_kept_profile",
                "the gateway profile changed after the takeover; undo kept it and the configLibrary entry",
                false,
            ));
            let mut status = inspect(paths, state_dir, policy, false)?;
            status.conflicts.extend(kept);
            return Ok(status);
        }
    }

    let meta_raw = read_optional(&paths.meta)?;
    if let Some(text) = meta_raw.as_deref() {
        let mut meta = parse_json(text).map_err(|error| {
            Error::Config(format!("cannot parse {}: {error}", paths.meta.display()))
        })?;
        let Some(object) = meta.as_object_mut() else {
            return Err(Error::Config("_meta.json must be a JSON object".into()));
        };
        let mut changed = false;
        if let Some(entries) = object.get_mut(ENTRIES_KEY).and_then(Value::as_array_mut) {
            let before = entries.len();
            entries.retain(|entry| {
                entry.get("id").and_then(Value::as_str) != Some(receipt.uuid.as_str())
            });
            changed |= entries.len() != before;
        }
        let current_applied = object
            .get(APPLIED_ID_KEY)
            .and_then(Value::as_str)
            .map(str::to_owned);
        if current_applied.as_deref() == Some(receipt.uuid.as_str()) {
            if receipt.applied_id_present {
                object.insert(
                    APPLIED_ID_KEY.to_owned(),
                    receipt.original_applied_id.clone().unwrap_or(Value::Null),
                );
            } else {
                object.remove(APPLIED_ID_KEY);
            }
            changed = true;
        } else if current_applied.is_some() {
            kept.push(TakeoverConflict::new(
                "applied_id_changed",
                "appliedId now points at another entry; it was left unchanged",
                false,
            ));
        }
        if changed {
            let updated = pretty(&meta)?;
            if read_optional(&paths.meta)? != meta_raw {
                return Err(Error::Config(
                    "_meta.json changed during the restore".into(),
                ));
            }
            atomic_write(&paths.meta, updated.as_bytes())?;
        }
    }

    remove_file_if_exists(profile)?;
    remove_file_if_exists(&state_dir.join(RECEIPT_FILE))?;
    let mut status = inspect(paths, state_dir, policy, false)?;
    status.conflicts.extend(kept);
    Ok(status)
}

/// 只读查询 `claude.exe` 是否在运行；不结束进程。
pub async fn process_running() -> bool {
    #[cfg(windows)]
    {
        use tokio::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = Command::new("tasklist");
        command.creation_flags(CREATE_NO_WINDOW);
        match command
            .args(["/FI", "IMAGENAME eq claude.exe", "/NH", "/FO", "CSV"])
            .output()
            .await
        {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("claude.exe"),
            Ok(output) => {
                tracing::warn!(status = %output.status, "cannot inspect the Claude Desktop process");
                false
            }
            Err(error) => {
                tracing::warn!(%error, "cannot inspect the Claude Desktop process");
                false
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        match tokio::process::Command::new("pgrep")
            .args(["-x", "Claude"])
            .status()
            .await
        {
            Ok(status) => status.success(),
            Err(error) => {
                tracing::warn!(%error, "cannot inspect the Claude Desktop process");
                false
            }
        }
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        false
    }
}

fn local_app_data_root() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(root) = std::env::var_os("LOCALAPPDATA") {
            Ok(PathBuf::from(root))
        } else {
            let home = dirs::home_dir()
                .ok_or_else(|| Error::Config("cannot resolve the user home directory".into()))?;
            Ok(home.join("AppData").join("Local"))
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Config("cannot resolve the user home directory".into()))?;
        Ok(home.join("Library").join("Application Support"))
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        dirs::data_local_dir().ok_or_else(|| {
            Error::Config("cannot resolve the local application data directory".into())
        })
    }
}

fn read_receipt(state_dir: &Path) -> Result<Option<Receipt>> {
    let Some(text) = read_optional(&state_dir.join(RECEIPT_FILE))? else {
        return Ok(None);
    };
    let receipt: Receipt = serde_json::from_str(&text)?;
    if receipt.version != RECEIPT_VERSION {
        return Err(Error::Config(
            "unsupported Claude Desktop takeover receipt".into(),
        ));
    }
    Ok(Some(receipt))
}

fn validate_meta(meta: &Value, path: &Path) -> Result<()> {
    let Some(object) = meta.as_object() else {
        return Err(Error::Config(format!(
            "{} must be a JSON object",
            path.display()
        )));
    };
    if object
        .get(ENTRIES_KEY)
        .is_some_and(|entries| !entries.is_array())
    {
        return Err(Error::Config(format!(
            "{} in {} is not an array",
            ENTRIES_KEY,
            path.display()
        )));
    }
    Ok(())
}

fn update_meta(meta: &mut Value, uuid: &str) -> Result<()> {
    let Some(object) = meta.as_object_mut() else {
        return Err(Error::Config("_meta.json must be a JSON object".into()));
    };
    let entries = object
        .entry(ENTRIES_KEY.to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(entries) = entries.as_array_mut() else {
        return Err(Error::Config(
            "entries in _meta.json must be an array".into(),
        ));
    };
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(uuid))
    {
        if let Some(entry) = entry.as_object_mut() {
            entry.insert(
                "name".to_owned(),
                Value::String(DESKTOP_ENTRY_NAME.to_owned()),
            );
        } else {
            *entry = serde_json::json!({"id": uuid, "name": DESKTOP_ENTRY_NAME});
        }
    } else {
        entries.push(serde_json::json!({"id": uuid, "name": DESKTOP_ENTRY_NAME}));
    }
    object.insert(APPLIED_ID_KEY.to_owned(), Value::String(uuid.to_owned()));
    Ok(())
}

fn meta_has_entry(meta: &Value, uuid: &str) -> bool {
    meta.get(ENTRIES_KEY)
        .and_then(Value::as_array)
        .is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry.get("id").and_then(Value::as_str) == Some(uuid))
        })
}

fn profile_value(route: &DesktopRoute) -> Result<Value> {
    let models = route
        .models
        .iter()
        .map(|model| {
            let mut entry = Map::new();
            entry.insert(
                "name".to_owned(),
                Value::String(discovery_id(&model.model_hash)),
            );
            if let Some(label) = model
                .label
                .as_deref()
                .map(str::trim)
                .filter(|label| !label.is_empty())
            {
                entry.insert("labelOverride".to_owned(), Value::String(label.to_owned()));
            }
            if let Some(tier) = model.tier {
                entry.insert(
                    "anthropicFamilyTier".to_owned(),
                    Value::String(tier.tier().to_owned()),
                );
            }
            Value::Object(entry)
        })
        .collect::<Vec<_>>();
    // 不写 coworkEgressAllowedHosts：它会把 Desktop 的出网范围放开，与本地网关无关。
    Ok(serde_json::json!({
        "inferenceProvider": "gateway",
        "inferenceGatewayBaseUrl": route.base_url.clone(),
        "inferenceGatewayApiKey": route.token.clone(),
        "inferenceGatewayAuthScheme": "bearer",
        "inferenceModels": models,
        "modelDiscoveryEnabled": false,
        "disableDeploymentModeChooser": true,
        "isClaudeCodeForDesktopEnabled": true,
    }))
}

fn validate_route(route: &DesktopRoute) -> Result<()> {
    let url = url::Url::parse(&route.base_url)
        .map_err(|error| Error::Config(format!("invalid Claude gateway URL: {error}")))?;
    if url.scheme() != "http" || url.host_str() != Some("127.0.0.1") || url.path() != "/" {
        return Err(Error::Config(
            "the Claude gateway URL must be http://127.0.0.1:<port>".into(),
        ));
    }
    if url.port().is_none_or(|port| port == 0) {
        return Err(Error::Config(
            "the Claude gateway URL must include a non-zero port".into(),
        ));
    }
    if route.token.trim().is_empty() {
        return Err(Error::Config("the Claude gateway token is empty".into()));
    }
    Ok(())
}

fn pretty(document: &Value) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(document)?))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::claude_code::ownership::DesktopPolicy;

    struct Fixture {
        _root: tempfile::TempDir,
        state: tempfile::TempDir,
        paths: DesktopPaths,
    }

    impl Fixture {
        fn installed() -> Self {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            fs::create_dir_all(root.path().join("Claude")).unwrap();
            let paths = DesktopPaths::at_local_app_data(root.path().to_path_buf());
            Self {
                _root: root,
                state,
                paths,
            }
        }

        fn missing() -> Self {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let paths = DesktopPaths::at_local_app_data(root.path().to_path_buf());
            Self {
                _root: root,
                state,
                paths,
            }
        }

        fn apply(&self, models: Vec<DesktopModel>) -> DesktopTakeoverStatus {
            apply(
                &self.paths,
                self.state.path(),
                &DesktopPolicy::default(),
                false,
                &route(models),
            )
            .unwrap()
        }

        fn undo(&self) -> DesktopTakeoverStatus {
            undo(
                &self.paths,
                self.state.path(),
                &DesktopPolicy::default(),
                false,
            )
            .unwrap()
        }

        fn meta(&self) -> Value {
            serde_json::from_str(&fs::read_to_string(&self.paths.meta).unwrap()).unwrap()
        }

        fn write_meta(&self, meta: &Value) {
            fs::create_dir_all(&self.paths.library).unwrap();
            fs::write(&self.paths.meta, serde_json::to_vec_pretty(meta).unwrap()).unwrap();
        }

        fn applied_uuid(&self) -> String {
            self.meta()["appliedId"].as_str().unwrap().to_owned()
        }

        fn profile(&self, uuid: &str) -> Value {
            serde_json::from_str(&fs::read_to_string(self.paths.profile(uuid)).unwrap()).unwrap()
        }
    }

    fn model(hash: &str, label: &str, tier: Option<Family>) -> DesktopModel {
        DesktopModel {
            model_hash: hash.into(),
            label: Some(label.into()),
            tier,
        }
    }

    fn route(models: Vec<DesktopModel>) -> DesktopRoute {
        DesktopRoute {
            base_url: "http://127.0.0.1:43121".into(),
            token: "local-test-token".into(),
            models,
        }
    }

    fn two_models() -> Vec<DesktopModel> {
        vec![
            model("sonnet-hash", "Claude Sonnet", Some(Family::Sonnet)),
            model("opus-hash", "Claude Opus", Some(Family::Opus)),
        ]
    }

    #[test]
    fn the_profile_declares_the_local_gateway_and_the_ordered_models() {
        let fixture = Fixture::installed();
        let status = fixture.apply(two_models());
        assert_eq!(status.state, TakeoverState::Applied);
        assert_eq!(status.config_source, ConfigSource::Local);
        assert_eq!(status.conflicts.len(), 0);

        let uuid = fixture.applied_uuid();
        assert_eq!(
            status.profile_path,
            fixture.paths.profile(&uuid).display().to_string()
        );
        let profile = fixture.profile(&uuid);
        assert_eq!(profile["inferenceProvider"], "gateway");
        assert_eq!(profile["inferenceGatewayBaseUrl"], "http://127.0.0.1:43121");
        assert_eq!(profile["inferenceGatewayApiKey"], "local-test-token");
        assert_eq!(profile["inferenceGatewayAuthScheme"], "bearer");
        assert_eq!(profile["modelDiscoveryEnabled"], false);
        assert_eq!(profile["disableDeploymentModeChooser"], true);
        assert_eq!(profile["isClaudeCodeForDesktopEnabled"], true);
        assert!(profile.get("coworkEgressAllowedHosts").is_none());
        let models = profile["inferenceModels"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["name"], "claude-sonnet-hash");
        assert_eq!(models[0]["labelOverride"], "Claude Sonnet");
        assert_eq!(models[0]["anthropicFamilyTier"], "sonnet");
        assert_eq!(models[1]["name"], "claude-opus-hash");
        assert_eq!(models[1]["anthropicFamilyTier"], "opus");
    }

    #[test]
    fn meta_keeps_other_entries_and_undo_restores_the_original_applied_id() {
        let fixture = Fixture::installed();
        fixture.write_meta(&serde_json::json!({
            "entries": [
                {"id": "other", "name": "Other provider"},
            ],
            "appliedId": "other",
            "unknown": {"keep": true},
        }));
        fixture.apply(two_models());

        let meta = fixture.meta();
        assert_eq!(meta["unknown"]["keep"], true);
        let entries = meta["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["id"], "other");
        assert_eq!(entries[1]["name"], DESKTOP_ENTRY_NAME);
        assert_eq!(meta["appliedId"], fixture.applied_uuid());

        let status = fixture.undo();
        assert_eq!(status.state, TakeoverState::Ready);
        let meta = fixture.meta();
        assert_eq!(meta["appliedId"], "other");
        assert_eq!(meta["entries"].as_array().unwrap().len(), 1);
        assert_eq!(meta["entries"][0]["id"], "other");
        assert!(!fixture.paths.profile("other").exists());
    }

    #[test]
    fn undo_deletes_applied_id_when_it_was_absent_before() {
        let fixture = Fixture::installed();
        fixture.write_meta(&serde_json::json!({"entries": []}));
        fixture.apply(two_models());
        assert!(fixture.meta().get("appliedId").is_some());
        fixture.undo();
        assert!(fixture.meta().get("appliedId").is_none());
    }

    #[test]
    fn appended_uuid_stays_stable_across_repeated_applies() {
        let fixture = Fixture::installed();
        let first = fixture.apply(two_models());
        let first_uuid = fixture.applied_uuid();
        let second = fixture.apply(two_models());
        assert_eq!(second.profile_path, first.profile_path);
        assert_eq!(second.state, TakeoverState::Applied);
        assert_eq!(fixture.meta()["entries"].as_array().unwrap().len(), 1);
        assert_eq!(fixture.applied_uuid(), first_uuid);
        assert!(fixture.paths.profile(&first_uuid).is_file());
    }

    #[test]
    fn machine_policy_is_managed_and_only_guided() {
        let fixture = Fixture::installed();
        let policy = DesktopPolicy {
            machine_values: vec!["AllowedProviders".into()],
            user_values: vec!["UserSetting".into()],
        };
        let status = inspect(&fixture.paths, fixture.state.path(), &policy, false).unwrap();
        assert_eq!(status.state, TakeoverState::Managed);
        assert_eq!(status.config_source, ConfigSource::MachinePolicy);
        assert!(status.guided);
        assert!(status
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "machine_policy" && conflict.blocking));
        assert!(status
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "user_policy_overridden" && !conflict.blocking));

        let error = apply(
            &fixture.paths,
            fixture.state.path(),
            &policy,
            false,
            &route(two_models()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("managed"), "{error}");
        assert!(!fixture.paths.meta.exists());
    }

    #[test]
    fn user_policy_is_a_conflict() {
        let fixture = Fixture::installed();
        let policy = DesktopPolicy {
            machine_values: Vec::new(),
            user_values: vec!["AllowedProviders".into()],
        };
        let status = inspect(&fixture.paths, fixture.state.path(), &policy, false).unwrap();
        assert_eq!(status.state, TakeoverState::Conflict);
        assert_eq!(status.config_source, ConfigSource::UserPolicy);
        assert!(status.guided);
        assert!(apply(
            &fixture.paths,
            fixture.state.path(),
            &policy,
            false,
            &route(two_models())
        )
        .is_err());
    }

    #[test]
    fn a_running_desktop_refuses_the_write() {
        let fixture = Fixture::installed();
        let status = inspect(
            &fixture.paths,
            fixture.state.path(),
            &DesktopPolicy::default(),
            true,
        )
        .unwrap();
        assert_eq!(status.state, TakeoverState::Running);
        assert!(status
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "desktop_running" && conflict.blocking));

        let error = apply(
            &fixture.paths,
            fixture.state.path(),
            &DesktopPolicy::default(),
            true,
            &route(two_models()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("quit Claude Desktop"), "{error}");
        assert!(!fixture.paths.meta.exists());
    }

    #[test]
    fn a_missing_desktop_is_reported_without_writing() {
        let fixture = Fixture::missing();
        let status = inspect(
            &fixture.paths,
            fixture.state.path(),
            &DesktopPolicy::default(),
            false,
        )
        .unwrap();
        assert_eq!(status.state, TakeoverState::Missing);
        assert_eq!(status.config_source, ConfigSource::None);
        assert!(status.profile_path.is_empty());
        assert!(apply(
            &fixture.paths,
            fixture.state.path(),
            &DesktopPolicy::default(),
            false,
            &route(two_models())
        )
        .is_err());
    }

    #[test]
    fn undo_keeps_other_profiles() {
        let fixture = Fixture::installed();
        fixture.write_meta(&serde_json::json!({
            "entries": [{"id": "other", "name": "Other"}],
            "appliedId": "other",
        }));
        let other = fixture.paths.profile("other");
        fs::create_dir_all(&fixture.paths.library).unwrap();
        fs::write(&other, "{}").unwrap();

        fixture.apply(two_models());
        fixture.undo();
        assert!(other.is_file());
        assert_eq!(fixture.meta()["appliedId"], "other");
    }

    #[test]
    fn the_legacy_desktop_config_is_backed_up_once() {
        let fixture = Fixture::installed();
        fs::create_dir_all(&fixture.paths.data_dir).unwrap();
        fs::write(&fixture.paths.legacy_config, "{\"mcpServers\": {}}").unwrap();
        fixture.apply(two_models());
        let backup = super::super::backup_path(&fixture.paths.legacy_config);
        assert_eq!(fs::read_to_string(backup).unwrap(), "{\"mcpServers\": {}}");
        fixture.undo();
        assert!(fixture.paths.legacy_config.is_file());
    }

    #[test]
    fn an_unparseable_meta_file_disables_undo_until_it_is_fixed() {
        let fixture = Fixture::installed();
        fixture.apply(two_models());
        fs::write(&fixture.paths.meta, "{ not json").unwrap();

        let status = inspect(
            &fixture.paths,
            fixture.state.path(),
            &DesktopPolicy::default(),
            false,
        )
        .unwrap();
        assert_eq!(status.state, TakeoverState::Conflict);
        assert!(
            !status.restorable,
            "undo would fail on the same parse error, so it must not be offered"
        );
    }

    #[test]
    fn a_modified_profile_blocks_reapplying_and_undo_keeps_it() {
        let fixture = Fixture::installed();
        fixture.apply(two_models());
        let uuid = fixture.applied_uuid();
        let profile = fixture.paths.profile(&uuid);
        let edited = "{\"inferenceProvider\": \"gateway\", \"userNote\": true}\n";
        fs::write(&profile, edited).unwrap();

        let error = apply(
            &fixture.paths,
            fixture.state.path(),
            &DesktopPolicy::default(),
            false,
            &route(two_models()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("modified"), "{error}");

        // 撤销不删用户改过的 profile，也不拆条目：报告后让用户自己决定。
        let status = fixture.undo();
        assert_eq!(status.state, TakeoverState::Conflict);
        assert!(status.restorable);
        assert!(status
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "undo_kept_profile" && !conflict.blocking));
        assert_eq!(fs::read_to_string(&profile).unwrap(), edited);
        assert_eq!(fixture.meta()["appliedId"], uuid.as_str());
        assert!(fixture.state.path().join("desktop.json").is_file());
    }

    #[test]
    fn undo_refuses_a_receipt_from_another_installation() {
        let fixture = Fixture::installed();
        fixture.apply(two_models());
        let uuid = fixture.applied_uuid();
        let receipt_path = fixture.state.path().join("desktop.json");
        let mut receipt: Value =
            serde_json::from_str(&fs::read_to_string(&receipt_path).unwrap()).unwrap();
        receipt["meta_path"] =
            Value::String("D:\\Other\\Claude-3p\\configLibrary\\_meta.json".into());
        fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();

        let error = undo(
            &fixture.paths,
            fixture.state.path(),
            &DesktopPolicy::default(),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("belongs to"), "{error}");
        assert!(fixture.paths.profile(&uuid).is_file());
        assert_eq!(fixture.meta()["appliedId"], uuid.as_str());
    }

    #[test]
    fn a_meta_with_a_non_array_entries_field_is_not_replaced() {
        let fixture = Fixture::installed();
        fixture.write_meta(&serde_json::json!({"entries": "legacy"}));
        let error = apply(
            &fixture.paths,
            fixture.state.path(),
            &DesktopPolicy::default(),
            false,
            &route(two_models()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("not an array"), "{error}");
        assert_eq!(fixture.meta()["entries"], "legacy");
    }
}
