//! Codex 用户级配置的字段所有权；拒绝覆盖外部路由与受管字段冲突。
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use toml_edit::{value, DocumentMut, Item};
const RECEIPT: &str = ".haxsd-byok-routing.json";
const PROVIDER: &str = "haxsd-byok";
const KEYS: &[&str] = &[
    "model",
    "model_provider",
    "model_catalog_json",
    "model_context_window",
    "model_auto_compact_token_limit",
    "web_search",
];

pub struct ConfigRoute {
    pub model: String,
    pub base_url: String,
    pub token: String,
    pub catalog_path: PathBuf,
    pub context_window: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct ConfigStatus {
    pub home: String,
    pub config_path: String,
    pub applied: bool,
    pub conflict: bool,
    pub message: String,
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u32,
    original: Option<String>,
    applied: String,
}

pub fn default_home() -> Result<PathBuf> {
    let home = if let Some(home) = std::env::var_os("CODEX_HOME") {
        PathBuf::from(home)
    } else {
        dirs::home_dir()
            .ok_or_else(|| Error::Config("cannot resolve Codex home".into()))?
            .join(".codex")
    };
    absolute(&home)?;
    Ok(home)
}
fn absolute(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::Config("Codex paths must be absolute".into()));
    }
    Ok(())
}
fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn parse(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .map_err(|e| Error::Config(format!("invalid Codex TOML: {e}")))
}
fn receipt(home: &Path) -> Result<Option<Receipt>> {
    read(&home.join(RECEIPT))?
        .map(|text| {
            let receipt: Receipt = serde_json::from_str(&text)?;
            if receipt.version != 1 {
                return Err(Error::Config("unsupported Codex restore receipt".into()));
            }
            Ok(receipt)
        })
        .transpose()
}
fn managed(doc: &DocumentMut) -> Vec<Option<String>> {
    let mut values = KEYS
        .iter()
        .map(|key| doc.get(key).map(Item::to_string))
        .collect::<Vec<_>>();
    values.push(
        doc.get("model_providers")
            .and_then(|i| i.get(PROVIDER))
            .map(Item::to_string),
    );
    values
}
fn external(doc: &DocumentMut) -> bool {
    doc.get("model_provider")
        .and_then(Item::as_str)
        .is_some_and(|p| p != "openai")
        || doc.get("openai_base_url").is_some()
        || doc.get("profile").is_some()
        || doc
            .get("model_providers")
            .and_then(|i| i.get(PROVIDER))
            .is_some()
}
pub fn inspect(home: &Path) -> Result<ConfigStatus> {
    absolute(home)?;
    let path = home.join("config.toml");
    let text = read(&path)?;
    let doc = parse(text.as_deref().unwrap_or(""))?;
    let (applied, conflict) = if let Some(receipt) = receipt(home)? {
        let applied = parse(&receipt.applied)?;
        let original = parse(receipt.original.as_deref().unwrap_or(""))?;
        let own = managed(&doc) == managed(&applied);
        (own, !own && managed(&doc) != managed(&original))
    } else {
        (false, external(&doc))
    };
    Ok(ConfigStatus {
        home: home.display().to_string(),
        config_path: path.display().to_string(),
        applied,
        conflict,
        message: if conflict {
            "Codex routing is managed by another configuration or was modified"
        } else if applied {
            "Codex configuration is applied"
        } else {
            "Codex configuration is not applied"
        }
        .into(),
    })
}
pub fn apply(home: &Path, route: &ConfigRoute) -> Result<ConfigStatus> {
    absolute(home)?;
    absolute(&route.catalog_path)?;
    let url = url::Url::parse(&route.base_url).map_err(|e| Error::Config(e.to_string()))?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || route.context_window < 4096
        || route.context_window > i64::MAX as u64
        || route.token.is_empty()
        || route.model.is_empty()
    {
        return Err(Error::Config("invalid local Codex route".into()));
    }
    fs::create_dir_all(home)?;
    let path = home.join("config.toml");
    let original = read(&path)?;
    let mut doc = parse(original.as_deref().unwrap_or(""))?;
    if let Some(receipt) = receipt(home)? {
        let current = inspect(home)?;
        if current.applied {
            let desired_provider = doc["model_providers"][PROVIDER].clone();
            if desired_provider.get("base_url").and_then(Item::as_str)
                != Some(route.base_url.as_str())
                || doc["model"].as_str() != Some(route.model.as_str())
                || doc["model_catalog_json"].as_str()
                    != Some(route.catalog_path.to_string_lossy().as_ref())
                || doc["model_context_window"].as_integer() != Some(route.context_window as i64)
                || desired_provider
                    .get("experimental_bearer_token")
                    .and_then(Item::as_str)
                    != Some(route.token.as_str())
            {
                return Err(Error::Config(
                    "restore the existing Codex route before changing it".into(),
                ));
            }
            return Ok(current);
        }
        if managed(&doc) != managed(&parse(receipt.original.as_deref().unwrap_or(""))?) {
            return Err(Error::Config(
                "Codex managed configuration was modified; refusing to overwrite".into(),
            ));
        }
        fs::remove_file(home.join(RECEIPT))?;
    }
    if external(&doc) {
        return Err(Error::Config(
            "Codex already selects another provider, base URL or profile; refusing to overwrite"
                .into(),
        ));
    }
    doc["model"] = value(route.model.as_str());
    doc["web_search"] = value("disabled");
    doc["model_provider"] = value(PROVIDER);
    doc["model_catalog_json"] = value(route.catalog_path.to_string_lossy().as_ref());
    doc["model_context_window"] = value(route.context_window as i64);
    doc["model_auto_compact_token_limit"] = value((route.context_window * 4 / 5) as i64);
    doc["model_providers"][PROVIDER]["name"] = value("haxsd byok");
    doc["model_providers"][PROVIDER]["base_url"] = value(route.base_url.as_str());
    doc["model_providers"][PROVIDER]["wire_api"] = value("responses");
    doc["model_providers"][PROVIDER]["requires_openai_auth"] = value(false);
    doc["model_providers"][PROVIDER]["supports_websockets"] = value(false);
    doc["model_providers"][PROVIDER]["experimental_bearer_token"] = value(route.token.as_str());
    doc["model_providers"][PROVIDER]["capabilities"]["remote_compaction"] = value("unsupported");
    let applied = doc.to_string();
    if read(&path)? != original {
        return Err(Error::Config(
            "Codex config changed while preparing the update".into(),
        ));
    }
    atomic_write(
        &home.join(RECEIPT),
        &serde_json::to_vec(&Receipt {
            version: 1,
            original: original.clone(),
            applied: applied.clone(),
        })?,
    )?;
    if read(&path)? != original {
        return Err(Error::Config(
            "Codex config changed before replacement; restore receipt retained".into(),
        ));
    }
    atomic_write(&path, applied.as_bytes())?;
    inspect(home)
}
pub fn restore(home: &Path) -> Result<ConfigStatus> {
    absolute(home)?;
    let Some(receipt) = receipt(home)? else {
        return inspect(home);
    };
    let path = home.join("config.toml");
    let current = read(&path)?;
    let mut doc = parse(current.as_deref().unwrap_or(""))?;
    let applied = parse(&receipt.applied)?;
    let original = parse(receipt.original.as_deref().unwrap_or(""))?;
    if managed(&doc) == managed(&original) {
        fs::remove_file(home.join(RECEIPT))?;
        return inspect(home);
    }
    if managed(&doc) != managed(&applied) {
        return Err(Error::Config(
            "Codex managed fields changed after apply; refusing to restore over external edits"
                .into(),
        ));
    }
    if current.as_deref() == Some(receipt.applied.as_str()) {
        if let Some(original) = receipt.original {
            atomic_write(&path, original.as_bytes())?;
        } else {
            fs::remove_file(&path)?;
        }
    } else {
        for key in KEYS {
            if let Some(item) = original.get(key) {
                doc[key] = item.clone();
            } else {
                doc.remove(key);
            }
        }
        if let Some(providers) = doc
            .get_mut("model_providers")
            .and_then(Item::as_table_like_mut)
        {
            if let Some(item) = original
                .get("model_providers")
                .and_then(|i| i.get(PROVIDER))
            {
                providers.insert(PROVIDER, item.clone());
            } else {
                providers.remove(PROVIDER);
            }
        }
        if original.get("model_providers").is_none()
            && doc
                .get("model_providers")
                .and_then(Item::as_table_like)
                .is_some_and(|t| t.is_empty())
        {
            doc.remove("model_providers");
        }
        if read(&path)? != current {
            return Err(Error::Config("Codex config changed during restore".into()));
        }
        atomic_write(&path, doc.to_string().as_bytes())?;
    }
    fs::remove_file(home.join(RECEIPT))?;
    inspect(home)
}
pub(super) fn applied_catalog(home: &Path) -> Result<Option<serde_json::Value>> {
    use sha2::{Digest, Sha256};
    let status = inspect(home)?;
    if status.conflict {
        return Err(Error::Config(
            "Codex managed configuration has external changes".into(),
        ));
    }
    if !status.applied {
        return Ok(None);
    }
    let doc = parse(&fs::read_to_string(home.join("config.toml"))?)?;
    let path = PathBuf::from(
        doc["model_catalog_json"]
            .as_str()
            .ok_or_else(|| Error::Config("missing Codex model catalog".into()))?,
    );
    let bytes = fs::read(&path)?;
    let expected = format!(
        "haxsd-byok-models-{}.json",
        hex::encode(Sha256::digest(&bytes))
    );
    if path.parent() != Some(home)
        || path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str())
    {
        return Err(Error::Config(
            "Codex model catalog was modified outside this application".into(),
        ));
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(super) fn write_catalog(home: &Path, bytes: &[u8]) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    absolute(home)?;
    let digest = hex::encode(Sha256::digest(bytes));
    let path = home.join(format!("haxsd-byok-models-{digest}.json"));
    fs::create_dir_all(home)?;
    let mut file = tempfile::NamedTempFile::new_in(home)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    if let Err(error) = file.persist_noclobber(&path) {
        if error.error.kind() != std::io::ErrorKind::AlreadyExists || fs::read(&path)? != bytes {
            return Err(Error::Config(
                "Codex model catalog path is occupied by another file".into(),
            ));
        }
    }
    Ok(path)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config("Codex path has no parent".into()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| Error::Io(error.error))?;
    Ok(())
}
