//! Claude Code 的用户设置（`~/.claude/settings.json`）只在需要长窗口时补一项
//! `modelSettings.<model>.autoCompactWindow`。
//!
//! Claude Desktop 的 profile 只能声明 1M 变体，不能声明任意窗口。声明 1M 后，
//! 内嵌 Claude Code 会按 1M 计算压缩；我们再把每个模型的自动压缩窗口改回它
//! 的真实上下文。这样既绕开 Desktop 的 200K 默认窗口，又不会把 1M 当成真实
//! 窗口发请求。

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{anthropic::models::discovery_id, Error, Result};

use super::desktop::DesktopModel;
use super::{
    atomic_write, backup_once, content_hash, parse_json, read_optional, remove_file_if_exists,
};

const RECEIPT_FILE: &str = "claude-code-settings.json";
const RECEIPT_VERSION: u32 = 1;
const MIN_AUTO_COMPACT_WINDOW: u64 = 100_000;
const MAX_AUTO_COMPACT_WINDOW: u64 = 1_000_000;
const LONG_CONTEXT_THRESHOLD: u64 = 200_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Receipt {
    version: u32,
    path: String,
    created: bool,
    previous: BTreeMap<String, Option<Value>>,
    applied_hash: String,
}

pub(crate) fn apply(state_dir: &Path, models: &[DesktopModel]) -> Result<()> {
    let path = settings_path()?;
    apply_at(state_dir, &path, models)
}

pub(crate) fn undo(state_dir: &Path) -> Result<()> {
    let path = settings_path()?;
    undo_at(state_dir, &path)
}

fn settings_path() -> Result<PathBuf> {
    if let Some(directory) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(directory).join("settings.json"));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve the user home directory".into()))?;
    Ok(home.join(".claude").join("settings.json"))
}

fn receipt_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RECEIPT_FILE)
}

fn auto_compact_window(model: &DesktopModel) -> Option<u64> {
    model
        .context_window_tokens
        .filter(|window| *window > LONG_CONTEXT_THRESHOLD)
        .map(|window| window.clamp(MIN_AUTO_COMPACT_WINDOW, MAX_AUTO_COMPACT_WINDOW))
}

fn model_id(model: &DesktopModel) -> String {
    discovery_id(&model.model_hash)
}

fn apply_at(state_dir: &Path, path: &Path, models: &[DesktopModel]) -> Result<()> {
    let raw = read_optional(path)?;
    let created = raw.is_none();
    let mut document = match raw.as_deref() {
        Some(text) => parse_json(text)
            .map_err(|error| Error::Config(format!("cannot parse {}: {error}", path.display())))?,
        None => Value::Object(Map::new()),
    };
    validate_document(&document, path)?;

    let previous_receipt = read_receipt(state_dir)?;
    let mut previous = previous_receipt
        .as_ref()
        .filter(|receipt| same_text_path(&receipt.path, path))
        .map(|receipt| receipt.previous.clone())
        .unwrap_or_default();

    let desired: BTreeSet<_> = models
        .iter()
        .filter(|model| auto_compact_window(model).is_some())
        .map(model_id)
        .collect();

    // A short-context-only selection is not a reason to create or rewrite the
    // user's Claude Code settings. If an earlier takeover is being replaced,
    // fall through so its old entries can be restored.
    if desired.is_empty() && previous_receipt.is_none() {
        return Ok(());
    }

    let model_settings = document
        .as_object_mut()
        .expect("validated as an object")
        .entry("modelSettings".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let model_settings = model_settings.as_object_mut().ok_or_else(|| {
        Error::Config(format!(
            "{}: modelSettings must be a JSON object",
            path.display()
        ))
    })?;

    // 先把不再接管的模型恢复原值，避免旧的 1M 压缩设置继续生效。
    for old_id in previous.keys().filter(|id| !desired.contains(*id)) {
        restore_entry(
            model_settings,
            old_id,
            previous.get(old_id).cloned().flatten(),
        );
    }
    previous.retain(|id, _| desired.contains(id));

    for model in models {
        let Some(window) = auto_compact_window(model) else {
            continue;
        };
        let id = model_id(model);
        if !previous.contains_key(&id) {
            let original = model_settings.get(&id).cloned();
            previous.insert(id.clone(), original);
        }
        let entry = model_settings
            .entry(id.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        let entry = entry.as_object_mut().ok_or_else(|| {
            Error::Config(format!(
                "{}: modelSettings.{id} must be a JSON object",
                path.display()
            ))
        })?;
        entry.insert("autoCompactWindow".to_owned(), Value::Number(window.into()));
    }

    if model_settings.is_empty() {
        document
            .as_object_mut()
            .expect("validated as an object")
            .remove("modelSettings");
    }

    let text = pretty(&document)?;
    if raw.as_deref() != Some(text.as_str()) {
        backup_once(path)?;
        atomic_write(path, text.as_bytes())?;
    }

    let receipt = Receipt {
        version: RECEIPT_VERSION,
        path: path.display().to_string(),
        created,
        previous,
        applied_hash: content_hash(&text),
    };
    write_receipt(state_dir, &receipt)
}

fn undo_at(state_dir: &Path, path: &Path) -> Result<()> {
    let Some(receipt) = read_receipt(state_dir)? else {
        return Ok(());
    };
    if !same_text_path(&receipt.path, path) {
        return Err(Error::Config(format!(
            "the Claude Code settings receipt belongs to {}; undo will not touch {}",
            receipt.path,
            path.display()
        )));
    }
    let Some(raw) = read_optional(path)? else {
        remove_file_if_exists(&receipt_path(state_dir))?;
        return Ok(());
    };
    let mut document = parse_json(&raw)
        .map_err(|error| Error::Config(format!("cannot parse {}: {error}", path.display())))?;
    validate_document(&document, path)?;
    let model_settings = document
        .as_object_mut()
        .expect("validated as an object")
        .entry("modelSettings".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let model_settings = model_settings.as_object_mut().ok_or_else(|| {
        Error::Config(format!(
            "{}: modelSettings must be a JSON object",
            path.display()
        ))
    })?;
    for (id, previous) in receipt.previous {
        restore_entry(model_settings, &id, previous);
    }
    if model_settings.is_empty() {
        document
            .as_object_mut()
            .expect("validated as an object")
            .remove("modelSettings");
    }
    let text = pretty(&document)?;
    if receipt.created && document.as_object().is_some_and(Map::is_empty) {
        remove_file_if_exists(path)?;
    } else if raw != text {
        backup_once(path)?;
        atomic_write(path, text.as_bytes())?;
    }
    remove_file_if_exists(&receipt_path(state_dir))
}

fn restore_entry(model_settings: &mut Map<String, Value>, id: &str, previous: Option<Value>) {
    match previous {
        Some(value) => {
            model_settings.insert(id.to_owned(), value);
        }
        None => {
            model_settings.remove(id);
        }
    }
}

fn validate_document(document: &Value, path: &Path) -> Result<()> {
    if !document.is_object() {
        return Err(Error::Config(format!(
            "{} must contain a JSON object",
            path.display()
        )));
    }
    Ok(())
}

fn read_receipt(state_dir: &Path) -> Result<Option<Receipt>> {
    let Some(text) = read_optional(&receipt_path(state_dir))? else {
        return Ok(None);
    };
    let receipt = serde_json::from_str(&text)
        .map_err(|error| Error::Config(format!("invalid Claude Code settings receipt: {error}")))?;
    Ok(Some(receipt))
}

fn write_receipt(state_dir: &Path, receipt: &Receipt) -> Result<()> {
    let text = pretty(&serde_json::to_value(receipt)?)?;
    atomic_write(&receipt_path(state_dir), text.as_bytes())
}

fn pretty(document: &Value) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(document)?))
}

fn same_text_path(left: &str, right: &Path) -> bool {
    let left = Path::new(left);
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_client::Family;

    fn model(hash: &str, window: Option<u64>) -> DesktopModel {
        DesktopModel {
            model_hash: hash.into(),
            label: Some(hash.into()),
            tier: Some(Family::Sonnet),
            context_window_tokens: window,
        }
    }

    fn write(path: &Path, value: Value) {
        std::fs::write(path, pretty(&value).unwrap()).unwrap();
    }

    #[test]
    fn long_context_models_get_a_real_auto_compact_window_and_other_keys_survive() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let path = directory.path().join("settings.json");
        write(
            &path,
            serde_json::json!({
                "outputStyle": "Proactive",
                "modelSettings": {
                    "claude-other": { "effortLevel": "high" }
                }
            }),
        );

        apply_at(&state, &path, &[model("hash", Some(600_000))]).unwrap();
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document["outputStyle"], "Proactive");
        assert_eq!(
            document["modelSettings"]["claude-other"]["effortLevel"],
            "high"
        );
        assert_eq!(
            document["modelSettings"]["claude-hash"]["autoCompactWindow"],
            600_000
        );

        undo_at(&state, &path).unwrap();
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["modelSettings"]["claude-other"]["effortLevel"],
            "high"
        );
        assert!(document["modelSettings"].get("claude-hash").is_none());
        assert_eq!(document["outputStyle"], "Proactive");
    }

    #[test]
    fn short_context_models_do_not_create_a_claude_settings_file() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let path = directory.path().join("settings.json");

        apply_at(&state, &path, &[model("short", Some(128_000))]).unwrap();
        assert!(!path.exists());
        undo_at(&state, &path).unwrap();
    }

    #[test]
    fn reapply_replaces_old_window_and_undo_restores_original_value() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let path = directory.path().join("settings.json");
        write(
            &path,
            serde_json::json!({
                "modelSettings": {
                    "claude-hash": { "autoCompactWindow": 300_000, "effortLevel": "high" }
                }
            }),
        );

        apply_at(&state, &path, &[model("hash", Some(600_000))]).unwrap();
        apply_at(&state, &path, &[model("hash", Some(500_000))]).unwrap();
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["modelSettings"]["claude-hash"]["autoCompactWindow"],
            500_000
        );
        undo_at(&state, &path).unwrap();
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["modelSettings"]["claude-hash"]["autoCompactWindow"],
            300_000
        );
        assert_eq!(
            document["modelSettings"]["claude-hash"]["effortLevel"],
            "high"
        );
    }
}
