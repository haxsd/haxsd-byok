//! 键归属、冲突检测与受管策略探测。
//!
//! 这里只做判断、不改文件：哪些 `env` 值属于我们、哪些是别人的配置、机器上有没有
//! 受管策略。写入与撤销在 `settings.rs` / `desktop.rs`。
use std::{collections::BTreeMap, path::PathBuf};

use serde_json::Value;

use super::{
    TakeoverConflict, API_KEY_HELPER_KEY, API_KEY_KEY, BASE_URL_KEY, BEDROCK_KEY, FOUNDRY_KEY,
    MANAGED_ENV_KEYS, VERTEX_KEY,
};

/// 与受管键同名的用户环境变量；只作提示，不挡路。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellVariable {
    pub value: String,
    pub source: &'static str,
}

/// 只读环境探测：受管策略来源与用户环境变量。
#[derive(Clone, Debug, Default)]
pub struct CliEnvironment {
    /// 受管策略文件/注册表项的展示路径；非空即 `managed`。
    pub managed_sources: Vec<String>,
    pub shell_env: BTreeMap<String, ShellVariable>,
}

impl CliEnvironment {
    pub fn detect() -> Self {
        let mut managed_sources = managed_settings_sources(managed_settings_dir().as_deref());
        managed_sources.extend(cli_policy_sources());
        let mut shell_env = BTreeMap::new();
        for key in MANAGED_ENV_KEYS {
            if let Ok(value) = std::env::var(key) {
                shell_env.insert(
                    key.to_owned(),
                    ShellVariable {
                        value,
                        source: "process environment",
                    },
                );
            } else if let Some(value) = user_environment_variable(key) {
                shell_env.insert(
                    key.to_owned(),
                    ShellVariable {
                        value,
                        source: "user environment",
                    },
                );
            }
        }
        Self {
            managed_sources,
            shell_env,
        }
    }
}

/// `HKLM\SOFTWARE\Policies\Claude` / `HKCU\SOFTWARE\Policies\Claude` 的只读快照。
#[derive(Clone, Debug, Default)]
pub struct DesktopPolicy {
    pub machine_values: Vec<String>,
    pub user_values: Vec<String>,
}

impl DesktopPolicy {
    #[cfg(windows)]
    pub fn detect() -> Self {
        Self {
            machine_values: policy_values(windows_registry::LOCAL_MACHINE),
            user_values: policy_values(windows_registry::CURRENT_USER),
        }
    }

    #[cfg(not(windows))]
    pub fn detect() -> Self {
        Self::default()
    }

    pub fn machine(&self) -> bool {
        !self.machine_values.is_empty()
    }

    pub fn user(&self) -> bool {
        !self.user_values.is_empty()
    }
}

/// 受管策略造成的阻断；CLI 与 Desktop 共用。
pub fn managed_conflicts(environment: &CliEnvironment) -> Vec<TakeoverConflict> {
    environment
        .managed_sources
        .iter()
        .map(|source| {
            TakeoverConflict::new(
                "managed_settings",
                format!("Claude Code settings are managed by {source}; settings.json is ignored"),
                true,
            )
        })
        .collect()
}

/// 用户 shell/系统环境里的同名变量：只提示。值与我们写入的相同时不打扰用户。
pub fn shell_conflicts(
    environment: &CliEnvironment,
    desired: &BTreeMap<String, Value>,
) -> Vec<TakeoverConflict> {
    environment
        .shell_env
        .iter()
        .filter_map(|(key, variable)| {
            if desired.get(key).and_then(Value::as_str) == Some(variable.value.as_str()) {
                return None;
            }
            Some(TakeoverConflict::keyed(
                "shell_env",
                key.clone(),
                format!(
                    "{key}={} is also set in the {}; Claude Code started outside these settings may use it",
                    display_value(key, &variable.value),
                    variable.source
                ),
                false,
            ))
        })
        .collect()
}

/// `settings.json` 内的冲突。
///
/// `owned` 是收据里记下的“我们写入的值”：同一键仍等于它时不算别人的配置。`desired`
/// 是本次准备写入的值：用户恰好改成目标值时无需再报冲突。
pub fn settings_conflicts(
    document: &Value,
    owned: &BTreeMap<String, Value>,
    desired: &BTreeMap<String, Value>,
) -> Vec<TakeoverConflict> {
    let mut conflicts = Vec::new();
    let Some(object) = document.as_object() else {
        return vec![TakeoverConflict::new(
            "settings_type",
            "Claude Code settings must be a JSON object",
            true,
        )];
    };
    if let Some(helper) = object
        .get(API_KEY_HELPER_KEY)
        .and_then(Value::as_str)
        .filter(|helper| !helper.trim().is_empty())
    {
        conflicts.push(TakeoverConflict::keyed(
            "api_key_helper",
            API_KEY_HELPER_KEY,
            format!(
                "apiKeyHelper is set to {helper}; remove it before the gateway can provide credentials"
            ),
            true,
        ));
    }
    let Some(env) = object.get("env") else {
        return conflicts;
    };
    let Some(env) = env.as_object() else {
        conflicts.push(TakeoverConflict::keyed(
            "env_type",
            "env",
            "env in Claude Code settings is not a JSON object",
            true,
        ));
        return conflicts;
    };

    for key in [
        BASE_URL_KEY,
        API_KEY_KEY,
        BEDROCK_KEY,
        VERTEX_KEY,
        FOUNDRY_KEY,
    ] {
        let Some(current) = env.get(key) else {
            continue;
        };
        // 上一轮接管写进去的键由下面的 `user_modified` 分支判断：它更清楚“是谁改的”。
        if owned.contains_key(key) {
            continue;
        }
        if desired.get(key) == Some(current) {
            continue;
        }
        let conflict = match key {
            BASE_URL_KEY => {
                let Some(value) = current.as_str() else {
                    conflicts.push(type_conflict(key));
                    continue;
                };
                TakeoverConflict::keyed(
                    "env_base_url",
                    key,
                    format!("env.{key} is already set to {value}; applying would replace it"),
                    true,
                )
            }
            API_KEY_KEY => {
                if current
                    .as_str()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    continue;
                }
                TakeoverConflict::keyed(
                    "env_api_key",
                    key,
                    "env.ANTHROPIC_API_KEY is set; remove it before the gateway can provide credentials",
                    true,
                )
            }
            _ => {
                if !feature_enabled(current) {
                    continue;
                }
                TakeoverConflict::keyed(
                    "env_feature_flag",
                    key,
                    format!(
                        "env.{key} is enabled; disable it before applying the gateway configuration"
                    ),
                    true,
                )
            }
        };
        conflicts.push(conflict);
    }

    // 收据里的键被外部改过：不静默覆盖。当前值恰好等于本次目标值时不挡路。
    for (key, applied) in owned {
        let current = env.get(key);
        if current == Some(applied) {
            continue;
        }
        if current
            .and_then(Value::as_str)
            .is_some_and(|current| desired.get(key).and_then(Value::as_str) == Some(current))
        {
            continue;
        }
        conflicts.push(TakeoverConflict::keyed(
            "user_modified",
            key.clone(),
            format!("env.{key} changed after the takeover; undo will keep the current value"),
            true,
        ));
    }
    conflicts
}

fn type_conflict(key: &str) -> TakeoverConflict {
    TakeoverConflict::keyed(
        "env_value_type",
        key,
        format!("env.{key} must be a string"),
        true,
    )
}

/// `"1"` / `"true"` 这类开关值算启用；`"0"`、`"false"`、空串不算。
fn feature_enabled(value: &Value) -> bool {
    match value {
        Value::Bool(enabled) => *enabled,
        Value::String(text) => {
            let text = text.trim();
            !text.is_empty()
                && !text.eq_ignore_ascii_case("0")
                && !text.eq_ignore_ascii_case("false")
        }
        Value::Null => false,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        _ => true,
    }
}

fn display_value(key: &str, value: &str) -> String {
    let key = key.to_ascii_lowercase();
    if key.contains("token") || key.contains("key") {
        "[redacted]".into()
    } else {
        value.to_owned()
    }
}

/// `C:\Program Files\ClaudeCode`、`/Library/Application Support/ClaudeCode` 或
/// `/etc/claude-code`；受管设置文件都放在这个目录下。
fn managed_settings_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let program_files = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\Program Files"));
        Some(program_files.join("ClaudeCode"))
    }
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from("/Library/Application Support/ClaudeCode"))
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        Some(PathBuf::from("/etc/claude-code"))
    }
}

/// 目录里生效的受管设置文件：`managed-settings.json` 与 `managed-settings.d/*.json`。
///
/// Claude Code 把 drop-in 目录里的 JSON 按文件名顺序合并；只要有一个文件，`settings.json`
/// 就不再是唯一来源。返回展示路径给界面诊断，不读取内容。
fn managed_settings_sources(directory: Option<&std::path::Path>) -> Vec<String> {
    let Some(directory) = directory else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    let base = directory.join("managed-settings.json");
    if base.is_file() {
        sources.push(base.display().to_string());
    }
    let Ok(entries) = std::fs::read_dir(directory.join("managed-settings.d")) else {
        return sources;
    };
    let mut drop_ins = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        })
        .collect::<Vec<_>>();
    drop_ins.sort();
    sources.extend(drop_ins.iter().map(|path| path.display().to_string()));
    sources
}

/// 用户名下的 `CLAUDE_CONFIG_DIR`（`HKCU\Environment`）。
#[cfg(windows)]
pub fn user_environment_variable(name: &str) -> Option<String> {
    windows_registry::CURRENT_USER
        .open("Environment")
        .ok()?
        .get_string(name)
        .ok()
}

#[cfg(not(windows))]
pub fn user_environment_variable(_name: &str) -> Option<String> {
    None
}

#[cfg(windows)]
fn cli_policy_sources() -> Vec<String> {
    let mut sources = Vec::new();
    for (root, hive) in [
        (windows_registry::LOCAL_MACHINE, "HKLM"),
        (windows_registry::CURRENT_USER, "HKCU"),
    ] {
        let managed = root
            .open("SOFTWARE\\Policies\\ClaudeCode")
            .ok()
            .and_then(|key| key.get_string("Settings").ok())
            .is_some();
        if managed {
            sources.push(hive.to_owned() + "\\SOFTWARE\\Policies\\ClaudeCode");
        }
    }
    sources
}

#[cfg(not(windows))]
fn cli_policy_sources() -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
fn policy_values(root: &windows_registry::Key) -> Vec<String> {
    let Ok(key) = root.open("SOFTWARE\\Policies\\Claude") else {
        return Vec::new();
    };
    match key.values() {
        Ok(values) => values.map(|(name, _)| name).collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;
    use crate::claude_code::{Family, AUTH_TOKEN_KEY};

    fn env(pairs: &[(&str, &str)]) -> Value {
        let mut map = Map::new();
        for (key, value) in pairs {
            map.insert((*key).into(), Value::String((*value).into()));
        }
        serde_json::json!({ "env": map })
    }

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), Value::String((*value).to_owned())))
            .collect()
    }

    #[test]
    fn a_foreign_base_url_blocks_the_takeover() {
        let document = env(&[(BASE_URL_KEY, "https://api.anthropic.com")]);
        let conflicts = settings_conflicts(&document, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].code, "env_base_url");
        assert!(conflicts[0].blocking);
    }

    #[test]
    fn the_value_we_are_about_to_write_is_not_a_conflict() {
        let document = env(&[(BASE_URL_KEY, "http://127.0.0.1:43121")]);
        let desired = values(&[(BASE_URL_KEY, "http://127.0.0.1:43121")]);
        assert!(settings_conflicts(&document, &BTreeMap::new(), &desired).is_empty());
    }

    #[test]
    fn our_own_previous_values_are_not_conflicts() {
        let document = env(&[(BASE_URL_KEY, "http://127.0.0.1:43121")]);
        let owned = values(&[(BASE_URL_KEY, "http://127.0.0.1:43121")]);
        assert!(settings_conflicts(&document, &owned, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_user_edit_after_takeover_is_reported() {
        let document = env(&[(AUTH_TOKEN_KEY, "user-token")]);
        let owned = values(&[(AUTH_TOKEN_KEY, "gateway-token")]);
        let conflicts = settings_conflicts(&document, &owned, &BTreeMap::new());
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].code, "user_modified");
        assert!(conflicts[0].blocking);
    }

    #[test]
    fn api_keys_and_feature_flags_block_but_disabled_flags_do_not() {
        let document = env(&[
            (API_KEY_KEY, "sk-user"),
            (BEDROCK_KEY, "1"),
            (VERTEX_KEY, "false"),
        ]);
        let conflicts = settings_conflicts(&document, &BTreeMap::new(), &BTreeMap::new());
        let codes = conflicts.iter().map(|c| c.code).collect::<Vec<_>>();
        assert!(codes.contains(&"env_api_key"), "{codes:?}");
        assert!(codes.contains(&"env_feature_flag"), "{codes:?}");
        assert!(!conflicts
            .iter()
            .any(|c| c.key.as_deref() == Some(VERTEX_KEY)));
    }

    #[test]
    fn an_api_key_helper_blocks_the_takeover() {
        let document = serde_json::json!({ "apiKeyHelper": "/usr/local/bin/get-key" });
        let conflicts = settings_conflicts(&document, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].code, "api_key_helper");
    }

    #[test]
    fn a_non_object_env_blocks_instead_of_being_replaced() {
        let document = serde_json::json!({ "env": "legacy" });
        let conflicts = settings_conflicts(&document, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(conflicts[0].code, "env_type");
    }

    #[test]
    fn shell_variables_only_hint_and_secrets_stay_redacted() {
        let mut environment = CliEnvironment::default();
        environment.shell_env.insert(
            BASE_URL_KEY.into(),
            ShellVariable {
                value: "https://api.anthropic.com".into(),
                source: "user environment",
            },
        );
        environment.shell_env.insert(
            AUTH_TOKEN_KEY.into(),
            ShellVariable {
                value: "secret".into(),
                source: "process environment",
            },
        );
        let conflicts = shell_conflicts(&environment, &BTreeMap::new());
        assert_eq!(conflicts.len(), 2);
        assert!(conflicts.iter().all(|c| !c.blocking));
        let token = conflicts
            .iter()
            .find(|c| c.key.as_deref() == Some(AUTH_TOKEN_KEY))
            .unwrap();
        assert!(!token.message.contains("secret"), "{}", token.message);
    }

    #[test]
    fn families_are_recognised_from_display_names() {
        assert_eq!(Family::from_hint("Claude Opus 4"), Some(Family::Opus));
        assert_eq!(Family::from_hint("claude-sonnet-4-5"), Some(Family::Sonnet));
        assert_eq!(Family::from_hint("haiku"), Some(Family::Haiku));
        assert_eq!(Family::from_hint("deepseek"), None);
    }

    #[test]
    fn an_empty_managed_directory_is_not_a_policy() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("managed-settings.d")).unwrap();
        assert!(managed_settings_sources(Some(directory.path())).is_empty());
        assert!(managed_settings_sources(None).is_empty());
    }

    #[test]
    fn managed_settings_and_every_drop_in_file_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let drop_ins = directory.path().join("managed-settings.d");
        std::fs::create_dir_all(&drop_ins).unwrap();
        std::fs::write(directory.path().join("managed-settings.json"), "{}").unwrap();
        std::fs::write(drop_ins.join("20-company.json"), "{}").unwrap();
        std::fs::write(drop_ins.join("10-team.json"), "{}").unwrap();
        // 非 JSON 文件不参与合并，也不算受管来源。
        std::fs::write(drop_ins.join("README.md"), "notes").unwrap();

        let sources = managed_settings_sources(Some(directory.path()));
        let names = sources
            .iter()
            .map(|source| {
                std::path::Path::new(source)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["managed-settings.json", "10-team.json", "20-company.json"]
        );
    }

    #[test]
    fn a_single_drop_in_file_is_enough_to_mark_the_client_managed() {
        let directory = tempfile::tempdir().unwrap();
        let drop_ins = directory.path().join("managed-settings.d");
        std::fs::create_dir_all(&drop_ins).unwrap();
        std::fs::write(drop_ins.join("10-company.json"), "{}").unwrap();

        let environment = CliEnvironment {
            managed_sources: managed_settings_sources(Some(directory.path())),
            shell_env: BTreeMap::new(),
        };
        let conflicts = managed_conflicts(&environment);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].code, "managed_settings");
        assert!(conflicts[0].blocking);
        assert!(conflicts[0].message.contains("10-company.json"));
    }
}
