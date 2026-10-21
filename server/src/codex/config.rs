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
/// 路由字段：只有本应用会写它们，一旦被外部改动，路由就不再是我们发布的那条。
///
/// `web_search` **不在**这里：Codex 客户端自己的设置界面会写它（联网搜索开关）。把它算作
/// 路由字段的代价是——用户在客户端里点一下搜索，我们这边就报"配置冲突"、两个按钮都动不了。
/// 它仍然是我们接管时会写的字段（见 `OWNED_KEYS`），但只作为"与接管快照不一致"的提示，
/// 不阻断路由判定。
const ROUTING_KEYS: &[&str] = &[
    "model_provider",
    "model_catalog_json",
    "model_context_window",
    "model_auto_compact_token_limit",
];
/// 客户端自己会写的字段：Codex 允许用户在它自己的界面里切模型，切换时它会写
/// `model = "<slug>"`。切换后的 slug 仍在本次发布目录里就不算外部冲突——否则用户
/// 一切换模型，所有请求都会 409。
const MODEL_KEY: &str = "model";
/// 应用与恢复都作用于这一组：路由字段加上我们要负责换回原样的 model。
const OWNED_KEYS: &[&str] = &[
    MODEL_KEY,
    "model_provider",
    "model_catalog_json",
    "model_context_window",
    "model_auto_compact_token_limit",
    "web_search",
];
/// 客户端自己的界面模式：新版 Codex 用 `conversationDetailMode` 在「ChatGPT Work」和
/// 「Codex 本地」两个界面之间切换。详细模式（`STEPS_PROSE`）会要求 Work 浏览器身份，
/// 未登录时客户端连本地任务都拒绝创建（`Work browser access requires an authenticated
/// account`）；本地模式（`STEPS_COMMANDS`）不要求登录。这个字段不是模型路由，接管只在
/// 未登录期间把它保持在本地模式，登录后还原用户原来的选择。
const CLIENT_MODE: &str = ".haxsd-byok-client-mode.json";
/// 首次接管前整份 `config.toml` 的备份；撤销/重复接管都不会覆盖它。
const BACKUP: &str = ".haxsd-byok-config-backup.toml";
const DESKTOP: &str = "desktop";
const DETAIL_MODE: &str = "conversationDetailMode";
const DETAIL_PROSE: &str = "STEPS_PROSE";
const DETAIL_LOCAL: &str = "STEPS_COMMANDS";

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
    /// 有接管凭证：可以撤销。即使受管字段被外部改过，撤销也应该能把它们还回去——
    /// 否则用户会卡在"应用不了、也撤销不了"的状态里。
    pub restorable: bool,
    pub conflict: bool,
    /// 与接管快照不一致的受管字段（含 `model`）。界面据此说清"哪一项被改成了什么"，
    /// 而不是只丢一句"配置冲突"。
    pub conflicts: Vec<ConfigFieldConflict>,
    /// Codex 配置目录里存在有效的 ChatGPT 登录凭据：ChatGPT 已登录。
    ///
    /// 接管只改模型路由，不碰登录凭据；但 Codex 的中文界面、云端会话、官方插件等由账号态
    /// 决定，未登录时接管后也不可用。界面用这个字段提示"先登录，再接管"。
    pub chatgpt_signed_in: bool,
    /// 客户端当前生效的界面模式：`STEPS_PROSE` = ChatGPT Work，`STEPS_COMMANDS` = Codex 本地。
    /// Work 界面在未登录时会拒绝本地任务，界面据此展示我们是否已把客户端保持在本地模式。
    pub conversation_detail_mode: Option<String>,
    /// 我们为了未登录可用性，把用户原来的详细模式临时改成了本地模式。
    pub local_mode_guard: bool,
    pub message: String,
}

/// 一个受管字段的当前值与接管时写下的值（字段缺失时用空串）。
#[derive(Clone, Debug, Serialize)]
pub struct ConfigFieldConflict {
    pub key: String,
    pub applied: String,
    pub current: String,
}

/// 字段值渲染成字符串：比较与界面展示都只关心值本身，去掉 TOML 的排版空白
/// （`Item::to_string` 会把 `=` 之后的空格也算进值的前缀）。
fn render_value(item: &Item) -> String {
    item.to_string().trim().to_owned()
}

/// 冲突卡片会把 `applied` / `current` 直接显示出来（并经 RPC 回传前端），所以凭据在渲染
/// 前就要脱敏：provider 表里装着本机网关的 bearer 令牌，整表 `to_string()` 会把它原样
/// 送到界面上。
const REDACTED: &str = "[redacted]";
/// provider 表作为一个受管字段参与冲突比较。
const PROVIDER_KEY: &str = "model_providers.haxsd-byok";

/// 按名字判定凭据字段：本应用现在只写一个令牌，但按名字判定才不会因为将来加字段而漏掉。
fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token") || key.contains("secret") || key.contains("key")
}

/// provider 表按字段渲染，凭据字段只回 `[redacted]`。
fn render_provider(item: &Item) -> String {
    let Some(table) = item.as_table_like() else {
        return REDACTED.into();
    };
    table
        .iter()
        .map(|(key, value)| {
            if is_secret_key(key) {
                format!("{key} = {REDACTED}")
            } else {
                format!("{key} = {}", render_value(value))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// 受管字段在文档里的位置（provider 表嵌在 `model_providers` 下）。
fn owned_item<'a>(document: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    match key {
        PROVIDER_KEY => document
            .get("model_providers")
            .and_then(|providers| providers.get(PROVIDER)),
        other => document.get(other),
    }
}

fn render_owned(key: &str, item: Option<&Item>) -> String {
    match item {
        None => String::new(),
        Some(item) if key == PROVIDER_KEY => render_provider(item),
        Some(item) => render_value(item),
    }
}

/// 受管字段里与快照不一致的那些。
///
/// 比较用原值：只看脱敏后的文本会把「只换了令牌」当成没改。
fn field_conflicts(doc: &DocumentMut, applied: &DocumentMut) -> Vec<ConfigFieldConflict> {
    let mut keys = OWNED_KEYS.to_vec();
    keys.push(PROVIDER_KEY);
    keys.iter()
        .filter_map(|key| {
            let applied_item = owned_item(applied, key);
            let current_item = owned_item(doc, key);
            let changed = applied_item.map(Item::to_string) != current_item.map(Item::to_string);
            changed.then(|| {
                let mut applied_value = render_owned(key, applied_item);
                let mut current_value = render_owned(key, current_item);
                if applied_value == current_value {
                    // 只有被脱敏的字段不同（例如只换了令牌）：文本一样，但卡片仍要说清有差异。
                    const NOTE: &str = "（已脱敏的字段与快照不同）";
                    applied_value.push_str(NOTE);
                    current_value.push_str(NOTE);
                }
                ConfigFieldConflict {
                    key: (*key).to_owned(),
                    applied: applied_value,
                    current: current_value,
                }
            })
        })
        .collect()
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u32,
    original: Option<String>,
    applied: String,
}

/// 记录「未登录期间我们把客户端的详细模式改成了本地模式」这件事。
///
/// `original` 是改写前的原始值；`None` 表示原来没有这个键。旧版本 marker 只有
/// `remembered_prose`，读取时等价于 original = Some(STEPS_PROSE)。
#[derive(Default, Serialize, Deserialize)]
struct ClientMode {
    #[serde(default)]
    active: bool,
    #[serde(default)]
    original: Option<String>,
    #[serde(default)]
    remembered_prose: bool,
}

impl ClientMode {
    /// 需要恢复原来的详细模式时返回目标值；`None` 表示删掉这个键。
    fn restore_target(&self) -> Option<Option<&str>> {
        if self.active {
            return Some(self.original.as_deref());
        }
        self.remembered_prose.then_some(Some(DETAIL_PROSE))
    }

    fn persist(&self) -> bool {
        self.active || self.remembered_prose
    }
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

fn client_mode(home: &Path) -> Result<ClientMode> {
    // 这个文件只是我们自己的显示偏好记录；损坏时按"没改过"处理，不能让它挡住整个接管。
    Ok(read(&home.join(CLIENT_MODE))?
        .and_then(|text| serde_json::from_str::<ClientMode>(&text).ok())
        .unwrap_or_default())
}

/// Codex 的 auth 存储位置：只有 `file`（默认）时 `auth.json` 才代表登录态。
fn auth_store_uses_file(home: &Path) -> bool {
    let Ok(Some(text)) = read(&home.join("config.toml")) else {
        return true;
    };
    let Ok(document) = parse(&text) else {
        return false;
    };
    matches!(
        document
            .get("cli_auth_credentials_store")
            .and_then(Item::as_str),
        None | Some("file")
    )
}

/// `auth.json` 里是否存在可用的 ChatGPT 登录凭据。
///
/// 只认真正的登录材料：ChatGPT tokens（`access_token` + `refresh_token` + `id_token`
/// 都必须非空，和 Codex 的 `TokenData` 一致）或 personal access token。单独的
/// `OPENAI_API_KEY` 不算——新版客户端的 Work 身份判断也要求账号登录。
///
/// `cli_auth_credentials_store` 不是 `file`（keyring/auto/未知）时无法从磁盘证明未登录，
/// 按已登录处理，避免把 keyring 用户误判成未登录后强行改界面模式。
fn chatgpt_signed_in(home: &Path) -> bool {
    if !auth_store_uses_file(home) {
        return true;
    }
    let Ok(Some(text)) = read(&home.join("auth.json")) else {
        return false;
    };
    // 有些工具写 JSON 会带 UTF-8 BOM；serde_json 不接受它，先剥掉再判断。
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let Ok(auth) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    let Some(object) = auth.as_object() else {
        return false;
    };
    let non_empty = |value: Option<&serde_json::Value>| {
        value
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    };
    if let Some(mode) = object.get("auth_mode").and_then(serde_json::Value::as_str) {
        return match mode {
            "chatgpt" | "chatgptAuthTokens" => object
                .get("tokens")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|tokens| {
                    ["access_token", "refresh_token", "id_token"]
                        .iter()
                        .all(|key| non_empty(tokens.get(*key)))
                }),
            "personalAccessToken" => non_empty(object.get("personal_access_token")),
            _ => false,
        };
    }
    if non_empty(object.get("personal_access_token")) {
        return true;
    }
    object
        .get("tokens")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|tokens| {
            ["access_token", "refresh_token", "id_token"]
                .iter()
                .all(|key| non_empty(tokens.get(*key)))
        })
}

fn detail_mode(document: &DocumentMut) -> Option<String> {
    document
        .get(DESKTOP)
        .and_then(|desktop| desktop.get(DETAIL_MODE))
        .and_then(Item::as_str)
        .map(str::to_owned)
}

fn set_detail_mode(document: &mut DocumentMut, mode: &str) {
    document[DESKTOP][DETAIL_MODE] = value(mode);
}

fn remove_detail_mode(document: &mut DocumentMut) {
    if let Some(desktop) = document.get_mut(DESKTOP).and_then(Item::as_table_like_mut) {
        desktop.remove(DETAIL_MODE);
    }
}

/// `None` 表示删掉这个键。
fn apply_detail_target(document: &mut DocumentMut, target: Option<&str>) {
    match target {
        Some(mode) => set_detail_mode(document, mode),
        None => remove_detail_mode(document),
    }
}

fn set_client_mode(home: &Path, mode: &ClientMode) -> Result<()> {
    let path = home.join(CLIENT_MODE);
    if mode.persist() {
        return atomic_write(&path, &serde_json::to_vec(mode)?);
    }
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// 把当前的详细模式改成 `target`；返回是否真的写了文件。
///
/// 写前重新读一次文件，发现它已被客户端改掉就放弃本次写入，避免覆盖客户端的更新。
fn write_detail_target(home: &Path, target: Option<&str>) -> Result<bool> {
    let path = home.join("config.toml");
    let Some(original) = read(&path)? else {
        return Ok(false);
    };
    let mut document = parse(&original)?;
    if detail_mode(&document).as_deref() == target {
        return Ok(false);
    }
    apply_detail_target(&mut document, target);
    if read(&path)? != Some(original) {
        return Ok(false);
    }
    atomic_write(&path, document.to_string().as_bytes())?;
    Ok(true)
}

/// 没有接管时，把之前为未登录守 local 而改的详细模式还回去。
fn restore_orphan_client_mode(home: &Path) -> Result<bool> {
    let mode = client_mode(home)?;
    let Some(target) = mode.restore_target() else {
        return Ok(false);
    };
    write_detail_target(home, target)?;
    set_client_mode(home, &ClientMode::default())?;
    Ok(true)
}

/// 撤销接管时按收据原值恢复详细模式（而不是一律写回 PROSE）。
fn restore_detail_mode_to(home: &Path, target: Option<&str>) -> Result<()> {
    write_detail_target(home, target)?;
    set_client_mode(home, &ClientMode::default())
}

/// 未登录时把客户端保持在 Codex 本地界面；登录后还原用户原来的详细模式。
///
/// 新版客户端的「输出详细程度」决定它进入 ChatGPT Work 还是 Codex 本地界面。Work 界面
/// 要求账号身份，未登录时连本地任务都会被客户端拒绝（错误发生在创建任务之前，请求根本
/// 到不了本机网关）。这个设置属于客户端的显示偏好，不是模型路由，接管只在未登录期间
/// 强制本地模式，并把用户原来的选择记在 `CLIENT_MODE` 里，登录后还回去。
///
/// 先写 journal 再改 config：任一步失败，下一轮都能按 journal 恢复原值。
pub fn ensure_local_mode(home: &Path) -> Result<bool> {
    absolute(home)?;
    if !inspect(home)?.applied {
        return restore_orphan_client_mode(home);
    }
    let path = home.join("config.toml");
    let Some(original) = read(&path)? else {
        return Ok(false);
    };
    let mut document = parse(&original)?;
    let current = detail_mode(&document);
    let mode = client_mode(home)?;
    let signed_in = chatgpt_signed_in(home);
    if signed_in {
        let Some(target) = mode.restore_target() else {
            return Ok(false);
        };
        if current.as_deref() != Some(DETAIL_LOCAL) {
            // 用户自己已经改走了：清掉 journal，不覆盖他的选择。
            set_client_mode(home, &ClientMode::default())?;
            return Ok(true);
        }
        if current.as_deref() == target {
            set_client_mode(home, &ClientMode::default())?;
            return Ok(true);
        }
        apply_detail_target(&mut document, target);
        if read(&path)? != Some(original) {
            return Ok(false);
        }
        atomic_write(&path, document.to_string().as_bytes())?;
        set_client_mode(home, &ClientMode::default())?;
        return Ok(true);
    }
    if current.as_deref() == Some(DETAIL_PROSE) {
        if !mode.active {
            set_client_mode(
                home,
                &ClientMode {
                    active: true,
                    original: Some(DETAIL_PROSE.to_owned()),
                    remembered_prose: false,
                },
            )?;
        }
        set_detail_mode(&mut document, DETAIL_LOCAL);
        if read(&path)? != Some(original) {
            return Ok(false);
        }
        atomic_write(&path, document.to_string().as_bytes())?;
        return Ok(true);
    }
    if mode.persist() && current.as_deref() != Some(DETAIL_LOCAL) {
        // 用户把模式改成了第三种值：不再替用户守 local，交给用户。
        set_client_mode(home, &ClientMode::default())?;
        return Ok(true);
    }
    Ok(false)
}

/// 已发布目录里的模型标识：客户端切模型时写的 slug 必须来自这里。
fn catalog_slugs(published: &DocumentMut) -> Vec<String> {
    published
        .get("model_catalog_json")
        .and_then(Item::as_str)
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|catalog| {
            catalog
                .get("models")
                .and_then(serde_json::Value::as_array)
                .map(|models| {
                    models
                        .iter()
                        .filter_map(|model| {
                            model
                                .get("slug")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        })
                        .collect()
                })
        })
        .unwrap_or_default()
}
fn route_matches(doc: &DocumentMut, reference: &DocumentMut) -> bool {
    let mut current = ROUTING_KEYS
        .iter()
        .map(|key| doc.get(key).map(Item::to_string))
        .collect::<Vec<_>>();
    current.push(
        doc.get("model_providers")
            .and_then(|i| i.get(PROVIDER))
            .map(Item::to_string),
    );
    let mut reference_values = ROUTING_KEYS
        .iter()
        .map(|key| reference.get(key).map(Item::to_string))
        .collect::<Vec<_>>();
    reference_values.push(
        reference
            .get("model_providers")
            .and_then(|i| i.get(PROVIDER))
            .map(Item::to_string),
    );
    current == reference_values
}
/// 客户端切到这个模型仍然可以路由：要么它还是我们写下的 slug，要么切换后的 slug 仍在
/// 本次发布目录里。只有后者需要读目录文件。
fn model_routes(doc: &DocumentMut, published: &DocumentMut) -> bool {
    let slug = doc
        .get(MODEL_KEY)
        .and_then(Item::as_str)
        .unwrap_or_default()
        .trim();
    if slug.is_empty() {
        return false;
    }
    if published
        .get(MODEL_KEY)
        .and_then(Item::as_str)
        .map(str::trim)
        == Some(slug)
    {
        return true;
    }
    catalog_slugs(published).iter().any(|known| known == slug)
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
/// 把 `reference` 里的受管字段（`OWNED_KEYS` + provider 表）搬到 `doc` 上。
///
/// 两个调用点：撤销时把接管前的值还回去；再接管时把新收据的原值基线对齐旧收据。
fn restore_owned(doc: &mut DocumentMut, reference: &DocumentMut) {
    for key in OWNED_KEYS {
        if let Some(item) = reference.get(key) {
            doc[key] = item.clone();
        } else {
            doc.remove(key);
        }
    }
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        if let Some(item) = reference
            .get("model_providers")
            .and_then(|providers| providers.get(PROVIDER))
        {
            providers.insert(PROVIDER, item.clone());
        } else {
            providers.remove(PROVIDER);
        }
    }
    if reference.get("model_providers").is_none()
        && doc
            .get("model_providers")
            .and_then(Item::as_table_like)
            .is_some_and(|table| table.is_empty())
    {
        doc.remove("model_providers");
    }
}

/// 新收据的原值基线：当前文件内容（用户可能又改过别的键）加上旧收据记下的受管字段原值。
///
/// 没有这一步，"用户手动把路由改回原样、但 `web_search` 还是我们写的 disabled"这种半还原
/// 状态会被当成用户的原值写进新收据，接管前的 `web_search` 再也回不来。
fn rebase_original(current: &str, previous: Option<&str>) -> Result<String> {
    let mut doc = parse(current)?;
    restore_owned(&mut doc, &parse(previous.unwrap_or(""))?);
    Ok(doc.to_string())
}

pub fn inspect(home: &Path) -> Result<ConfigStatus> {
    absolute(home)?;
    let path = home.join("config.toml");
    let text = read(&path)?;
    let doc = parse(text.as_deref().unwrap_or(""))?;
    let (applied, conflict, restorable, conflicts) = if let Some(receipt) = receipt(home)? {
        let applied = parse(&receipt.applied)?;
        let original = parse(receipt.original.as_deref().unwrap_or(""))?;
        let own = route_matches(&doc, &applied) && model_routes(&doc, &applied);
        let original_route = route_matches(&doc, &original);
        (
            own,
            !own && !original_route,
            true,
            field_conflicts(&doc, &applied),
        )
    } else {
        (false, external(&doc), false, Vec::new())
    };
    Ok(ConfigStatus {
        home: home.display().to_string(),
        config_path: path.display().to_string(),
        applied,
        restorable,
        conflict,
        conflicts,
        chatgpt_signed_in: chatgpt_signed_in(home),
        conversation_detail_mode: detail_mode(&doc),
        local_mode_guard: client_mode(home)?.persist(),
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
    let mut rebased_original = None;
    let existing_receipt = receipt(home)?;
    let first_takeover = existing_receipt.is_none();
    if let Some(receipt) = existing_receipt {
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
        if !route_matches(&doc, &parse(receipt.original.as_deref().unwrap_or(""))?) {
            return Err(Error::Config(
                "Codex managed configuration was modified; refusing to overwrite".into(),
            ));
        }
        // 路由已经回到原样，但受管字段里可能还留着我们的值（撤销的短路分支过去只删收据）。
        // 新收据的原值基线要沿用旧收据，否则这些残留会被当成用户的原值。
        if let Some(current_text) = original.as_deref() {
            rebased_original = Some(rebase_original(current_text, receipt.original.as_deref())?);
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
    let receipt_original = rebased_original.or_else(|| original.clone());
    if read(&path)? != original {
        return Err(Error::Config(
            "Codex config changed while preparing the update".into(),
        ));
    }
    // 首次接管前保留整份原文件。用户之后在 Codex 里改插件/审批/沙箱也能从这里找回；
    // 重复接管不覆盖它，保证备份始终是"接管发生前"的那一份。
    let backup = home.join(BACKUP);
    if first_takeover && !backup.exists() {
        if let Some(original) = original.as_deref() {
            atomic_write(&backup, original.as_bytes())?;
        }
    }
    atomic_write(
        &home.join(RECEIPT),
        &serde_json::to_vec(&Receipt {
            version: 1,
            original: receipt_original,
            applied: applied.clone(),
        })?,
    )?;
    if read(&path)? != original {
        return Err(Error::Config(
            "Codex config changed before replacement; restore receipt retained".into(),
        ));
    }
    atomic_write(&path, applied.as_bytes())?;
    // 接管生效后立刻把未登录的客户端切到本地界面：Work 界面的浏览器身份门禁会让
    // 客户端在创建本地任务之前就报错，请求根本到不了本机网关。
    ensure_local_mode(home)?;
    inspect(home)
}
pub fn restore(home: &Path) -> Result<ConfigStatus> {
    absolute(home)?;
    let Some(receipt) = receipt(home)? else {
        // 没有收据也要自愈：崩溃可能留下"只有 marker、没有收据"的半截状态。
        restore_orphan_client_mode(home)?;
        return inspect(home);
    };
    let path = home.join("config.toml");
    let current = read(&path)?;
    let mut doc = parse(current.as_deref().unwrap_or(""))?;
    let original = parse(receipt.original.as_deref().unwrap_or(""))?;
    let original_detail = original
        .get(DESKTOP)
        .and_then(|desktop| desktop.get(DETAIL_MODE))
        .and_then(Item::as_str)
        .map(str::to_owned);
    // 受管字段被外部改过也照样撤销：撤销的意思就是"把我们的字段还回去"，而下面这段只动
    // `OWNED_KEYS`（含 provider 表），不碰其它键。以前这里整体拒绝，于是用户在"应用不了、
    // 也撤销不了"的状态里出不来。
    //
    // 过去还有一条"路由已经回到原样 → 只删收据"的短路。它漏掉了 `OWNED_KEYS` 里不属于
    // 路由的字段（`web_search`）：用户手动把路由清干净之后撤销，我们写的
    // `web_search = "disabled"` 会留在文件里，下一次接管又会把它当成用户的原值。
    if current.as_deref() == Some(receipt.applied.as_str()) {
        // 整体回滚：当前文件就是我们写下的那一份，直接写回原文最干净。
        if let Some(original) = receipt.original {
            atomic_write(&path, original.as_bytes())?;
        } else {
            fs::remove_file(&path)?;
        }
    } else {
        restore_owned(&mut doc, &original);
        let updated = doc.to_string();
        if Some(updated.as_str()) != current.as_deref() {
            if read(&path)? != current {
                return Err(Error::Config("Codex config changed during restore".into()));
            }
            atomic_write(&path, updated.as_bytes())?;
        }
    }
    // 详细模式按收据里的原值恢复（可能是"未设置"或别的模式），不是一律写回 PROSE。
    // 必须在删收据之前完成：失败时收据还在，下次撤销还能重试。
    restore_detail_mode_to(home, original_detail.as_deref())?;
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

/// 目录是写给客户端的快照，模型被编辑后它会过期。请求路径读的是模型库里的最新
/// 配置，这里把目录刷新到同一份路由：只换 `model_catalog_json` 指针——`model` 是
/// 客户端自己的选择，切过模型不能被重置。
pub(super) fn republish_catalog(home: &Path, catalog: &[u8]) -> Result<PathBuf> {
    absolute(home)?;
    let Some(receipt) = receipt(home)? else {
        return Err(Error::Config(
            "Codex configuration is not applied; refusing to republish its catalog".into(),
        ));
    };
    let path = home.join("config.toml");
    let original = read(&path)?;
    let mut doc = parse(original.as_deref().unwrap_or(""))?;
    let previous = doc
        .get("model_catalog_json")
        .and_then(Item::as_str)
        .map(PathBuf::from);
    let catalog_path = write_catalog(home, catalog)?;
    let pointer = catalog_path.to_string_lossy().into_owned();
    doc["model_catalog_json"] = value(pointer.as_str());
    // 恢复收据里的 applied 快照必须跟着换指针：`inspect`/`restore` 靠它判断配置
    // 是否仍由本应用发布，不同步的话这次刷新会被当成外部改动。
    let mut applied = parse(&receipt.applied)?;
    applied["model_catalog_json"] = value(pointer.as_str());
    let updated = doc.to_string();
    if read(&path)? != original {
        return Err(Error::Config(
            "Codex config changed while preparing the update".into(),
        ));
    }
    atomic_write(
        &home.join(RECEIPT),
        &serde_json::to_vec(&Receipt {
            version: 1,
            original: receipt.original,
            applied: applied.to_string(),
        })?,
    )?;
    if read(&path)? != original {
        return Err(Error::Config(
            "Codex config changed before replacement; restore receipt retained".into(),
        ));
    }
    atomic_write(&path, updated.as_bytes())?;
    // 上一份目录是本应用的产物，留着只会让人分不清哪份在用。
    if let Some(previous) = previous.filter(|previous| previous != &catalog_path) {
        if previous.parent() == Some(home) {
            if let Err(error) = fs::remove_file(&previous) {
                tracing::warn!(%error, path = %previous.display(), "Codex catalog cleanup failed");
            }
        }
    }
    Ok(catalog_path)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 发布目录：客户端在它自己的界面里能选到的 slug 来自这里。
    fn catalog(home: &Path, slugs: &[&str]) -> PathBuf {
        let path = home.join("models.json");
        let models = slugs
            .iter()
            .map(|slug| serde_json::json!({"slug": slug}))
            .collect::<Vec<_>>();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"models": models})).unwrap(),
        )
        .unwrap();
        path
    }

    fn route(home: &Path) -> ConfigRoute {
        ConfigRoute {
            model: "model-a".into(),
            base_url: "http://127.0.0.1:43120/v1".into(),
            token: "local-test-token".into(),
            catalog_path: catalog(home, &["model-a", "model-b"]),
            context_window: 32768,
        }
    }

    fn applied_home(content: Option<&str>) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        if let Some(content) = content {
            fs::write(directory.path().join("config.toml"), content).unwrap();
        }
        apply(directory.path(), &route(directory.path())).unwrap();
        directory
    }

    /// 客户端切换模型就是重写 config.toml 里的这一行。
    fn switch_client_model(home: &Path, slug: &str) {
        edit(home, |doc| doc[MODEL_KEY] = value(slug));
    }

    /// 一份有效的 ChatGPT 登录缓存（新版客户端靠 tokens 判断账号身份）。
    fn chatgpt_auth() -> &'static str {
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"access","refresh_token":"refresh","id_token":"id"}}"#
    }

    /// 登录态只读：`auth.json` 里有有效 ChatGPT tokens 才算已登录；接管与撤销都不应碰它。
    #[test]
    fn chatgpt_sign_in_is_detected_from_auth_json_and_survives_apply_and_restore() {
        let directory = tempfile::tempdir().unwrap();
        let auth = directory.path().join("auth.json");
        assert!(!inspect(directory.path()).unwrap().chatgpt_signed_in);

        fs::write(&auth, chatgpt_auth()).unwrap();
        assert!(inspect(directory.path()).unwrap().chatgpt_signed_in);

        apply(directory.path(), &route(directory.path())).unwrap();
        assert!(
            inspect(directory.path()).unwrap().chatgpt_signed_in,
            "接管不能动登录凭据"
        );

        restore(directory.path()).unwrap();
        assert!(
            inspect(directory.path()).unwrap().chatgpt_signed_in,
            "撤销也不能动登录凭据"
        );
        assert_eq!(fs::read_to_string(&auth).unwrap(), chatgpt_auth());
    }

    /// API key / 空凭据不算 ChatGPT 登录：界面提示的是"登录账号"，不是"配置了 key"。
    #[test]
    fn api_key_or_empty_auth_json_is_not_a_chatgpt_login() {
        let directory = tempfile::tempdir().unwrap();
        let auth = directory.path().join("auth.json");
        for payload in [
            r#"{}"#,
            r#"{"OPENAI_API_KEY":"sk-test"}"#,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test"}"#,
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":""}}"#,
        ] {
            fs::write(&auth, payload).unwrap();
            assert!(
                !inspect(directory.path()).unwrap().chatgpt_signed_in,
                "{payload}"
            );
        }
    }

    /// 首次接管前保存整份原文件；后续接管/撤销不覆盖它。
    #[test]
    fn first_takeover_backs_up_the_whole_config_file() {
        let directory = tempfile::tempdir().unwrap();
        let original = "model = 'user-choice'\n\n[desktop]\nscale = 1.25\n";
        fs::write(directory.path().join("config.toml"), original).unwrap();

        apply(directory.path(), &route(directory.path())).unwrap();
        let backup = directory.path().join(BACKUP);
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);

        restore(directory.path()).unwrap();
        apply(directory.path(), &route(directory.path())).unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    }

    fn prose_desktop() -> &'static str {
        "[desktop]\nconversationDetailMode = \"STEPS_PROSE\"\nscale = 1.25\n"
    }

    fn config_text(home: &Path) -> String {
        fs::read_to_string(home.join("config.toml")).unwrap()
    }

    /// 未登录时，接管必须把客户端留在 Codex 本地界面：Work 界面的浏览器身份门禁会在
    /// 创建本地任务之前就报「需要登录」，请求根本到不了本机网关。
    #[test]
    fn signed_out_takeover_keeps_the_client_in_local_mode() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("config.toml"), prose_desktop()).unwrap();
        apply(directory.path(), &route(directory.path())).unwrap();

        let text = config_text(directory.path());
        assert!(
            text.contains("conversationDetailMode = \"STEPS_COMMANDS\""),
            "{text}"
        );
        assert!(
            text.contains("scale = 1.25"),
            "其它 desktop 设置不能被带走: {text}"
        );
        assert!(
            !ensure_local_mode(directory.path()).unwrap(),
            "已经是本地模式，不应重复写"
        );
        let status = inspect(directory.path()).unwrap();
        assert_eq!(
            status.conversation_detail_mode.as_deref(),
            Some("STEPS_COMMANDS")
        );
        assert!(status.local_mode_guard, "应当记录我们替用户切过模式");
    }

    /// 登录之后把用户原来的详细模式还回去，且只还原一次。
    #[test]
    fn prose_mode_returns_after_chatgpt_sign_in() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("config.toml"), prose_desktop()).unwrap();
        apply(directory.path(), &route(directory.path())).unwrap();
        assert!(config_text(directory.path()).contains("STEPS_COMMANDS"));

        fs::write(directory.path().join("auth.json"), chatgpt_auth()).unwrap();
        assert!(ensure_local_mode(directory.path()).unwrap());
        assert!(config_text(directory.path()).contains("conversationDetailMode = \"STEPS_PROSE\""));
        assert!(!directory.path().join(CLIENT_MODE).exists());
        assert!(!ensure_local_mode(directory.path()).unwrap());
    }

    /// 已登录就不该碰用户的显示偏好。
    #[test]
    fn signed_in_takeover_leaves_the_prose_setting_alone() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("config.toml"), prose_desktop()).unwrap();
        fs::write(directory.path().join("auth.json"), chatgpt_auth()).unwrap();
        apply(directory.path(), &route(directory.path())).unwrap();
        assert!(config_text(directory.path()).contains("conversationDetailMode = \"STEPS_PROSE\""));
        assert!(!inspect(directory.path()).unwrap().local_mode_guard);
    }

    /// 撤销时的详细模式必须按收据原值恢复，而不是一律写回 PROSE。
    #[test]
    fn restore_uses_the_receipt_original_detail_mode() {
        let directory = tempfile::tempdir().unwrap();
        let original = "[desktop]\nconversationDetailMode = \"STEPS_COMMANDS\"\nscale = 1.25\n";
        fs::write(directory.path().join("config.toml"), original).unwrap();
        apply(directory.path(), &route(directory.path())).unwrap();

        // 接管后用户在客户端里切到详细模式；未登录守卫改成 local 并记下 PROSE。
        edit(directory.path(), |doc| {
            doc[DESKTOP][DETAIL_MODE] = value(DETAIL_PROSE)
        });
        assert!(ensure_local_mode(directory.path()).unwrap());

        restore(directory.path()).unwrap();
        let text = config_text(directory.path());
        assert!(
            text.contains("conversationDetailMode = \"STEPS_COMMANDS\""),
            "撤销应回到收据里的原值，而不是 PROSE: {text}"
        );
        assert!(!directory.path().join(CLIENT_MODE).exists());
    }

    /// 没有收据（崩溃/旧版本残留）也要把 marker 里记录的详细模式还回去。
    #[test]
    fn orphan_client_mode_marker_is_healed_without_receipt() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("config.toml"),
            "[desktop]\nconversationDetailMode = \"STEPS_COMMANDS\"\n",
        )
        .unwrap();
        fs::write(
            directory.path().join(CLIENT_MODE),
            serde_json::to_vec(&ClientMode {
                active: true,
                original: Some(DETAIL_PROSE.to_owned()),
                remembered_prose: false,
            })
            .unwrap(),
        )
        .unwrap();

        assert!(ensure_local_mode(directory.path()).unwrap());
        assert!(config_text(directory.path()).contains("conversationDetailMode = \"STEPS_PROSE\""));
        assert!(!directory.path().join(CLIENT_MODE).exists());
        assert!(!ensure_local_mode(directory.path()).unwrap());
    }

    /// keyring/auto 存储下 auth.json 不代表登录态：不能把已登录用户误判成未登录。
    #[test]
    fn keyring_auth_store_is_treated_as_signed_in() {
        let directory = tempfile::tempdir().unwrap();
        let config = "cli_auth_credentials_store = \"keyring\"\n[desktop]\nconversationDetailMode = \"STEPS_PROSE\"\n";
        fs::write(directory.path().join("config.toml"), config).unwrap();
        assert!(!directory.path().join("auth.json").exists());

        apply(directory.path(), &route(directory.path())).unwrap();
        let text = config_text(directory.path());
        assert!(
            text.contains("conversationDetailMode = \"STEPS_PROSE\""),
            "keyring 用户不应被强行切模式: {text}"
        );
        assert!(inspect(directory.path()).unwrap().chatgpt_signed_in);
    }

    /// 带 UTF-8 BOM 的 auth.json 也要能识别（一些编辑器/脚本会写 BOM）。
    #[test]
    fn auth_json_with_utf8_bom_is_still_recognized() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("auth.json"),
            "\u{feff}{\"auth_mode\":\"chatgpt\",\"tokens\":{\"access_token\":\"a\",\"refresh_token\":\"r\",\"id_token\":\"i\"}}",
        )
        .unwrap();
        assert!(inspect(directory.path()).unwrap().chatgpt_signed_in);
    }

    /// personal access token 在 Codex 里也算 ChatGPT 账号登录。
    #[test]
    fn personal_access_token_counts_as_signed_in() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("auth.json"),
            br#"{"auth_mode":"personalAccessToken","personal_access_token":"pat"}"#,
        )
        .unwrap();
        assert!(inspect(directory.path()).unwrap().chatgpt_signed_in);
    }

    /// 撤销接管时把替用户保存的详细模式还回去。
    #[test]
    fn restore_returns_the_remembered_prose_mode() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("config.toml"), prose_desktop()).unwrap();
        apply(directory.path(), &route(directory.path())).unwrap();
        restore(directory.path()).unwrap();
        assert!(config_text(directory.path()).contains("conversationDetailMode = \"STEPS_PROSE\""));
        assert!(!directory.path().join(CLIENT_MODE).exists());
    }

    fn edit(home: &Path, change: impl FnOnce(&mut DocumentMut)) -> String {
        let path = home.join("config.toml");
        let mut doc = parse(&fs::read_to_string(&path).unwrap()).unwrap();
        change(&mut doc);
        let text = doc.to_string();
        fs::write(&path, &text).unwrap();
        text
    }

    /// 客户端切到目录里发布的另一个模型：路由没变，不能报冲突，否则每个请求都 409。
    #[test]
    fn switching_to_another_published_model_keeps_the_route_applied() {
        let directory = applied_home(None);
        switch_client_model(directory.path(), "model-b");
        let status = inspect(directory.path()).unwrap();
        assert!(status.applied, "{}", status.message);
        assert!(!status.conflict, "{}", status.message);
    }

    /// 切到目录里不存在的模型：上游绑定已经不属于本次发布，必须报冲突。
    #[test]
    fn switching_to_an_unpublished_model_is_a_conflict() {
        let directory = applied_home(None);
        switch_client_model(directory.path(), "not-in-catalog");
        let status = inspect(directory.path()).unwrap();
        assert!(!status.applied, "{}", status.message);
        assert!(status.conflict, "{}", status.message);
    }

    /// 路由字段被外部改动：仍然报冲突（应用不能覆盖别家的路由），但撤销永远可用——
    /// 只把 `OWNED_KEYS` 与 provider 表还回接管前的样子，用户自己的其它键原样保留。
    #[test]
    fn external_edits_to_routing_fields_are_conflicts_but_restore_still_returns_them() {
        let original = "model = 'user-choice'\nweb_search = 'live'\n\n[projects.'x']\ntrust_level = 'trusted'\n\n[desktop]\nscale = 1.25\n";
        let changes: [fn(&mut DocumentMut); 3] = [
            |doc| doc["model_providers"][PROVIDER]["base_url"] = value("http://127.0.0.1:43121/v1"),
            |doc| {
                doc["model_providers"][PROVIDER]["experimental_bearer_token"] = value("other-token")
            },
            |doc| doc["model_context_window"] = value(16384),
        ];
        for change in changes {
            let directory = applied_home(Some(original));
            let edited = edit(directory.path(), change);
            let status = inspect(directory.path()).unwrap();
            assert!(!status.applied && status.conflict, "{}", status.message);
            assert!(status.restorable, "{}", status.message);
            assert!(!status.conflicts.is_empty(), "{}", status.message);
            let restored = restore(directory.path()).unwrap();
            assert!(
                !restored.applied && !restored.conflict && !restored.restorable,
                "{}",
                restored.message
            );
            assert!(restored.conflicts.is_empty(), "{:?}", restored.conflicts);
            let text = fs::read_to_string(directory.path().join("config.toml")).unwrap();
            assert_ne!(text, edited, "撤销没有把受管字段还回去");
            let doc = parse(&text).unwrap();
            assert_eq!(doc.get("model").and_then(Item::as_str), Some("user-choice"));
            assert_eq!(doc.get("web_search").and_then(Item::as_str), Some("live"));
            for key in [
                "model_provider",
                "model_catalog_json",
                "model_context_window",
                "model_auto_compact_token_limit",
                "model_providers",
            ] {
                assert!(doc.get(key).is_none(), "{key} 在撤销后仍然存在: {text}");
            }
            assert!(
                text.contains("trust_level = 'trusted'") && text.contains("scale = 1.25"),
                "用户自己的配置项被撤销动过: {text}"
            );
        }
    }

    /// 客户端在它自己的界面里打开联网搜索（写 `web_search = "live"`）：路由照旧可用，
    /// 只在 `conflicts` 里给出"快照写的是 disabled、现在是 live"，撤销时按快照还原。
    #[test]
    fn a_client_web_search_switch_is_reported_without_blocking_the_route() {
        let directory = applied_home(Some("model = 'user-choice'\n"));
        edit(directory.path(), |doc| doc["web_search"] = value("live"));
        let status = inspect(directory.path()).unwrap();
        assert!(status.applied, "{}", status.message);
        assert!(!status.conflict, "{}", status.message);
        assert!(status.restorable, "{}", status.message);
        assert_eq!(status.conflicts.len(), 1, "{:?}", status.conflicts);
        let conflict = &status.conflicts[0];
        assert_eq!(conflict.key, "web_search");
        assert_eq!(conflict.applied, "\"disabled\"");
        assert_eq!(conflict.current, "\"live\"");
        restore(directory.path()).unwrap();
        let doc =
            parse(&fs::read_to_string(directory.path().join("config.toml")).unwrap()).unwrap();
        assert!(doc.get("web_search").is_none(), "接管前的配置里没有这一项");
    }

    /// 与接管快照完全一致的配置没有字段冲突。
    #[test]
    fn a_configuration_matching_the_snapshot_has_no_field_conflicts() {
        let directory = applied_home(Some("model = 'user-choice'\n"));
        let status = inspect(directory.path()).unwrap();
        assert!(status.applied && !status.conflict, "{}", status.message);
        assert!(status.restorable, "{}", status.message);
        assert!(status.conflicts.is_empty(), "{:?}", status.conflicts);
    }

    /// 客户端切过模型之后仍然可以恢复，并且 model 回到收据里的原值。
    #[test]
    fn restore_after_a_client_model_switch_puts_back_the_original_model() {
        let directory = applied_home(Some("model = 'user-choice'\n"));
        switch_client_model(directory.path(), "model-b");
        restore(directory.path()).unwrap();
        let text = fs::read_to_string(directory.path().join("config.toml")).unwrap();
        assert!(text.contains("model = 'user-choice'"), "{text}");
        assert!(!text.contains("local-test-token"), "{text}");
        let status = inspect(directory.path()).unwrap();
        assert!(!status.applied && !status.conflict, "{}", status.message);
    }

    /// 目录文件还没生成或已被删除时，没有切换过模型的配置仍然算我们的路由。
    #[test]
    fn a_missing_catalog_keeps_an_untouched_route_applied() {
        let directory = tempfile::tempdir().unwrap();
        let mut route = route(directory.path());
        route.catalog_path = directory.path().join("missing.json");
        apply(directory.path(), &route).unwrap();
        let status = inspect(directory.path()).unwrap();
        assert!(status.applied, "{}", status.message);
        assert!(!status.conflict, "{}", status.message);
        // 客户端切换模型后无法再确认新 slug 属于本次发布。
        switch_client_model(directory.path(), "model-b");
        assert!(inspect(directory.path()).unwrap().conflict);
    }

    /// 冲突卡片会显示 `applied` / `current`：provider 表里的 bearer 令牌不得出现在里面。
    /// 「只换了令牌」仍要算冲突——比较用原值，渲染才脱敏。
    #[test]
    fn provider_conflicts_never_carry_the_gateway_token() {
        let directory = applied_home(None);
        edit(directory.path(), |doc| {
            doc["model_providers"][PROVIDER]["experimental_bearer_token"] = value("other-token")
        });
        let status = inspect(directory.path()).unwrap();
        assert!(status.conflict, "{}", status.message);
        let conflict = status
            .conflicts
            .iter()
            .find(|conflict| conflict.key == PROVIDER_KEY)
            .expect("令牌被改过就要报 provider 表冲突");
        for rendered in [&conflict.applied, &conflict.current] {
            assert!(!rendered.contains("local-test-token"), "{rendered}");
            assert!(!rendered.contains("other-token"), "{rendered}");
            assert!(rendered.contains(REDACTED), "{rendered}");
        }
        assert!(
            conflict.applied.contains("base_url"),
            "非凭据字段照旧显示: {}",
            conflict.applied
        );
    }

    /// 用户手动把路由清干净：删掉路由键与 provider 表，模型改回自己的值。
    /// `web_search` 故意留着——它也是受管字段，但不是路由键。
    fn manually_unroute(home: &Path) {
        edit(home, |doc| {
            doc.remove("model_provider");
            doc.remove("model_catalog_json");
            doc.remove("model_context_window");
            doc.remove("model_auto_compact_token_limit");
            if let Some(providers) = doc
                .get_mut("model_providers")
                .and_then(Item::as_table_like_mut)
            {
                providers.remove(PROVIDER);
            }
            doc["model"] = value("user-choice");
        });
    }

    /// 手动清干净路由、但留下我们写的 `web_search`：撤销必须把它也还回去。
    /// 以前「路由已回到原样」的短路只删收据，于是 `web_search = "disabled"` 永久留在
    /// 用户的配置里，下一次接管还会把它当成用户的原值。
    #[test]
    fn restore_returns_leftovers_even_when_the_route_already_matches_again() {
        let directory = applied_home(Some("model = 'user-choice'\nweb_search = 'live'\n"));
        manually_unroute(directory.path());
        let status = inspect(directory.path()).unwrap();
        assert!(!status.conflict, "{}", status.message);
        assert!(status.restorable, "{}", status.message);

        restore(directory.path()).unwrap();

        let doc =
            parse(&fs::read_to_string(directory.path().join("config.toml")).unwrap()).unwrap();
        assert_eq!(
            doc.get("web_search").and_then(Item::as_str),
            Some("live"),
            "受管字段里不属于路由的那一项也要还回去"
        );
        assert!(doc.get("model_providers").is_none());
    }

    /// 半还原状态下再次接管：新收据的原值基线要沿用旧收据里的受管字段，否则我们留下的
    /// `web_search = "disabled"` 会被当成用户的原值，接管前的 `live` 再也还不出来。
    #[test]
    fn applying_again_after_a_manual_revert_keeps_the_true_original() {
        let directory = applied_home(Some("model = 'user-choice'\nweb_search = 'live'\n"));
        manually_unroute(directory.path());

        apply(directory.path(), &route(directory.path())).unwrap();
        restore(directory.path()).unwrap();

        let doc =
            parse(&fs::read_to_string(directory.path().join("config.toml")).unwrap()).unwrap();
        assert_eq!(
            doc.get("web_search").and_then(Item::as_str),
            Some("live"),
            "接管前的值不能丢"
        );
        assert!(doc.get("model_providers").is_none());
    }
}
