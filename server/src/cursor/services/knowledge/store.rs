//! Persists rules as markdown files with a JSON metadata sidecar.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const META_FILE: &str = "meta.json";
const RULE_EXTENSION: &str = "md";
pub const LOCAL_ID_PREFIX: &str = "local-";

/// 进程级规则缓存:目录 → (目录指纹, 规则列表)。
///
/// 缓存原先挂在 `RuleStore` 实例上,而每轮 Run 都会 `RuleStore::open` 一个**新实例**
/// (`compile/context.rs` 的 `merge_local_rules`)——实例级缓存永不命中,每轮仍要把每个
/// `.md` 重读一遍、重新解析 meta.json。提到进程级之后,只有"目录指纹变了"才真的读内容
/// (用户手改、别的进程写入、增删文件都会让指纹变);指纹本身仍然每轮算一次,它只是
/// `read_dir` + 每个文件的 mtime/长度,不读正文。与 plugin catalog 的进程级缓存同一套路。
/// 规则目录 → (目录指纹, 规则表)。
type RuleCache = HashMap<PathBuf, (String, Vec<RuleRecord>)>;

static CACHE: OnceLock<Mutex<RuleCache>> = OnceLock::new();

fn cache() -> &'static Mutex<RuleCache> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 测试用:每个目录的内容被真正读了几次(缓存命中不增加)。
///
/// 按目录分开计数:同进程里其他用例也在读写自己的规则目录,进程级计数会被它们带偏。
#[cfg(test)]
fn content_loads() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static LOADS: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    LOADS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn content_load_count(root: &Path) -> u64 {
    content_loads()
        .lock()
        .expect("content load counts")
        .get(root)
        .copied()
        .unwrap_or(0)
}

/// 测试用:记录一次目录内容读取(生产构建里是空函数,调用点不分支)。
fn record_content_load(_root: &Path) {
    #[cfg(test)]
    {
        *content_loads()
            .lock()
            .expect("content load counts")
            .entry(_root.to_path_buf())
            .or_default() += 1;
    }
}

/// 一条规则的完整视图:knowledge 来自 md 文件,其余字段来自 meta.json。
#[derive(Clone, Debug, PartialEq)]
pub struct RuleRecord {
    pub id: String,
    pub knowledge: String,
    pub title: String,
    pub created_at: String,
    pub is_generated: bool,
    pub git_origin: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOp {
    Add,
    Update,
    Remove,
}

/// 离线期间未同步到上游的一次变更。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub op: JournalOp,
    pub id: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Meta {
    #[serde(default)]
    rules: BTreeMap<String, RuleMeta>,
    #[serde(default)]
    journal: Vec<JournalEntry>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct RuleMeta {
    #[serde(default)]
    title: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    is_generated: bool,
    #[serde(default)]
    git_origin: String,
}

/// md 文件为核心的规则存储;调用方需自行串行化并发访问。
pub struct RuleStore {
    root: PathBuf,
}

impl RuleStore {
    pub fn open(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn list(&self) -> Result<Vec<RuleRecord>> {
        let fingerprint = self.fingerprint();
        {
            // 进程级缓存:每轮 Run 都会 new 一个 store,缓存挂在实例上等于永不命中。
            let cache = cache().lock().expect("rule cache lock");
            if let Some((cached, records)) = cache.get(&self.root) {
                if *cached == fingerprint {
                    return Ok(records.clone());
                }
            }
        }
        let records = self.read_all()?;
        cache()
            .lock()
            .expect("rule cache lock")
            .insert(self.root.clone(), (fingerprint, records.clone()));
        Ok(records)
    }

    /// 目录当前状态：根目录与每个文件（含 meta.json）的 (名字, mtime, 大小)。
    ///
    /// 带上大小是因为同一秒内改写文件时 mtime 可能不变，而长度一定变。
    fn fingerprint(&self) -> String {
        let mut stamp = String::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            // 读不出来就别用缓存：让 `read_all` 去报真正的错误。
            return stamp;
        };
        let mut names = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        for name in names {
            stamp.push_str(&name);
            match std::fs::metadata(self.root.join(&name)) {
                Ok(metadata) => {
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |elapsed| elapsed.as_nanos());
                    stamp.push_str(&format!(":{modified}:{};", metadata.len()));
                }
                Err(_) => stamp.push_str(":missing;"),
            }
        }
        stamp
    }

    fn read_all(&self) -> Result<Vec<RuleRecord>> {
        record_content_load(&self.root);
        let meta = self.read_meta()?;
        let mut records = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some(RULE_EXTENSION) {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if validate_id(id).is_err() {
                continue;
            }
            let knowledge = match std::fs::read_to_string(&path) {
                Ok(knowledge) => knowledge,
                Err(error) => {
                    // 一个读不出来的文件（被占用、权限、坏盘）不该让整份规则列表失败：
                    // 这份列表是"本轮 Run 的上下文"，离线降级也靠它，一条坏文件等于把用户
                    // 所有规则一起关掉。目录级错误仍然上抛（`read_dir` 与 `read_meta` 的 `?`）。
                    tracing::warn!(
                        path = %path.display(),
                        error = %crate::diagnostics::error_chain(&error),
                        "skipping an unreadable knowledge rule file"
                    );
                    continue;
                }
            };
            records.push(assemble(id, knowledge, meta.rules.get(id), &path));
        }
        records.sort_by(|left, right| {
            timestamp(&right.created_at)
                .cmp(&timestamp(&left.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(records)
    }

    pub fn get(&self, id: &str) -> Result<Option<RuleRecord>> {
        validate_id(id)?;
        let path = self.rule_path(id);
        let knowledge = match std::fs::read_to_string(&path) {
            Ok(knowledge) => knowledge,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let meta = self.read_meta()?;
        Ok(Some(assemble(id, knowledge, meta.rules.get(id), &path)))
    }

    pub fn upsert(&self, record: &RuleRecord) -> Result<()> {
        validate_id(&record.id)?;
        write_atomic(&self.rule_path(&record.id), record.knowledge.as_bytes())?;
        let mut meta = self.read_meta()?;
        meta.rules.insert(record.id.clone(), rule_meta(record));
        self.write_meta(&meta)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        remove_file_if_exists(&self.rule_path(id))?;
        let mut meta = self.read_meta()?;
        if meta.rules.remove(id).is_some() {
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    /// 离线新增的规则在上游落地后,把本地临时 id 换成上游分配的真实 id。
    pub fn promote(&self, old_id: &str, new_id: &str) -> Result<()> {
        validate_id(old_id)?;
        validate_id(new_id)?;
        let source = self.rule_path(old_id);
        let target = self.rule_path(new_id);
        #[cfg(windows)]
        remove_file_if_exists(&target)?;
        std::fs::rename(&source, &target)?;
        let mut meta = self.read_meta()?;
        if let Some(rule) = meta.rules.remove(old_id) {
            meta.rules.insert(new_id.into(), rule);
        }
        for entry in &mut meta.journal {
            if entry.id == old_id {
                entry.id = new_id.into();
            }
        }
        self.write_meta(&meta)
    }

    /// 用上游的完整列表覆盖本地镜像;仅应在日志为空(已全部回放)时调用。
    pub fn replace_all(&self, records: &[RuleRecord]) -> Result<()> {
        let mut meta = self.read_meta()?;
        // 只有"上一版由本镜像写过、这一版上游不再返回"的文件才删。没有元数据的 .md
        // 是用户自己放进来的,`local-` 前缀的是本地还没推上去的规则——上游列表里
        // 没有它们不代表它们该消失,删掉不可恢复。
        let mirrored: BTreeSet<String> = meta.rules.keys().cloned().collect();
        meta.rules.clear();
        for record in records {
            validate_id(&record.id)?;
            write_atomic(&self.rule_path(&record.id), record.knowledge.as_bytes())?;
            meta.rules.insert(record.id.clone(), rule_meta(record));
        }
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some(RULE_EXTENSION) {
                continue;
            }
            let keep = path
                .file_stem()
                .and_then(|value| value.to_str())
                .is_none_or(|id| {
                    meta.rules.contains_key(id)
                        || !mirrored.contains(id)
                        || id.starts_with(LOCAL_ID_PREFIX)
                });
            if !keep {
                remove_file_if_exists(&path)?;
            }
        }
        self.write_meta(&meta)
    }

    pub fn journal_front(&self) -> Result<Option<JournalEntry>> {
        Ok(self.read_meta()?.journal.first().cloned())
    }

    pub fn pop_journal(&self) -> Result<()> {
        let mut meta = self.read_meta()?;
        if !meta.journal.is_empty() {
            meta.journal.remove(0);
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    pub fn record_add(&self, id: &str) -> Result<()> {
        let mut meta = self.read_meta()?;
        meta.journal.push(JournalEntry {
            op: JournalOp::Add,
            id: id.into(),
        });
        self.write_meta(&meta)
    }

    pub fn record_update(&self, id: &str) -> Result<()> {
        let mut meta = self.read_meta()?;
        if journal_contains(&meta.journal, id, JournalOp::Add) {
            // 回放 add 时会读取最新内容,无需单独的 update 日志。
            return Ok(());
        }
        let op = if id.starts_with(LOCAL_ID_PREFIX) {
            // 本地临时 id 没有对应的 add 日志(如镜像覆盖后的残留),按新增回放。
            JournalOp::Add
        } else {
            JournalOp::Update
        };
        if !journal_contains(&meta.journal, id, op) {
            meta.journal.push(JournalEntry { op, id: id.into() });
            self.write_meta(&meta)?;
        }
        Ok(())
    }

    pub fn record_remove(&self, id: &str) -> Result<()> {
        let mut meta = self.read_meta()?;
        let never_synced = journal_contains(&meta.journal, id, JournalOp::Add);
        meta.journal.retain(|entry| entry.id != id);
        if !never_synced && !id.starts_with(LOCAL_ID_PREFIX) {
            meta.journal.push(JournalEntry {
                op: JournalOp::Remove,
                id: id.into(),
            });
        }
        self.write_meta(&meta)
    }

    fn rule_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.{RULE_EXTENSION}"))
    }

    fn meta_path(&self) -> PathBuf {
        self.root.join(META_FILE)
    }

    /// 读元数据。只有"文件不存在"才算空的:别的读错误(权限、IO)与 JSON 损坏一律向上
    /// 报错。以前这两类都返回空 `Meta`,调用方紧接着 `write_meta` 就把 meta.json 覆盖成
    /// 空的——规则标题、`git_origin` 和整个 journal 一起丢。
    fn read_meta(&self) -> Result<Meta> {
        let bytes = match std::fs::read(self.meta_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Meta::default())
            }
            Err(error) => return Err(error.into()),
        };
        serde_json::from_slice(&bytes)
            .map_err(|error| Error::Store(format!("rules meta.json is unreadable: {error}")))
    }

    fn write_meta(&self, meta: &Meta) -> Result<()> {
        write_atomic(&self.meta_path(), &serde_json::to_vec_pretty(meta)?)
    }
}

fn assemble(id: &str, knowledge: String, meta: Option<&RuleMeta>, path: &Path) -> RuleRecord {
    match meta {
        Some(meta) => RuleRecord {
            id: id.into(),
            knowledge,
            title: meta.title.clone(),
            created_at: meta.created_at.clone(),
            is_generated: meta.is_generated,
            git_origin: meta.git_origin.clone(),
        },
        // 用户手放的 md 文件没有元数据,用文件名当标题、修改时间当创建时间。
        None => RuleRecord {
            id: id.into(),
            knowledge,
            title: id.into(),
            created_at: file_modified_at(path),
            is_generated: false,
            git_origin: String::new(),
        },
    }
}

fn rule_meta(record: &RuleRecord) -> RuleMeta {
    RuleMeta {
        title: record.title.clone(),
        created_at: record.created_at.clone(),
        is_generated: record.is_generated,
        git_origin: record.git_origin.clone(),
    }
}

fn journal_contains(journal: &[JournalEntry], id: &str, op: JournalOp) -> bool {
    journal.iter().any(|entry| entry.id == id && entry.op == op)
}

fn timestamp(created_at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|time| time.timestamp_millis())
        .unwrap_or(0)
}

fn file_modified_at(path: &Path) -> String {
    let modified = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or_else(|_| std::time::SystemTime::now());
    chrono::DateTime::<chrono::Utc>::from(modified)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Protocol(format!("invalid rule id: {id:?}")));
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let directory = path.parent().expect("rule path has a parent");
    let temporary = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    // 直接 rename 覆盖旧文件（Windows 上 Rust 用的是带 REPLACE_EXISTING 的
    // `MoveFileEx`，一直如此，`local_app/settings.rs` 的 `write_to` 也靠这一点）。
    // 先删旧文件再改名会留一个"旧内容已经没了、新内容还没到位"的窗口：那一刻断电
    // 或强杀，这条规则就从列表里消失。目标文件被别的程序锁住时这一句会失败，但那
    // 是能说清楚、且不动数据的失败。
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, knowledge: &str, created_at: &str) -> RuleRecord {
        RuleRecord {
            id: id.into(),
            knowledge: knowledge.into(),
            title: format!("title-{id}"),
            created_at: created_at.into(),
            is_generated: false,
            git_origin: String::new(),
        }
    }

    fn journal(store: &RuleStore) -> Vec<JournalEntry> {
        store.read_meta().unwrap().journal
    }

    /// 缓存必须是**进程级**的:每轮 Run 都会 `RuleStore::open` 一个全新实例
    /// (`compile/context.rs`),缓存挂在实例上就永不命中,第二轮又要把每个 md 读一遍。
    ///
    /// 逆变:把缓存改回实例字段(或每轮 `open` 后不复用),第二条断言立刻红;直接改坏文件
    /// 那条则钉住"指纹变了必须重读",缓存不能盖住外部改动。
    #[test]
    fn a_fresh_store_reuses_the_process_wide_rule_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rules");
        let write = |name: &str, body: &str| std::fs::write(path.join(name), body).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        write("a.md", "rule a");
        write("b.md", "rule b");
        let before = content_load_count(&path);

        let first = RuleStore::open(path.clone()).unwrap().list().unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(
            content_load_count(&path),
            before + 1,
            "第一次必须真的读目录内容"
        );

        // 第二轮:新实例,目录没变 —— 不许再读内容。
        let second = RuleStore::open(path.clone()).unwrap().list().unwrap();
        assert_eq!(
            content_load_count(&path),
            before + 1,
            "内容没变时第二个实例必须命中进程级缓存(实例级缓存会在这里重读)"
        );
        assert_eq!(second, first);

        // 变了:绕过 store 直接改写文件(长度也变),缓存必须失效。
        write("b.md", "second version, longer than the first");
        let refreshed = RuleStore::open(path.clone()).unwrap().list().unwrap();
        assert_eq!(content_load_count(&path), before + 2, "目录变了必须重读");
        assert_eq!(
            refreshed
                .iter()
                .find(|rule| rule.id == "b")
                .expect("rule b")
                .knowledge,
            "second version, longer than the first"
        );
    }

    #[test]
    fn upserts_lists_and_removes_rules() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&record("100", "older", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        store
            .upsert(&record("200", "newer", "2026-02-01T00:00:00.000Z"))
            .unwrap();

        let listed = store.list().unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>(),
            ["200", "100"],
            "list is sorted by created_at descending"
        );
        assert_eq!(listed[0].knowledge, "newer");
        assert_eq!(listed[0].title, "title-200");

        store.remove("200").unwrap();
        assert!(store.get("200").unwrap().is_none());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        assert!(store.get("../escape").is_err());
        assert!(store.get("a/b").is_err());
        assert!(store.get("").is_err());
    }

    /// 元数据读不动时必须报错。以前损坏的 meta.json 会被当成"空元数据",调用方接着写回
    /// 就把规则标题、git_origin 与整个 journal 覆盖掉了。
    #[test]
    fn unreadable_metadata_is_reported_instead_of_overwritten_with_an_empty_one() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&record("42", "内容", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        store.record_add("42").unwrap();
        let meta_path = store.meta_path();
        let before = std::fs::read(&meta_path).unwrap();

        std::fs::write(&meta_path, b"{ this is not json").unwrap();
        assert!(store.list().is_err(), "损坏的元数据要报错");
        assert!(store.record_update("42").is_err(), "写回路径同样要报错");
        assert_eq!(
            std::fs::read(&meta_path).unwrap(),
            b"{ this is not json",
            "报错之后不得改写 meta.json"
        );

        // 手工修好后照常工作(不是把文件删掉重建):元数据里的标题与 journal 都还在。
        std::fs::write(&meta_path, &before).unwrap();
        assert_eq!(journal(&store).len(), 1);
        assert_eq!(store.get("42").unwrap().unwrap().title, "title-42");
    }

    #[test]
    fn compacts_offline_journal() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();

        // 离线新增后再更新:回放 add 即可携带最新内容,不产生 update 日志。
        store
            .upsert(&record("local-a", "v1", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        store.record_add("local-a").unwrap();
        store.record_update("local-a").unwrap();
        assert_eq!(
            journal(&store),
            vec![JournalEntry {
                op: JournalOp::Add,
                id: "local-a".into()
            }]
        );

        // 离线新增后又删除:上游从未见过它,日志清空。
        store.record_remove("local-a").unwrap();
        assert!(journal(&store).is_empty());

        // 更新上游已有规则:多次更新合并为一条;删除后 update 日志被顶替。
        store.record_update("42").unwrap();
        store.record_update("42").unwrap();
        assert_eq!(
            journal(&store),
            vec![JournalEntry {
                op: JournalOp::Update,
                id: "42".into()
            }]
        );
        store.record_remove("42").unwrap();
        assert_eq!(
            journal(&store),
            vec![JournalEntry {
                op: JournalOp::Remove,
                id: "42".into()
            }]
        );
    }

    #[test]
    fn promote_renames_rule_and_journal_ids() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&record("local-a", "content", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        store.record_add("local-a").unwrap();

        store.promote("local-a", "17353272").unwrap();

        assert!(store.get("local-a").unwrap().is_none());
        let promoted = store.get("17353272").unwrap().unwrap();
        assert_eq!(promoted.knowledge, "content");
        assert_eq!(promoted.title, "title-local-a");
        assert_eq!(journal(&store)[0].id, "17353272");
    }

    #[test]
    fn replace_all_mirrors_upstream_state() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&record("stale", "gone soon", "2026-01-01T00:00:00.000Z"))
            .unwrap();

        store
            .replace_all(&[record(
                "17353272",
                "from upstream",
                "2026-02-01T00:00:00.000Z",
            )])
            .unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "17353272");
        assert_eq!(listed[0].knowledge, "from upstream");
        assert!(store.get("stale").unwrap().is_none());
    }

    /// 回归:上游列表里没有的文件,如果本镜像从未写过,就不能删。
    /// 手写规则与还没推上去的 local- 规则都属于这一类。
    #[test]
    fn replace_all_keeps_files_the_mirror_never_wrote() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&record("stale", "gone soon", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        std::fs::write(root.path().join("rules/manual_rule.md"), "hand written").unwrap();
        std::fs::write(root.path().join("rules/local-pending.md"), "not pushed yet").unwrap();

        store
            .replace_all(&[record(
                "17353272",
                "from upstream",
                "2026-02-01T00:00:00.000Z",
            )])
            .unwrap();

        let ids: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|rule| rule.id)
            .collect();
        assert!(ids.contains(&"17353272".to_string()));
        assert!(
            ids.contains(&"manual_rule".to_string()),
            "手写文件不能被上游列表删掉: {ids:?}"
        );
        assert!(
            ids.contains(&"local-pending".to_string()),
            "还没推上去的本地规则不能被删掉: {ids:?}"
        );
        assert!(
            !ids.contains(&"stale".to_string()),
            "镜像自己写过的过期文件应被清理: {ids:?}"
        );
    }

    #[test]
    fn lists_hand_written_markdown_without_metadata() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        std::fs::write(root.path().join("rules/manual_rule.md"), "hand written").unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "manual_rule");
        assert_eq!(listed[0].title, "manual_rule");
        assert_eq!(listed[0].knowledge, "hand written");
    }

    /// 覆盖同一条规则：旧内容被新内容原子替换，不做"先删再写"。
    #[test]
    fn upsert_replaces_an_existing_rule_in_place() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&record("42", "v1", "2026-01-01T00:00:00.000Z"))
            .unwrap();

        store
            .upsert(&record("42", "v2", "2026-01-01T00:00:00.000Z"))
            .unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1, "覆盖不能留下第二份文件");
        assert_eq!(listed[0].knowledge, "v2");
    }

    /// `write_atomic` 的前提：目标已存在时 `fs::rename` 直接替换它。
    ///
    /// Windows 上 Rust 的 `rename` 走带 `REPLACE_EXISTING` 的 `MoveFileEx`，所以
    /// 写入前不需要先删旧文件（那会留下"旧内容已删、新内容未到"的窗口）。这条测试
    /// 把前提钉住：哪天平台语义变了，这里会先红。
    #[test]
    fn rename_replaces_an_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.md");
        let temporary = root.path().join("temporary.md");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&temporary, b"new").unwrap();

        std::fs::rename(&temporary, &target).unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(!temporary.exists());
    }

    /// 一个读不出来的 md 不该让整份列表失败：其余规则照常返回。
    ///
    /// 这份列表是"本轮 Run 的上下文"，离线降级也靠它；以前 `?` 会把一条坏文件放大成
    /// "所有规则都用不了"。
    #[test]
    fn an_unreadable_rule_file_does_not_fail_the_list() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&record("100", "kept", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        // 用一个目录冒充 .md 文件：`read_to_string` 读它必然失败（Windows 上也一样）。
        std::fs::create_dir(store.rule_path("200")).unwrap();

        let listed = store.list().unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "100");
    }
}
