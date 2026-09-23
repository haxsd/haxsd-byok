//! Integrates local application settings.
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::{Error, Result};

const NO_PROXY_KEY: &str = "http.noProxy";
const KEYS: [&str; 5] = [
    "http.proxy",
    "http.proxyKerberosServicePrincipal",
    "http.proxySupport",
    "cursor.general.disableHttp2",
    "http.experimental.systemCertificatesV2",
];

/// Marks the proxy entries this product wrote, so ownership is recorded rather
/// than guessed.
///
/// Recognising our leftovers by their shape cannot work here: a sibling product on
/// the same machine (Cursor BYOK) is a fork of this code base and writes these same
/// five keys with the same values, pointing at loopback. The previous heuristic
/// therefore could not tell its configuration from ours, and every launch of this
/// app deleted it, which broke that product's Cursor integration until it rewrote
/// its own settings. Anything without this marker belongs to somebody else and is
/// left exactly as it is.
const MANAGED_MARKER_KEY: &str = "haxsd-byok.managedProxy";

fn path() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve user home directory".into()))?;
    match std::env::consts::OS {
        "macos" => Ok(home.join("Library/Application Support/Cursor/User/settings.json")),
        "windows" => Ok(std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("Cursor/User/settings.json")),
        "linux" => Ok(std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("Cursor/User/settings.json")),
        platform => Err(Error::Config(format!(
            "Cursor settings are unsupported on {platform}"
        ))),
    }
}

fn read_from(path: &Path) -> Result<BTreeMap<String, Value>> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    if data.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    json5::from_str(&data)
        .map_err(|error| Error::Config(format!("parse Cursor settings JSONC: {error}")))
}

fn write_to(path: &Path, settings: &BTreeMap<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(settings)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, [data.as_slice(), b"\n"].concat())?;
    fs::rename(temp, path)?;
    Ok(())
}

pub fn write_proxy_settings(proxy_url: &str) -> Result<()> {
    write_proxy_settings_at(&path()?, proxy_url)
}

fn write_proxy_settings_at(path: &Path, proxy_url: &str) -> Result<()> {
    let mut settings = read_from(path)?;
    settings.remove(NO_PROXY_KEY);
    settings.insert(KEYS[0].into(), Value::String(proxy_url.into()));
    settings.insert(KEYS[1].into(), Value::String(proxy_url.into()));
    settings.insert(KEYS[2].into(), Value::String("on".into()));
    settings.insert(KEYS[3].into(), Value::Bool(true));
    settings.insert(KEYS[4].into(), Value::Bool(true));
    settings.insert(MANAGED_MARKER_KEY.into(), Value::String(proxy_url.into()));
    write_to(path, &settings)
}

pub fn clear_proxy_settings() -> Result<()> {
    clear_proxy_settings_at(&path()?)
}

/// Removes the entries this product wrote, marker included.
///
/// Guarded by the marker for the same reason the startup cleanup is: if another
/// product has since claimed these keys, they are no longer ours to delete.
fn clear_proxy_settings_at(path: &Path) -> Result<()> {
    let mut settings = read_from(path)?;
    if !mentions_us(&settings) {
        return Ok(());
    }
    let before = settings.len();
    for key in KEYS {
        settings.remove(key);
    }
    settings.remove(MANAGED_MARKER_KEY);
    if settings.len() != before {
        write_to(path, &settings)?;
    }
    Ok(())
}

pub fn settings_match(proxy_url: &str) -> Result<bool> {
    let settings = read_from(&path()?)?;
    Ok(is_our_configuration(&settings, proxy_url))
}

/// 文件里现在的代理配置是不是**别人写的**。
///
/// 判据只看一项：`http.proxy` 与我们留下的标记是否一致。
///
/// - 没有 `http.proxy`：谁都没占用，可以写；
/// - 标记与它一致：这是我们自己写的（值可能已经过期，比如代理换了端口），可以改写；
/// - 没有标记，或标记与它不一致：有人在我们之后动过这几个键。同一个代码库分叉出来的
///   兄弟产品就在同一台机器上，它写的是同样的五个键，而且会**保留不认识的键**（我们的
///   标记），所以「标记还在」不等于「值还是我们的」——必须比较两者的当前值。
///
/// 认出这种情况后调用方会停手：两个软件轮流覆盖同一份配置，只会让用户的两条链路
/// 都不稳定，该由用户自己决定留哪一个。
pub fn proxy_configuration_is_foreign() -> Result<bool> {
    Ok(is_foreign_configuration(&read_from(&path()?)?))
}

fn is_foreign_configuration(settings: &BTreeMap<String, Value>) -> bool {
    let Some(proxy) = settings.get(KEYS[0]) else {
        return false;
    };
    !matches!(settings.get(MANAGED_MARKER_KEY), Some(marker) if marker == proxy)
}

fn is_our_configuration(settings: &BTreeMap<String, Value>, proxy_url: &str) -> bool {
    settings.get(KEYS[0]) == Some(&Value::String(proxy_url.into()))
        && settings.get(KEYS[1]) == Some(&Value::String(proxy_url.into()))
        && settings.get(KEYS[2]) == Some(&Value::String("on".into()))
        && settings.get(KEYS[3]) == Some(&Value::Bool(true))
        && settings.get(KEYS[4]) == Some(&Value::Bool(true))
}

/// Whether this file carries the marker proving this product wrote it.
fn mentions_us(settings: &BTreeMap<String, Value>) -> bool {
    settings.contains_key(MANAGED_MARKER_KEY)
}

/// Drops the proxy entries a previous run of this product left behind.
///
/// A file without the marker is not ours, whatever it looks like, and is never
/// touched. That is the whole point: the sibling product's configuration is
/// indistinguishable by content.
pub fn clear_stale_managed_settings() -> Result<()> {
    clear_proxy_settings()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Exactly what the sibling Cursor BYOK product writes. Every key of it must
    /// survive, because deleting it takes that product's Cursor integration down.
    const SIBLING_PRODUCT_SETTINGS: &str = r#"{
  "cursor.general.disableHttp2": true,
  "http.experimental.systemCertificatesV2": true,
  "http.proxy": "http://127.0.0.1:15524",
  "http.proxyKerberosServicePrincipal": "http://127.0.0.1:15524",
  "http.proxySupport": "on",
  "window.autoDetectColorScheme": true
}"#;

    fn settings_path(directory: &tempfile::TempDir) -> PathBuf {
        directory.path().join("settings.json")
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn another_products_configuration_is_left_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(&path, SIBLING_PRODUCT_SETTINGS).unwrap();

        clear_proxy_settings_at(&path).unwrap();

        let settings = read(&path);
        assert_eq!(settings["http.proxy"], json!("http://127.0.0.1:15524"));
        assert_eq!(settings["http.proxySupport"], json!("on"));
        assert_eq!(settings["cursor.general.disableHttp2"], json!(true));
        assert_eq!(
            settings["http.experimental.systemCertificatesV2"],
            json!(true)
        );
        assert_eq!(
            settings["http.proxyKerberosServicePrincipal"],
            json!("http://127.0.0.1:15524")
        );
        assert_eq!(settings["window.autoDetectColorScheme"], json!(true));
    }

    #[test]
    fn our_own_leftovers_are_removed() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(&path, r#"{"editor.fontSize": 14}"#).unwrap();

        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();
        assert!(mentions_us(&read_from(&path).unwrap()));

        clear_proxy_settings_at(&path).unwrap();

        let settings = read(&path);
        assert_eq!(settings, json!({ "editor.fontSize": 14 }));
    }

    #[test]
    fn a_clear_leaves_configuration_this_product_did_not_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(&path, SIBLING_PRODUCT_SETTINGS).unwrap();

        clear_proxy_settings_at(&path).unwrap();

        assert_eq!(read(&path)["http.proxy"], json!("http://127.0.0.1:15524"));
        assert_eq!(read(&path)["http.proxySupport"], json!("on"));
    }

    #[test]
    fn written_settings_are_recognised_as_ours() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(&path, "{}").unwrap();

        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();

        assert!(is_our_configuration(
            &read_from(&path).unwrap(),
            "http://127.0.0.1:1634"
        ));
        assert!(!is_our_configuration(
            &read_from(&path).unwrap(),
            "http://127.0.0.1:1"
        ));
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);

        clear_proxy_settings_at(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn the_sibling_products_configuration_is_recognised_as_foreign() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(&path, SIBLING_PRODUCT_SETTINGS).unwrap();

        assert!(is_foreign_configuration(&read_from(&path).unwrap()));
    }

    #[test]
    fn our_own_configuration_is_never_foreign_even_when_the_port_changed() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();

        // 换端口之后 http.proxy 与「当前代理地址」不一致，但那仍然是我们写的：
        // 标记与文件里的值一致，所以可以放心改写。
        assert!(!is_foreign_configuration(&read_from(&path).unwrap()));
        assert!(!is_our_configuration(
            &read_from(&path).unwrap(),
            "http://127.0.0.1:1635"
        ));
    }

    #[test]
    fn another_product_rewriting_the_keys_makes_the_file_foreign_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();

        // 兄弟产品与我们同源：它认不出我们的标记，会把它原样留在文件里，只改那五个键。
        let mut settings = read_from(&path).unwrap();
        settings.insert(KEYS[0].into(), json!("http://127.0.0.1:15524"));
        settings.insert(KEYS[1].into(), json!("http://127.0.0.1:15524"));
        write_to(&path, &settings).unwrap();

        assert!(is_foreign_configuration(&read_from(&path).unwrap()));
    }

    #[test]
    fn a_file_without_proxy_entries_is_not_foreign() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(&path, r#"{"editor.fontSize": 14}"#).unwrap();

        assert!(!is_foreign_configuration(&read_from(&path).unwrap()));
    }
}
