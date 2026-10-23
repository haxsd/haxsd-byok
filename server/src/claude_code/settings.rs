//! Claude Code CLI 的 `settings.json` 接管。
//!
//! 只改 `env` 下本应用负责的键：顶层 `model`、`permissions`、`hooks` 与所有未知键原样
//! 保留。首次写入前备份整份文件，收据（每个键的原值、是否存在、写前哈希）放在应用数据
//! 目录；撤销只还原“当前值仍等于我们写入值”的键。
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Error, Result};

use super::{
    atomic_write, backup_once, content_hash,
    ownership::{
        managed_conflicts, settings_conflicts, shell_conflicts, user_environment_variable,
        CliEnvironment,
    },
    parse_json, read_optional, remove_file_if_exists, same_path, CliTakeoverStatus, Family,
    TakeoverConflict, TakeoverState, ATTRIBUTION_HEADER_KEY, ATTRIBUTION_VALUE, AUTH_TOKEN_KEY,
    BASE_URL_KEY, RECEIPT_VERSION,
};

const RECEIPT_FILE: &str = "cli.json";

#[derive(Clone, Debug)]
pub struct CliPaths {
    pub config_dir: PathBuf,
    pub settings: PathBuf,
    pub backup: PathBuf,
    /// 配置目录或 `claude` 可执行文件存在；`false` 时状态是 `missing`、拒绝写入。
    pub installed: bool,
}

impl CliPaths {
    pub fn discover() -> Result<Self> {
        if let Some(directory) = config_dir_override()? {
            return Self::at_config_dir(directory);
        }
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Config("cannot resolve the user home directory".into()))?;
        Self::at_config_dir(home.join(".claude"))
    }

    /// 注入用的构造：调用方给出配置目录，不读 `CLAUDE_CONFIG_DIR`。
    pub fn at_config_dir(directory: PathBuf) -> Result<Self> {
        if !directory.is_absolute() {
            return Err(Error::Config(
                "Claude Code config paths must be absolute".into(),
            ));
        }
        let settings = directory.join("settings.json");
        let installed = directory.is_dir() || settings.is_file() || cli_on_path();
        Ok(Self {
            backup: super::backup_path(&settings),
            settings,
            config_dir: directory,
            installed,
        })
    }
}

/// 状态检查时已知的网关值：只用于判断“这个值是不是我们即将写的”。
#[derive(Clone, Copy, Debug)]
pub struct GatewayValues<'a> {
    pub base_url: &'a str,
    pub token: &'a str,
}

#[derive(Clone, Debug)]
pub struct CliModel {
    pub family: Family,
    pub model_hash: String,
    pub display_name: String,
}

#[derive(Clone, Debug)]
pub struct CliRoute {
    pub base_url: String,
    pub token: String,
    pub models: Vec<CliModel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    version: u32,
    settings_path: String,
    /// 第一次接管前的文件哈希；文件本来不存在时为空。
    #[serde(default)]
    before_hash: Option<String>,
    applied_hash: String,
    /// `env` 是我们在第一次接管时创建的：撤销后它若为空就删掉。
    env_created: bool,
    keys: Vec<KeyRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct KeyRecord {
    key: String,
    existed: bool,
    #[serde(default)]
    original: Option<Value>,
    applied: Value,
}

pub fn inspect(
    paths: &CliPaths,
    state_dir: &Path,
    environment: &CliEnvironment,
    gateway: Option<&GatewayValues>,
) -> Result<CliTakeoverStatus> {
    let mut conflicts = managed_conflicts(environment);
    let desired = gateway_values(gateway);
    conflicts.extend(shell_conflicts(environment, &desired));

    let receipt = match read_receipt(state_dir) {
        Ok(receipt) => receipt,
        Err(error) => {
            conflicts.push(TakeoverConflict::new(
                "receipt_unreadable",
                format!("cannot read the Claude Code takeover receipt: {error}"),
                true,
            ));
            None
        }
    };
    let owned = receipt
        .as_ref()
        .map(|receipt| {
            receipt
                .keys
                .iter()
                .map(|record| (record.key.clone(), record.applied.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    let raw = read_optional(&paths.settings)?;
    let mut settings_broken = false;
    let document = match raw.as_deref() {
        Some(text) => match parse_json(text) {
            Ok(document) => {
                conflicts.extend(settings_conflicts(&document, &owned, &desired));
                Some(document)
            }
            Err(error) => {
                settings_broken = true;
                conflicts.push(TakeoverConflict::new(
                    "settings_invalid",
                    format!("cannot parse {}: {error}", paths.settings.display()),
                    true,
                ));
                None
            }
        },
        None => None,
    };

    let path_matches = receipt
        .as_ref()
        .map(|receipt| same_path(Path::new(&receipt.settings_path), &paths.settings))
        .unwrap_or(true);
    if let Some(receipt) = &receipt {
        if !path_matches {
            conflicts.push(TakeoverConflict::new(
                "receipt_mismatch",
                format!(
                    "the takeover receipt belongs to {}; restore CLAUDE_CONFIG_DIR or undo there",
                    receipt.settings_path
                ),
                true,
            ));
        }
        if raw.is_none() {
            conflicts.push(TakeoverConflict::new(
                "settings_missing",
                "Claude Code settings.json is missing; apply again or undo to drop the receipt",
                false,
            ));
        }
    }

    let applied = match (&receipt, &document) {
        (Some(receipt), Some(document)) => {
            path_matches
                && receipt.keys.iter().all(|record| {
                    document
                        .get("env")
                        .and_then(Value::as_object)
                        .and_then(|env| env.get(&record.key))
                        == Some(&record.applied)
                })
        }
        _ => false,
    };
    if receipt.is_some()
        && !applied
        && raw.is_some()
        && !conflicts.iter().any(|conflict| conflict.blocking)
    {
        conflicts.push(TakeoverConflict::new(
            "takeover_incomplete",
            "the managed env values are no longer all present; apply again or undo",
            false,
        ));
    }

    let state = if !environment.managed_sources.is_empty() {
        TakeoverState::Managed
    } else if receipt.is_some() {
        if applied {
            TakeoverState::Applied
        } else {
            TakeoverState::Conflict
        }
    } else if !paths.installed {
        TakeoverState::Missing
    } else if conflicts.iter().any(|conflict| conflict.blocking) {
        TakeoverState::Conflict
    } else {
        TakeoverState::Ready
    };

    Ok(CliTakeoverStatus {
        state,
        settings_path: paths.settings.display().to_string(),
        backup_path: if paths.backup.is_file() {
            paths.backup.display().to_string()
        } else {
            String::new()
        },
        restorable: receipt.is_some() && path_matches && !settings_broken,
        conflicts,
        missing_models: Vec::new(),
        managed_sources: environment.managed_sources.clone(),
    })
}

pub fn apply(
    paths: &CliPaths,
    state_dir: &Path,
    environment: &CliEnvironment,
    route: &CliRoute,
) -> Result<CliTakeoverStatus> {
    if !paths.installed {
        return Err(Error::Config(
            "Claude Code is not installed yet; run `claude` once before applying the takeover"
                .into(),
        ));
    }
    let desired = desired_values(route)?;
    let raw = read_optional(&paths.settings)?;
    let mut document = match raw.as_deref() {
        Some(text) => parse_json(text).map_err(|error| {
            Error::Config(format!(
                "cannot parse {}: {error}",
                paths.settings.display()
            ))
        })?,
        None => Value::Object(Map::new()),
    };
    if !document.is_object() {
        return Err(Error::Config(
            "Claude Code settings must be a JSON object".into(),
        ));
    }

    let previous = read_receipt(state_dir)?;
    if let Some(receipt) = &previous {
        if !same_path(Path::new(&receipt.settings_path), &paths.settings) {
            return Err(Error::Config(format!(
                "the Claude Code takeover receipt belongs to {}; restore CLAUDE_CONFIG_DIR or undo there",
                receipt.settings_path
            )));
        }
    }
    let owned = previous
        .as_ref()
        .map(|receipt| {
            receipt
                .keys
                .iter()
                .map(|record| (record.key.clone(), record.applied.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut conflicts = managed_conflicts(environment);
    conflicts.extend(settings_conflicts(&document, &owned, &desired));
    if let Some(conflict) = conflicts.iter().find(|conflict| conflict.blocking) {
        return Err(Error::Config(conflict.message.clone()));
    }

    // 原始值必须从写入前的快照里取；下面会先还原不再管理的旧键，再写新值。
    let original_env = document.get("env").and_then(Value::as_object).cloned();
    let env_created = previous
        .as_ref()
        .map(|receipt| receipt.env_created)
        .unwrap_or(original_env.is_none());
    let desired_keys = desired.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let previous_keys = previous
        .as_ref()
        .map(|receipt| receipt.keys.clone())
        .unwrap_or_default();
    let previous_by_key = previous_keys
        .iter()
        .map(|record| (record.key.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let mut records = Vec::with_capacity(desired.len());
    for (key, value) in &desired {
        if let Some(record) = previous_by_key.get(key.as_str()) {
            let mut record = (*record).clone();
            record.applied = value.clone();
            records.push(record);
        } else {
            let current = original_env
                .as_ref()
                .and_then(|environment| environment.get(key))
                .cloned();
            records.push(KeyRecord {
                key: key.clone(),
                existed: current.is_some(),
                original: current,
                applied: value.clone(),
            });
        }
    }

    let Some(object) = document.as_object_mut() else {
        return Err(Error::Config(
            "Claude Code settings must be a JSON object".into(),
        ));
    };
    if let Some(Value::Object(env)) = object.get_mut("env") {
        for record in &previous_keys {
            if !desired_keys.contains(record.key.as_str()) {
                restore_if_ours(env, record);
            }
        }
    }
    if !object.contains_key("env") {
        object.insert("env".into(), Value::Object(Map::new()));
    }
    let env = object
        .get_mut("env")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Error::Config("env in Claude Code settings is not a JSON object".into()))?;
    for (key, value) in &desired {
        env.insert(key.clone(), value.clone());
    }
    let applied = pretty(&document)?;

    // 首次接管前保留整份原文件；重复接管不覆盖它。
    if previous.is_none() {
        backup_once(&paths.settings)?;
    }
    let receipt = Receipt {
        version: RECEIPT_VERSION,
        settings_path: paths.settings.display().to_string(),
        before_hash: previous
            .as_ref()
            .map(|receipt| receipt.before_hash.clone())
            .unwrap_or_else(|| raw.as_deref().map(content_hash)),
        applied_hash: content_hash(&applied),
        env_created,
        keys: records,
    };
    let receipt_path = state_dir.join(RECEIPT_FILE);
    atomic_write(&receipt_path, &serde_json::to_vec_pretty(&receipt)?)?;
    if read_optional(&paths.settings)? != raw {
        return Err(Error::Config(
            "Claude Code settings changed while preparing the update".into(),
        ));
    }
    if raw.as_deref() != Some(applied.as_str()) {
        atomic_write(&paths.settings, applied.as_bytes())?;
    }
    inspect(
        paths,
        state_dir,
        environment,
        Some(&GatewayValues {
            base_url: &route.base_url,
            token: &route.token,
        }),
    )
}

pub fn undo(
    paths: &CliPaths,
    state_dir: &Path,
    environment: &CliEnvironment,
) -> Result<CliTakeoverStatus> {
    let Some(receipt) = read_receipt(state_dir)? else {
        return inspect(paths, state_dir, environment, None);
    };
    if !same_path(Path::new(&receipt.settings_path), &paths.settings) {
        return Err(Error::Config(format!(
            "the Claude Code takeover receipt belongs to {}; restore CLAUDE_CONFIG_DIR or undo there",
            receipt.settings_path
        )));
    }
    let Some(raw) = read_optional(&paths.settings)? else {
        remove_file_if_exists(&state_dir.join(RECEIPT_FILE))?;
        return inspect(paths, state_dir, environment, None);
    };
    let mut document = parse_json(&raw).map_err(|error| {
        Error::Config(format!(
            "cannot parse {}: {error}",
            paths.settings.display()
        ))
    })?;
    let Some(object) = document.as_object_mut() else {
        return Err(Error::Config(
            "Claude Code settings must be a JSON object".into(),
        ));
    };

    let mut kept = Vec::new();
    let mut remove_env = false;
    match object.get_mut("env") {
        Some(Value::Object(env)) => {
            for record in &receipt.keys {
                match env.get(&record.key) {
                    Some(current) if current == &record.applied => restore(env, record),
                    Some(_) => kept.push(TakeoverConflict::keyed(
                        "user_modified",
                        record.key.clone(),
                        format!(
                            "kept env.{} because it no longer matches the takeover value",
                            record.key
                        ),
                        false,
                    )),
                    None => kept.push(TakeoverConflict::keyed(
                        "user_modified",
                        record.key.clone(),
                        format!(
                            "kept the removal of env.{} because it changed after the takeover",
                            record.key
                        ),
                        false,
                    )),
                }
            }
            remove_env = receipt.env_created && env.is_empty();
        }
        Some(_) => kept.push(TakeoverConflict::keyed(
            "env_replaced",
            "env",
            "env is no longer a JSON object; it was left unchanged",
            false,
        )),
        None => kept.push(TakeoverConflict::new(
            "env_removed",
            "env was removed after the takeover; the removal is kept",
            false,
        )),
    }
    if remove_env {
        object.remove("env");
    }

    let updated = pretty(&document)?;
    if updated != raw {
        if read_optional(&paths.settings)?.as_deref() != Some(raw.as_str()) {
            return Err(Error::Config(
                "Claude Code settings changed during the restore".into(),
            ));
        }
        atomic_write(&paths.settings, updated.as_bytes())?;
    }
    remove_file_if_exists(&state_dir.join(RECEIPT_FILE))?;
    let mut status = inspect(paths, state_dir, environment, None)?;
    status.conflicts.extend(kept);
    Ok(status)
}

fn read_receipt(state_dir: &Path) -> Result<Option<Receipt>> {
    let Some(text) = read_optional(&state_dir.join(RECEIPT_FILE))? else {
        return Ok(None);
    };
    let receipt: Receipt = serde_json::from_str(&text)?;
    if receipt.version != RECEIPT_VERSION {
        return Err(Error::Config(
            "unsupported Claude Code takeover receipt".into(),
        ));
    }
    Ok(Some(receipt))
}

fn gateway_values(gateway: Option<&GatewayValues>) -> BTreeMap<String, Value> {
    let Some(gateway) = gateway else {
        return BTreeMap::new();
    };
    BTreeMap::from([
        (
            BASE_URL_KEY.to_owned(),
            Value::String(gateway.base_url.to_owned()),
        ),
        (
            AUTH_TOKEN_KEY.to_owned(),
            Value::String(gateway.token.to_owned()),
        ),
    ])
}

fn desired_values(route: &CliRoute) -> Result<BTreeMap<String, Value>> {
    validate_route(route)?;
    let mut desired = BTreeMap::from([
        (
            BASE_URL_KEY.to_owned(),
            Value::String(route.base_url.clone()),
        ),
        (
            AUTH_TOKEN_KEY.to_owned(),
            Value::String(route.token.clone()),
        ),
        (
            ATTRIBUTION_HEADER_KEY.to_owned(),
            Value::String(ATTRIBUTION_VALUE.to_owned()),
        ),
    ]);
    let mut families = BTreeSet::new();
    for model in &route.models {
        if !families.insert(model.family) {
            return Err(Error::Config(format!(
                "duplicate Claude {} model",
                model.family.tier()
            )));
        }
        let hash = model.model_hash.trim();
        if hash.is_empty() {
            return Err(Error::Config(format!(
                "Claude {} model must not be blank",
                model.family.tier()
            )));
        }
        desired.insert(
            model.family.model_env().to_owned(),
            Value::String(hash.to_owned()),
        );
        let name = model.display_name.trim();
        if !name.is_empty() {
            desired.insert(
                model.family.name_env().to_owned(),
                Value::String(name.to_owned()),
            );
        }
    }
    Ok(desired)
}

fn validate_route(route: &CliRoute) -> Result<()> {
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

fn restore(environment: &mut Map<String, Value>, record: &KeyRecord) {
    if record.existed {
        environment.insert(
            record.key.clone(),
            record.original.clone().unwrap_or(Value::Null),
        );
    } else {
        environment.remove(&record.key);
    }
}

fn restore_if_ours(environment: &mut Map<String, Value>, record: &KeyRecord) {
    if environment.get(&record.key) == Some(&record.applied) {
        restore(environment, record);
    }
}

fn config_dir_override() -> Result<Option<PathBuf>> {
    let Some(raw) = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .or_else(|| user_environment_variable("CLAUDE_CONFIG_DIR"))
    else {
        return Ok(None);
    };
    let raw = raw.trim().trim_matches('"');
    if raw.is_empty() {
        return Err(Error::Config(
            "CLAUDE_CONFIG_DIR is empty; unset it or point it at a directory".into(),
        ));
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(Error::Config(format!(
            "CLAUDE_CONFIG_DIR must be an absolute path: {raw}"
        )));
    }
    Ok(Some(path))
}

fn cli_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        ["claude", "claude.exe", "claude.cmd", "claude.bat"]
            .iter()
            .any(|name| directory.join(name).is_file())
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    struct Fixture {
        config: tempfile::TempDir,
        state: tempfile::TempDir,
        paths: CliPaths,
    }

    impl Fixture {
        fn new(settings: Option<&str>) -> Self {
            let config = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            if let Some(settings) = settings {
                fs::write(config.path().join("settings.json"), settings).unwrap();
            }
            let paths = CliPaths::at_config_dir(config.path().to_path_buf()).unwrap();
            Self {
                config,
                state,
                paths,
            }
        }

        fn apply(&self) -> CliTakeoverStatus {
            apply(
                &self.paths,
                self.state.path(),
                &CliEnvironment::default(),
                &route(),
            )
            .unwrap()
        }

        fn undo(&self) -> CliTakeoverStatus {
            undo(&self.paths, self.state.path(), &CliEnvironment::default()).unwrap()
        }

        fn document(&self) -> Value {
            serde_json::from_str(
                &fs::read_to_string(self.config.path().join("settings.json")).unwrap(),
            )
            .unwrap()
        }

        fn backup(&self) -> String {
            fs::read_to_string(self.config.path().join("settings.json.haxsd-byok.bak")).unwrap()
        }

        fn write(&self, document: &Value) {
            fs::write(
                self.config.path().join("settings.json"),
                serde_json::to_vec_pretty(document).unwrap(),
            )
            .unwrap();
        }
    }

    fn route() -> CliRoute {
        CliRoute {
            base_url: "http://127.0.0.1:43121".into(),
            token: "local-test-token".into(),
            models: vec![
                CliModel {
                    family: Family::Sonnet,
                    model_hash: "sonnet-hash".into(),
                    display_name: "Claude Sonnet".into(),
                },
                CliModel {
                    family: Family::Opus,
                    model_hash: "opus-hash".into(),
                    display_name: "Claude Opus".into(),
                },
            ],
        }
    }

    fn route_without_models() -> CliRoute {
        CliRoute {
            models: Vec::new(),
            ..route()
        }
    }

    #[test]
    fn first_takeover_backs_up_the_whole_file_and_preserves_unknown_keys() {
        let original = r#"{
  "model": "user-model",
  "permissions": {"allow": ["Bash"]},
  "hooks": {"Stop": []},
  "unknown": {"a": 1},
  "env": {"KEEP": "me"}
}"#;
        let fixture = Fixture::new(Some(original));
        let status = fixture.apply();
        assert_eq!(status.state, TakeoverState::Applied);
        assert_eq!(fixture.backup(), original);

        let document = fixture.document();
        assert_eq!(document["model"], "user-model");
        assert_eq!(document["permissions"]["allow"][0], "Bash");
        assert_eq!(document["hooks"]["Stop"], serde_json::json!([]));
        assert_eq!(document["unknown"]["a"], 1);
        assert_eq!(document["env"]["KEEP"], "me");
        assert_eq!(document["env"][BASE_URL_KEY], "http://127.0.0.1:43121");
        assert_eq!(document["env"][AUTH_TOKEN_KEY], "local-test-token");
        assert_eq!(document["env"][ATTRIBUTION_HEADER_KEY], "0");
        assert_eq!(
            document["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            "sonnet-hash"
        );
        assert_eq!(
            document["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"],
            "Claude Sonnet"
        );
        assert_eq!(document["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "opus-hash");
        assert!(document["env"].get("ANTHROPIC_MODEL").is_none());
        assert!(document["env"].get("ANTHROPIC_API_KEY").is_none());
        assert!(document["env"].get("CLAUDE_CODE_OAUTH_TOKEN").is_none());
    }

    #[test]
    fn repeated_apply_does_not_replace_the_first_backup() {
        let fixture = Fixture::new(Some(r#"{"env": {"KEEP": "me"}}"#));
        fixture.apply();
        let backup = fixture.backup();

        let changed = CliRoute {
            token: "rotated-token".into(),
            ..route()
        };
        let status = apply(
            &fixture.paths,
            fixture.state.path(),
            &CliEnvironment::default(),
            &changed,
        )
        .unwrap();
        assert_eq!(status.state, TakeoverState::Applied);
        assert_eq!(fixture.backup(), backup);
        assert_eq!(fixture.document()["env"][AUTH_TOKEN_KEY], "rotated-token");

        // 重复应用同一份配置也不产生第二份备份或额外键。
        fixture.apply();
        assert_eq!(fixture.backup(), backup);
        assert_eq!(fixture.document()["env"]["KEEP"], "me");
    }

    #[test]
    fn a_foreign_base_url_is_reported_and_never_overwritten() {
        let fixture = Fixture::new(Some(
            r#"{"env": {"ANTHROPIC_BASE_URL": "https://api.anthropic.com"}}"#,
        ));
        let status = inspect(
            &fixture.paths,
            fixture.state.path(),
            &CliEnvironment::default(),
            None,
        )
        .unwrap();
        assert_eq!(status.state, TakeoverState::Conflict);
        assert!(status
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "env_base_url" && conflict.blocking));

        let error = apply(
            &fixture.paths,
            fixture.state.path(),
            &CliEnvironment::default(),
            &route(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("ANTHROPIC_BASE_URL"), "{error}");
        assert_eq!(
            fixture.document()["env"][BASE_URL_KEY],
            "https://api.anthropic.com"
        );
        assert!(!fixture
            .config
            .path()
            .join("settings.json.haxsd-byok.bak")
            .exists());
    }

    #[test]
    fn user_edits_after_takeover_are_reported_and_kept_on_undo() {
        let fixture = Fixture::new(Some(r#"{"env": {"KEEP": "me"}}"#));
        fixture.apply();
        let mut document = fixture.document();
        document["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"] = Value::String("user-choice".into());
        fixture.write(&document);

        let status = inspect(
            &fixture.paths,
            fixture.state.path(),
            &CliEnvironment::default(),
            None,
        )
        .unwrap();
        assert_eq!(status.state, TakeoverState::Conflict);
        assert!(status
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "user_modified"
                && conflict.key.as_deref() == Some("ANTHROPIC_DEFAULT_SONNET_MODEL")));

        let status = fixture.undo();
        assert_eq!(status.state, TakeoverState::Ready);
        assert!(status
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "user_modified"
                && conflict.key.as_deref() == Some("ANTHROPIC_DEFAULT_SONNET_MODEL")));
        let document = fixture.document();
        assert_eq!(
            document["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            "user-choice"
        );
        assert_eq!(document["env"]["KEEP"], "me");
        assert!(document["env"].get(BASE_URL_KEY).is_none());
        assert!(document["env"].get(AUTH_TOKEN_KEY).is_none());
        assert!(document["env"]
            .get("ANTHROPIC_DEFAULT_OPUS_MODEL")
            .is_none());
        assert!(!fixture.state.path().join("cli.json").exists());
    }

    #[test]
    fn undo_removes_an_env_object_it_created() {
        let original = r#"{"model": "user-model"}"#;
        let fixture = Fixture::new(Some(original));
        apply(
            &fixture.paths,
            fixture.state.path(),
            &CliEnvironment::default(),
            &route_without_models(),
        )
        .unwrap();
        assert!(fixture.document().get("env").is_some());

        let status = fixture.undo();
        assert_eq!(status.state, TakeoverState::Ready);
        assert!(fixture.document().get("env").is_none());
        assert_eq!(
            serde_json::to_string(&fixture.document()).unwrap(),
            serde_json::to_string(&serde_json::from_str::<Value>(original).unwrap()).unwrap()
        );
    }

    #[test]
    fn undo_restores_the_original_auth_token_and_leaves_a_later_api_key_alone() {
        let fixture = Fixture::new(Some(r#"{"env": {"ANTHROPIC_AUTH_TOKEN": "old-token"}}"#));
        fixture.apply();
        let mut document = fixture.document();
        // 接管之后用户自己加了 API key：撤销只动收据里的键，不能顺手删掉它。
        document["env"]["ANTHROPIC_API_KEY"] = Value::String("sk-user".into());
        fixture.write(&document);

        let status = fixture.undo();
        assert_eq!(status.state, TakeoverState::Conflict);
        let document = fixture.document();
        assert_eq!(document["env"][AUTH_TOKEN_KEY], "old-token");
        assert_eq!(document["env"]["ANTHROPIC_API_KEY"], "sk-user");
    }

    #[test]
    fn an_existing_api_key_blocks_apply_without_touching_the_file() {
        let original = r#"{"env": {"ANTHROPIC_API_KEY": "sk-user"}}"#;
        let fixture = Fixture::new(Some(original));
        let error = apply(
            &fixture.paths,
            fixture.state.path(),
            &CliEnvironment::default(),
            &route(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("ANTHROPIC_API_KEY"), "{error}");
        assert_eq!(
            fs::read_to_string(fixture.config.path().join("settings.json")).unwrap(),
            original
        );
        assert!(!fixture
            .config
            .path()
            .join("settings.json.haxsd-byok.bak")
            .exists());
    }

    #[test]
    fn managed_settings_block_the_takeover() {
        let fixture = Fixture::new(None);
        let environment = CliEnvironment {
            managed_sources: vec!["C:\\Program Files\\ClaudeCode\\managed-settings.json".into()],
            shell_env: BTreeMap::new(),
        };
        let status = inspect(&fixture.paths, fixture.state.path(), &environment, None).unwrap();
        assert_eq!(status.state, TakeoverState::Managed);

        let error =
            apply(&fixture.paths, fixture.state.path(), &environment, &route()).unwrap_err();
        assert!(error.to_string().contains("managed"), "{error}");
    }

    #[test]
    fn an_unparseable_settings_file_disables_undo_until_it_is_fixed() {
        let fixture = Fixture::new(None);
        assert!(fixture.apply().restorable);
        fs::write(fixture.config.path().join("settings.json"), "{ not json").unwrap();

        let status = inspect(
            &fixture.paths,
            fixture.state.path(),
            &CliEnvironment::default(),
            None,
        )
        .unwrap();
        assert_eq!(status.state, TakeoverState::Conflict);
        assert!(
            !status.restorable,
            "undo would fail on the same parse error, so it must not be offered"
        );
    }

    #[test]
    fn invalid_json_is_reported_and_not_replaced() {
        let original = "{ // comments are not JSON\n \"env\": {} }";
        let fixture = Fixture::new(Some(original));
        let status = inspect(
            &fixture.paths,
            fixture.state.path(),
            &CliEnvironment::default(),
            None,
        )
        .unwrap();
        assert_eq!(status.state, TakeoverState::Conflict);
        assert!(status
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "settings_invalid" && conflict.blocking));

        let error = apply(
            &fixture.paths,
            fixture.state.path(),
            &CliEnvironment::default(),
            &route(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot parse"), "{error}");
        assert_eq!(
            fs::read_to_string(fixture.config.path().join("settings.json")).unwrap(),
            original
        );
    }

    #[test]
    fn undo_then_apply_records_the_restored_values_as_the_new_originals() {
        let fixture = Fixture::new(Some(r#"{"env": {"ANTHROPIC_AUTH_TOKEN": "old-token"}}"#));
        fixture.apply();
        fixture.undo();
        fixture.apply();
        assert_eq!(
            fixture.document()["env"][AUTH_TOKEN_KEY],
            "local-test-token"
        );
        fixture.undo();
        assert_eq!(fixture.document()["env"][AUTH_TOKEN_KEY], "old-token");
    }

    #[test]
    fn a_missing_settings_file_is_created_and_removed_again() {
        let fixture = Fixture::new(None);
        assert!(fixture.apply().state == TakeoverState::Applied);
        assert!(fixture.paths.settings.is_file());
        fixture.undo();
        assert!(fixture.document().get("env").is_none());
    }

    #[test]
    fn a_receipt_for_another_config_directory_is_refused() {
        let fixture = Fixture::new(None);
        fixture.apply();
        let receipt_path = fixture.state.path().join("cli.json");
        let mut receipt: Value =
            serde_json::from_str(&fs::read_to_string(&receipt_path).unwrap()).unwrap();
        receipt["settings_path"] = Value::String("C:\\Users\\other\\.claude\\settings.json".into());
        fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();

        let error = undo(
            &fixture.paths,
            fixture.state.path(),
            &CliEnvironment::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("belongs to"), "{error}");
    }
}
