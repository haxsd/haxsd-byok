//! Stores plugin-owned JSON with private permissions and atomic replacement.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::{config, Error, Result};

#[derive(Clone)]
pub struct PluginDataStore {
    root: PathBuf,
    locks: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
}

impl PluginDataStore {
    pub fn managed() -> Result<Self> {
        Self::new(config::managed_data_dir()?.join("plugins/data"))
    }

    #[cfg(test)]
    pub(super) fn for_test(root: PathBuf) -> Result<Self> {
        Self::new(root)
    }

    fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        set_directory_permissions(&root)?;
        Ok(Self {
            root,
            locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn read(&self, plugin_id: &str, key: &str) -> Result<serde_json::Value> {
        let path = self.path(plugin_id, key)?;
        let lock = self.lock(plugin_id);
        let _guard = lock.lock().await;
        self.read_locked(&path).await
    }

    /// 在一个临界区内完成"读 → 改 → 写"，闭包返回 (要写入的值, 调用方要的结果)。
    ///
    /// 读与写各自加锁是不够的：同一插件的两个并发更新会读到同一份快照、各自整份写回，
    /// 后写的把先写的盖掉（丢更新）。宿主本来就并发调用插件（多个资源同时刷新、控制面
    /// 同时操作），所以读改写必须共用同一把插件锁。闭包是同步的：锁只在这一个临界区里，
    /// 任何 await 都在锁外。
    pub async fn modify<T, F>(&self, plugin_id: &str, key: &str, apply: F) -> Result<T>
    where
        F: FnOnce(serde_json::Value) -> Result<(serde_json::Value, T)> + Send,
    {
        let path = self.path(plugin_id, key)?;
        let lock = self.lock(plugin_id);
        let _guard = lock.lock().await;
        let current = self.read_locked(&path).await?;
        let (next, output) = apply(current)?;
        self.write_locked(&path, key, &next)
            .await
            // 带上具体路径,Windows 上的拒绝访问才能定位到是哪一步。
            .map_err(|error| {
                Error::Config(format!(
                    "plugin data write failed at {}: {error}",
                    path.display()
                ))
            })?;
        Ok(output)
    }

    async fn read_locked(&self, path: &Path) -> Result<serde_json::Value> {
        match tokio::fs::read(path).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // "没配过"与"读不到"在调用方看来都是空,但后者需要留一条诊断线索:
                // 数据文件被删/被搬走时,日志里至少能看到是哪个文件不见了。
                tracing::debug!(path = %path.display(), "plugin data file is missing");
                Ok(serde_json::Value::Null)
            }
            Err(error) => Err(Error::Config(format!(
                "plugin data read failed at {}: {error}",
                path.display()
            ))),
        }
    }

    /// 直接覆盖一个键，不做读取。生产路径都走 [`Self::modify`]（读改写必须在同一个
    /// 临界区里）；这里留给数据层自己的用例做前置状态与失败路径。
    #[cfg(test)]
    pub(super) async fn update(
        &self,
        plugin_id: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<()> {
        let path = self.path(plugin_id, key)?;
        let lock = self.lock(plugin_id);
        let _guard = lock.lock().await;
        self.write_locked(&path, key, value)
            .await
            // 带上具体路径,Windows 上的拒绝访问才能定位到是哪一步。
            .map_err(|error| {
                Error::Config(format!(
                    "plugin data write failed at {}: {error}",
                    path.display()
                ))
            })
    }

    /// 全程使用同步 IO 在阻塞线程完成:tokio 异步文件的关闭是延迟的,
    /// 替换前句柄可能仍被本进程持有;同步写入保证替换时句柄已确定关闭。
    async fn write_locked(&self, path: &Path, key: &str, value: &serde_json::Value) -> Result<()> {
        let directory = path
            .parent()
            .expect("plugin data path has a parent")
            .to_owned();
        let temporary = directory.join(format!(".{key}.{}.tmp", uuid::Uuid::new_v4()));
        let target = path.to_owned();
        let bytes = serde_json::to_vec_pretty(value)?;
        tokio::task::spawn_blocking(move || {
            // Windows 上杀软或索引器会短暂锁住新建文件,任何一步都可能
            // 拒绝访问,因此把整个序列作为一个整体重试。
            let mut attempts = 0;
            loop {
                match write_once(&directory, &temporary, &target, &bytes) {
                    Ok(()) => return Ok(()),
                    Err((step, error)) if attempts < 20 && transient(&error) => {
                        attempts += 1;
                        tracing::debug!(step, attempts, %error, "retrying plugin data write");
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err((step, error)) => {
                        let _ = std::fs::remove_file(&temporary);
                        tracing::warn!(
                            path = %target.display(),
                            step,
                            attempts,
                            %error,
                            "plugin data write failed"
                        );
                        return Err(Error::Config(format!("{step}: {error}")));
                    }
                }
            }
        })
        .await
        .expect("plugin data write task panicked")
    }

    pub async fn clear(&self, plugin_id: &str) -> Result<()> {
        validate_component(plugin_id, "plugin id")?;
        let lock = self.lock(plugin_id);
        let _guard = lock.lock().await;
        let path = self.root.join(plugin_id);
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Config(format!(
                "plugin data cleanup failed at {}: {error}",
                path.display()
            ))),
        }
    }

    fn path(&self, plugin_id: &str, key: &str) -> Result<PathBuf> {
        validate_component(plugin_id, "plugin id")?;
        validate_component(key, "plugin data key")?;
        Ok(self.root.join(plugin_id).join(format!("{key}.json")))
    }

    fn lock(&self, plugin_id: &str) -> Arc<AsyncMutex<()>> {
        self.locks
            .lock()
            .entry(plugin_id.to_owned())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

/// 单次完整写入:建目录、写临时文件、落盘、原子替换。
/// 失败时返回失败步骤的标签,供上层区分重试与报错。
fn write_once(
    directory: &Path,
    temporary: &Path,
    target: &Path,
    bytes: &[u8],
) -> std::result::Result<(), (&'static str, std::io::Error)> {
    use std::io::Write;
    std::fs::create_dir_all(directory).map_err(|error| ("create data directory", error))?;
    let _ = set_directory_permissions(directory);
    let mut file =
        std::fs::File::create(temporary).map_err(|error| ("create temporary file", error))?;
    file.write_all(bytes)
        .map_err(|error| ("write temporary file", error))?;
    file.sync_all()
        .map_err(|error| ("sync temporary file", error))?;
    drop(file);
    let _ = set_file_permissions(temporary);
    // 直接改名覆盖:两边的 rename 都能替换已存在的文件(Rust 在 Windows 上用
    // MoveFileExW + MOVEFILE_REPLACE_EXISTING)。先删除再改名会留一个"旧文件已删、
    // 新文件还没就位"的窗口,此时崩溃或改名失败就永久丢掉旧值——而这里存的是
    // 插件的账号 token 与模型目录。共享冲突仍由外层的 transient 重试兜住。
    std::fs::rename(temporary, target).map_err(|error| ("replace target file", error))?;
    let _ = set_file_permissions(target);
    Ok(())
}

/// Windows 下拒绝访问(5)与共享冲突(32)通常是杀软或索引器的
/// 瞬时锁定,值得重试;其余错误与其他平台一律直接失败。
fn transient(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        const ACCESS_DENIED: i32 = 5;
        const SHARING_VIOLATION: i32 = 32;
        matches!(
            error.raw_os_error(),
            Some(ACCESS_DENIED | SHARING_VIOLATION)
        )
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

fn validate_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        // Windows 会归一化尾部句点,不能让这些名称指向其他目录。
        || value.ends_with('.')
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(Error::Config(format!("invalid {label}: {value}")));
    }
    Ok(())
}

fn set_directory_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn set_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_aliases_are_not_plugin_path_components() {
        for value in [".", "..", "...", "com."] {
            assert!(
                validate_component(value, "plugin id").is_err(),
                "目录别名必须在文件操作前被拒绝: {value}"
            );
        }
        assert!(validate_component("com.example", "plugin id").is_ok());
        assert!(validate_component("plugin_state-1", "plugin data key").is_ok());
    }

    #[tokio::test]
    async fn directory_aliases_cannot_read_write_or_clear_plugin_data() {
        let root = tempfile::tempdir().unwrap();
        let data_root = root.path().join("data");
        let store = PluginDataStore::new(data_root.clone()).unwrap();
        let sentinel = root.path().join("outside.json");
        std::fs::write(&sentinel, br#"{"outside":true}"#).unwrap();
        store
            .update("com.example", "state", &serde_json::json!({"ok":true}))
            .await
            .unwrap();
        for value in [".", "..", "...", "com."] {
            // 先确认校验拒绝,失败时不执行可能删除临时父目录的旧实现。
            assert!(validate_component(value, "plugin id").is_err());
            assert!(store.read(value, "outside").await.is_err());
            assert!(store
                .update(value, "outside", &serde_json::json!({}))
                .await
                .is_err());
            assert!(store.clear(value).await.is_err());
        }
        assert_eq!(std::fs::read(&sentinel).unwrap(), br#"{"outside":true}"#);
        assert!(data_root.exists());
        assert_eq!(
            store.read("com.example", "state").await.unwrap()["ok"],
            true
        );
    }

    #[tokio::test]
    async fn writes_reads_and_removes_json() {
        let root = tempfile::tempdir().unwrap();
        let store = PluginDataStore::new(root.path().join("data")).unwrap();
        store
            .update(
                "com.example",
                "state",
                &serde_json::json!({"token":"secret"}),
            )
            .await
            .unwrap();
        assert_eq!(
            store.read("com.example", "state").await.unwrap()["token"],
            "secret"
        );
        store.clear("com.example").await.unwrap();
        assert!(store.read("com.example", "state").await.unwrap().is_null());
    }

    /// 回归:替换失败时不得动旧值,也不得留下临时文件。
    /// 旧实现先在 Windows 上删除目标文件再改名,失败即永久丢凭据。
    #[tokio::test]
    async fn a_failed_replace_keeps_the_target_and_leaves_no_temporary() {
        let root = tempfile::tempdir().unwrap();
        let store = PluginDataStore::new(root.path().join("data")).unwrap();
        store
            .update(
                "com.example",
                "state",
                &serde_json::json!({"token":"first"}),
            )
            .await
            .unwrap();
        let directory = root.path().join("data").join("com.example");
        let target = directory.join("state.json");
        // 用同名目录占住目标路径,让改名这一步必然失败。
        std::fs::remove_file(&target).unwrap();
        std::fs::create_dir(&target).unwrap();

        let failed = store
            .update(
                "com.example",
                "state",
                &serde_json::json!({"token":"second"}),
            )
            .await;
        assert!(failed.is_err(), "目标被占住时必须报错,不能静默成功");
        assert!(target.is_dir(), "失败路径不得删除或覆盖占位目录");
        let leftovers: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers,
            vec![std::ffi::OsString::from("state.json")],
            "失败后只应剩下目标本身,临时文件必须清理"
        );
    }

    /// 真故障注入：替换过程中目标文件**不能有一刻不存在**。
    ///
    /// 上面那条同名目录占位的用例证明不了这一点——旧实现（先 `remove_file(target)` 再
    /// `rename`）在那种注入下会因为 `remove_file` 先失败而照样通过。真正的窗口是"旧文件已删、
    /// 新文件还没就位"的瞬间：此刻崩溃就是永久丢失，而并发读的人会直接看到 NotFound。
    /// 所以这里让一个读者从头到尾轮询同一个文件，同时连续替换 150 次：只要有一次读到
    /// NotFound，就说明替换不是原子的。
    #[tokio::test]
    async fn a_replacement_never_lets_the_target_disappear() {
        let root = tempfile::tempdir().unwrap();
        let store = PluginDataStore::new(root.path().join("data")).unwrap();
        store
            .update(
                "com.example",
                "state",
                &serde_json::json!({"token":"first"}),
            )
            .await
            .unwrap();
        let target = root
            .path()
            .join("data")
            .join("com.example")
            .join("state.json");

        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let missing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            use std::sync::atomic::Ordering;
            let target = target.clone();
            let seen = seen.clone();
            let missing = missing.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match std::fs::read(&target) {
                        Ok(_) => {
                            seen.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            missing.store(true, Ordering::Relaxed);
                            return;
                        }
                        Err(_) => {}
                    }
                }
            })
        };

        for round in 0..150 {
            store
                .update("com.example", "state", &serde_json::json!({"token": round}))
                .await
                .unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().unwrap();

        assert!(
            !missing.load(std::sync::atomic::Ordering::Relaxed),
            "替换过程中目标文件消失过：这正是旧实现丢凭据的窗口"
        );
        assert!(
            seen.load(std::sync::atomic::Ordering::Relaxed) > 10,
            "读者必须真的读到过文件，否则这条用例什么也没证明"
        );
    }
}
