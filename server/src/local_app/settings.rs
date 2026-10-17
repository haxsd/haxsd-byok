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

/// 接管期间由我们替用户保管的原始代理设置：`用户键 -> 保管键`。
///
/// 这六个键接管时都必须让位：`http.noProxy` 里的例外会让 Cursor 绕过本机代理直连厂商
/// （接管就失效了），另外五个是它读代理配置的开关与地址，我们要换成自己的。可它们记的是
/// 用户自己的公司代理、Kerberos 主体、代理调试开关与证书策略——VS Code/Cursor 不备份
/// `settings.json`，覆盖之后再删掉就只剩用户的记忆。所以照 `http.noProxy` 早就定下的
/// 办法办：认领配置时把原值挪进带标记的保管键，撤回时"文件里没有才还"。用户本来没有的
/// 键不写保管键，免得在用户文件里留垃圾键。
const CUSTODY_KEYS: [(&str, &str); 6] = [
    (KEYS[0], MANAGED_HTTP_PROXY_KEY),
    (KEYS[1], MANAGED_KERBEROS_PRINCIPAL_KEY),
    (KEYS[2], MANAGED_PROXY_SUPPORT_KEY),
    (KEYS[3], MANAGED_DISABLE_HTTP2_KEY),
    (KEYS[4], MANAGED_SYSTEM_CERTIFICATES_KEY),
    (NO_PROXY_KEY, MANAGED_NO_PROXY_KEY),
];

const MANAGED_HTTP_PROXY_KEY: &str = "haxsd-byok.managedHttpProxy";
const MANAGED_KERBEROS_PRINCIPAL_KEY: &str = "haxsd-byok.managedProxyKerberosServicePrincipal";
const MANAGED_PROXY_SUPPORT_KEY: &str = "haxsd-byok.managedProxySupport";
const MANAGED_DISABLE_HTTP2_KEY: &str = "haxsd-byok.managedDisableHttp2";
const MANAGED_SYSTEM_CERTIFICATES_KEY: &str = "haxsd-byok.managedSystemCertificatesV2";

/// 接管期间由我们替用户保管的 `http.noProxy` 原值。见 `CUSTODY_KEYS`。
const MANAGED_NO_PROXY_KEY: &str = "haxsd-byok.managedNoProxy";

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
        Err(error) => return Err(settings_file_error("read", path, &error)),
    };
    if data.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    // 这句会进界面提示（读状态失败时会跳一条错误提示）：settings.json 是用户手写的
    // 文件，得说清是哪个文件解析不了、他才知道去改哪里。
    json5::from_str(&data)
        .map_err(|error| Error::Config(format!("cannot parse {}: {error}", path.display())))
}

/// 读写 Cursor 设置文件失败的统一说法：带上路径。
///
/// `Access is denied. (os error 5)` 本身说不出是哪个文件——我们碰的文件不止一个，
/// 而这句会直接进界面提示。
fn settings_file_error(operation: &str, path: &Path, error: &std::io::Error) -> Error {
    Error::Config(format!("cannot {operation} {}: {error}", path.display()))
}

fn write_to(path: &Path, settings: &BTreeMap<String, Value>) -> Result<()> {
    back_up_once(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| settings_file_error("write", path, &error))?;
    }
    let data = serde_json::to_vec_pretty(settings)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, [data.as_slice(), b"\n"].concat())
        .map_err(|error| settings_file_error("write", &temp, &error))?;
    fs::rename(&temp, path).map_err(|error| settings_file_error("write", path, &error))?;
    Ok(())
}

/// 用户手写文件的备份路径：`settings.json` → `settings.json.haxsd-byok.bak`。
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".{BACKUP_SUFFIX}"));
    path.with_file_name(name)
}

const BACKUP_SUFFIX: &str = "haxsd-byok.bak";

/// 第一次改写用户文件之前，把原文整份留一份备份（只留一次）。
///
/// `settings.json` 是用户手写的：我们用 `json5` **读**它（注释、尾逗号、单引号都合法），
/// 却用 `serde_json::to_vec_pretty` 把一张 `BTreeMap` 写回去——注释、键序、数字格式、尾随
/// 换行全部消失，第一次接管就会发生。真正保留原文要写"只替换键值"的文本补丁（要处理缩进、
/// 注释归属与尾逗号），风险不小；最小且可回退的做法是先留一份备份，并在日志里说清楚它在哪。
fn back_up_once(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let backup = backup_path(path);
    if backup.exists() {
        return Ok(());
    }
    fs::copy(path, &backup).map_err(|error| settings_file_error("back up", path, &error))?;
    tracing::info!(
        backup = %backup.display(),
        "kept a copy of the user's settings.json before rewriting it"
    );
    Ok(())
}

pub fn write_proxy_settings(proxy_url: &str) -> Result<()> {
    write_proxy_settings_at(&path()?, proxy_url)
}

fn write_proxy_settings_at(path: &Path, proxy_url: &str) -> Result<()> {
    let settings = read_from(path)?;
    // 标记也要在：兄弟产品写的是同样的五个键，只比较那几个值会把它的配置
    // 误当成"我们已经写好了"，于是永远不会认领归我们。
    //
    // `http.noProxy` 不参与这个判断：文件里带着我们的标记时又出现这个键，只能说明接管
    // 之后有人（用户自己，或 Cursor 把内存里那一份写回来）写了它——那是用户的设置，不是
    // "这次写入还没生效"的证据。以前那句 `!settings.contains_key(NO_PROXY_KEY)` 会让每次
    // 写入都把它再删一遍。
    let already_written = settings.get(MANAGED_MARKER_KEY)
        == Some(&Value::String(proxy_url.into()))
        && is_our_configuration(&settings, proxy_url);
    if already_written {
        // 读状态每几秒就走一次这条路：内容没变就不写文件，免得白改 mtime
        // 让 Cursor 反复重载配置。
        return Ok(());
    }
    let mut settings = settings;
    take_custody(&mut settings);
    settings.insert(KEYS[0].into(), Value::String(proxy_url.into()));
    settings.insert(KEYS[1].into(), Value::String(proxy_url.into()));
    settings.insert(KEYS[2].into(), Value::String("on".into()));
    settings.insert(KEYS[3].into(), Value::Bool(true));
    settings.insert(KEYS[4].into(), Value::Bool(true));
    settings.insert(MANAGED_MARKER_KEY.into(), Value::String(proxy_url.into()));
    write_to(path, &settings)?;
    tracing::info!(
        path = %path.display(),
        proxy_url,
        "wrote Cursor proxy configuration"
    );
    Ok(())
}

pub fn clear_proxy_settings() -> Result<bool> {
    clear_proxy_settings_at(&path()?)
}

/// 复制一份 Cursor 设置里与代理有关的键，给诊断快照用。
///
/// 报错时最先要回答的就是"那份 settings.json 当时是谁写的、写的什么"。
pub fn proxy_settings_snapshot() -> Result<Value> {
    let path = path()?;
    let settings = read_from(&path)?;
    let mut entries = serde_json::Map::new();
    for key in KEYS
        .into_iter()
        .chain(CUSTODY_KEYS.iter().map(|(_, custody)| *custody))
        .chain([MANAGED_MARKER_KEY, NO_PROXY_KEY])
    {
        if let Some(value) = settings.get(key) {
            entries.insert(key.to_owned(), value.clone());
        }
    }
    Ok(serde_json::json!({
        "path": path.display().to_string(),
        "entries": entries,
        "is_foreign": is_foreign_configuration(&settings),
    }))
}

/// Removes the entries this product wrote, marker included.
///
/// Guarded by the marker for the same reason the startup cleanup is: if another
/// product has since claimed these keys, they are no longer ours to delete.
fn clear_proxy_settings_at(path: &Path) -> Result<bool> {
    let mut settings = read_from(path)?;
    // 保管键也要能单独认出来：兄弟产品清理自己的残留时会删掉那五个键与标记键，但认得
    // 我们的保管键（它不认识，所以原样留着），此时用户的原值只存在保管键里，必须还回去。
    if !mentions_us(&settings) && !holds_custody(&settings) {
        return Ok(false);
    }
    // 标记还在，但文件里的值已经不是我们写的那一份：同机的兄弟产品、公司代理或用户
    // 自己接管了这几个键（`is_foreign_configuration` 认的就是这件事，读状态也按它停手）。
    // 这时候撤配置等于删掉别人正在用的代理，对方的客户端会立刻失去代理——标记只说明
    // "这里曾经由我们写过"，撤配置只认当前值。
    if is_foreign_configuration(&settings) {
        return Ok(false);
    }
    let before = settings.clone();
    for key in KEYS {
        settings.remove(key);
    }
    settings.remove(MANAGED_MARKER_KEY);
    give_back_custody(&mut settings);
    // 按内容而不是长度判断"有没有改动"：撤回时可能正好是一还一删（保管键换成
    // `http.noProxy`），长度不变但文件确实变了。
    if settings == before {
        return Ok(false);
    }
    write_to(path, &settings)?;
    tracing::info!(
        path = %path.display(),
        removed = before.len() - settings.len(),
        "cleared Cursor proxy configuration"
    );
    Ok(true)
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

/// 认领一份 Cursor 设置时，把用户原来的代理相关设定挪进保管键里（见 `CUSTODY_KEYS`）。
///
/// **只在还没有我们标记的文件上收。** 带着标记就说明这份配置已经在接管中，此时文件里
/// 还有这些键，一定是接管之后被写进来的（用户自己设的，或 Cursor 把内存里那一份写回来）
/// ——那是用户的设置，归用户：不能收第二次，撤回时也不拿我们保管的旧值覆盖它。
fn take_custody(settings: &mut BTreeMap<String, Value>) {
    if mentions_us(settings) {
        return;
    }
    for (key, custody) in CUSTODY_KEYS {
        if let Some(value) = settings.remove(key) {
            settings.insert(custody.into(), value);
        }
    }
}

/// 撤回配置时把保管的用户原值原样还回去。
///
/// 文件里已经有这个键时不还：那是接管之后写的值，归用户，我们只把自己的保管记录丢掉。
fn give_back_custody(settings: &mut BTreeMap<String, Value>) {
    for (key, custody) in CUSTODY_KEYS {
        let held = settings.remove(custody);
        if settings.contains_key(key) {
            continue;
        }
        if let Some(value) = held {
            settings.insert(key.into(), value);
        }
    }
}

/// 文件里是否还有我们留下的保管记录（原值只存在保管键里的情况）。
fn holds_custody(settings: &BTreeMap<String, Value>) -> bool {
    CUSTODY_KEYS
        .iter()
        .any(|(_, custody)| settings.contains_key(*custody))
}

/// Drops the proxy entries a previous run of this product left behind.
///
/// A file without the marker is not ours, whatever it looks like, and is never
/// touched. That is the whole point: the sibling product's configuration is
/// indistinguishable by content.
pub fn clear_stale_managed_settings() -> Result<()> {
    clear_proxy_settings().map(|_| ())
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

    /// 用户手写的 settings.json 有语法错误（或者读不了）时，提示必须说清是哪个文件：
    /// 界面只会显示这句话，而 `Access is denied` / `expected value` 本身指不出文件。
    #[test]
    fn an_unusable_settings_file_is_reported_with_its_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(&path, "{ \"editor.fontSize\": }").unwrap();

        let error = clear_proxy_settings_at(&path).unwrap_err().to_string();

        assert!(error.contains("settings.json"), "{error}");
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

    /// 接管期间另一个同类软件改写了那五个键（我们的标记被它原样留下）：退出时不能
    /// 把这些键删掉，否则它的 Cursor 配置会当场消失，客户端只能直连厂商。
    #[test]
    fn a_clear_leaves_the_keys_alone_once_another_writer_claimed_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();

        // 兄弟产品与我们同源：它认不出我们的标记，把它留在文件里，只改那五个键。
        let mut settings = read_from(&path).unwrap();
        settings.insert(KEYS[0].into(), json!("http://127.0.0.1:15524"));
        settings.insert(KEYS[1].into(), json!("http://127.0.0.1:15524"));
        write_to(&path, &settings).unwrap();

        assert!(
            !clear_proxy_settings_at(&path).unwrap(),
            "值已经不是我们的，撤配置必须停手"
        );
        let settings = read(&path);
        assert_eq!(settings["http.proxy"], json!("http://127.0.0.1:15524"));
        assert_eq!(
            settings["http.proxyKerberosServicePrincipal"],
            json!("http://127.0.0.1:15524")
        );
        assert_eq!(settings["cursor.general.disableHttp2"], json!(true));
    }

    /// 用户自己的 `http.noProxy`（内网地址不走代理）：接管期间让位，但不能丢。
    #[test]
    fn the_users_no_proxy_is_held_while_we_manage_and_given_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(
            &path,
            r#"{"http.noProxy": "internal.example.com", "editor.fontSize": 14}"#,
        )
        .unwrap();

        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();

        // 接管期间它不在文件里：留着会让 Cursor 绕过本机代理直连厂商。
        let settings = read(&path);
        assert!(settings.get(NO_PROXY_KEY).is_none());
        assert_eq!(
            settings[MANAGED_NO_PROXY_KEY],
            json!("internal.example.com")
        );

        clear_proxy_settings_at(&path).unwrap();

        // 撤回之后原样还回去，连旁边不相干的键一起。
        let settings = read(&path);
        assert_eq!(settings["http.noProxy"], json!("internal.example.com"));
        assert_eq!(
            settings,
            json!({"http.noProxy": "internal.example.com", "editor.fontSize": 14})
        );
    }

    /// 接管之后用户（或 Cursor 的写回）又写了 `http.noProxy`：那是他的设置，不再删，
    /// 撤回时也不拿我们保管的旧值覆盖它。
    #[test]
    fn a_no_proxy_written_after_us_is_left_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(&path, r#"{"http.noProxy": "internal.example.com"}"#).unwrap();
        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();

        let mut settings = read_from(&path).unwrap();
        settings.insert(NO_PROXY_KEY.into(), json!("later.example.com"));
        write_to(&path, &settings).unwrap();

        // 读状态每几秒调一次写入：不能再把它删掉。
        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();
        assert_eq!(read(&path)["http.noProxy"], json!("later.example.com"));

        // 撤回时也不能用保管的 "internal.example.com" 覆盖它，保管记录随撤回一起清掉。
        clear_proxy_settings_at(&path).unwrap();
        let settings = read(&path);
        assert_eq!(settings["http.noProxy"], json!("later.example.com"));
        assert!(settings.get(MANAGED_NO_PROXY_KEY).is_none());
    }

    /// 用户本来没有这些键：不能凭空造出保管键。
    #[test]
    fn without_the_users_settings_no_bookkeeping_keys_are_written() {
        let directory = tempfile::tempdir().unwrap();
        for original in [r#"{"editor.fontSize": 14}"#, "{}"] {
            let path = settings_path(&directory);
            fs::write(&path, original).unwrap();

            write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();
            let settings = read(&path);
            for (_, custody) in CUSTODY_KEYS {
                assert!(settings.get(custody).is_none(), "{custody} 不该凭空出现");
            }

            clear_proxy_settings_at(&path).unwrap();
            assert_eq!(
                read(&path),
                serde_json::from_str::<Value>(original).unwrap()
            );
        }
    }

    /// 兄弟产品清理自己的残留时会删掉那五个键与标记键，但认不出我们的保管键：这时用户
    /// 的原值只存在保管键里，撤回必须把它还回去——不能因为"文件里没有我们的标记"就跳过。
    #[test]
    fn a_file_with_only_our_bookkeeping_keys_still_gives_the_values_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(
            &path,
            json!({
                MANAGED_HTTP_PROXY_KEY: "http://corp-proxy.example.com:8080",
                MANAGED_NO_PROXY_KEY: "internal.example.com",
            })
            .to_string(),
        )
        .unwrap();

        assert!(clear_proxy_settings_at(&path).unwrap());

        let settings = read(&path);
        assert_eq!(
            settings["http.proxy"],
            json!("http://corp-proxy.example.com:8080")
        );
        assert_eq!(settings[NO_PROXY_KEY], json!("internal.example.com"));
        assert!(settings.get(MANAGED_HTTP_PROXY_KEY).is_none());
        assert!(settings.get(MANAGED_NO_PROXY_KEY).is_none());
    }

    /// 用户自己的代理设置（公司代理、Kerberos 主体、代理开关、HTTP/2 与证书策略）：
    /// 接管期间让位，撤回时原样还回去。
    ///
    /// 覆盖之后再删掉就是永久丢失：VS Code/Cursor 不备份 `settings.json`，用户只能自己
    /// 想起来重配。以前只有 `http.noProxy` 有这个待遇，其余五个键被覆盖后直接删掉。
    #[test]
    fn the_users_own_proxy_settings_are_held_and_given_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        let original = json!({
            "http.proxy": "http://corp-proxy.example.com:8080",
            "http.proxyKerberosServicePrincipal": "HTTP/proxy.example.com",
            "http.proxySupport": "off",
            "cursor.general.disableHttp2": false,
            "http.experimental.systemCertificatesV2": false,
            "http.noProxy": "internal.example.com",
            "editor.fontSize": 14,
        });
        fs::write(&path, serde_json::to_string(&original).unwrap()).unwrap();

        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();

        // 接管期间五个键是我们的值，用户原值一个不少地挪进保管键。
        let settings = read(&path);
        assert_eq!(settings["http.proxy"], json!("http://127.0.0.1:1634"));
        assert_eq!(settings["http.proxySupport"], json!("on"));
        assert!(settings.get(NO_PROXY_KEY).is_none(), "例外会让接管失效");
        for key in KEYS {
            assert!(settings.get(key).is_some(), "{key} 接管期间由我们写");
        }
        for (key, custody) in CUSTODY_KEYS {
            assert_eq!(
                settings[custody], original[key],
                "{custody} 应当保管用户原来的 {key}"
            );
        }

        clear_proxy_settings_at(&path).unwrap();

        assert_eq!(read(&path), original, "撤回后用户原来的设置一字不动地回来");
    }

    /// 我们改写的那五个键在接管期间归我们：接管之后被写进来的值不参与"文件里已经有就
    /// 不还"的判断——读状态每几秒会把它们重写回我们的值，撤回时它们连同我们的标记一起
    /// 删掉，然后把接管之前的原值还回去。
    ///
    /// 只有 `http.noProxy` 不一样（它不在我们改写的键里，见
    /// `a_no_proxy_written_after_us_is_left_alone`）。
    #[test]
    fn a_managed_key_written_after_us_is_withdrawn_and_the_original_returns() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        fs::write(
            &path,
            r#"{"http.proxySupport": "off", "http.proxy": "http://corp-proxy.example.com:8080"}"#,
        )
        .unwrap();
        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();

        let mut settings = read_from(&path).unwrap();
        settings.insert(KEYS[2].into(), json!("on-demand"));
        write_to(&path, &settings).unwrap();

        clear_proxy_settings_at(&path).unwrap();

        let settings = read(&path);
        assert_eq!(
            settings["http.proxySupport"],
            json!("off"),
            "受管键撤回后回到接管之前的原值"
        );
        assert_eq!(
            settings["http.proxy"],
            json!("http://corp-proxy.example.com:8080"),
        );
        for (_, custody) in CUSTODY_KEYS {
            assert!(settings.get(custody).is_none(), "{custody} 撤回后不该留下");
        }
    }

    /// 改写用户手写的 `settings.json` 之前先留一份原样备份。
    ///
    /// 我们把 `json5` 读进来的东西用 `serde_json` 写回去：注释、键序、数字格式都会丢。
    /// 备份是"用户还能找回来"的唯一保证，而且只能留一次——后续改写（包括撤回）不能覆盖它。
    #[test]
    fn the_users_file_is_backed_up_before_the_first_rewrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory);
        let original = "{\n  // 我自己的注释\n  \"editor.fontSize\": 14,\n}\n";
        fs::write(&path, original).unwrap();

        write_proxy_settings_at(&path, "http://127.0.0.1:1634").unwrap();
        let backup = backup_path(&path);
        assert_eq!(
            fs::read_to_string(&backup).unwrap(),
            original,
            "原文（含注释与尾逗号）必须原样留着"
        );

        // 撤回、再次接管都不能覆盖那一份。
        clear_proxy_settings_at(&path).unwrap();
        write_proxy_settings_at(&path, "http://127.0.0.1:1635").unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);

        // 本来没有这个文件时不要凭空造备份。
        let fresh = settings_path(&tempfile::tempdir().unwrap());
        write_proxy_settings_at(&fresh, "http://127.0.0.1:1634").unwrap();
        assert!(!backup_path(&fresh).exists());
    }
}
