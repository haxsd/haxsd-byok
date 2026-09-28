//! 同一份数据目录只允许一个进程在跑。
//!
//! 两个实例各自起一个本地代理，端口、`settings.json`、数据库三处都会互相踩：占用者
//! 抢不到配置端口就退到随机端口，Cursor 于是被指向一个马上消失的地址。这里用文件锁
//! 挡住第二个实例。
//!
//! 用文件锁而不是 PID 文件或端口标记：锁跟着句柄走，进程被杀（含 `taskkill /F`）
//! 由内核立刻释放，不会留下"上次崩溃后永远起不来"的状态。

use std::{
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{config, Error, Result};

const LOCK_FILE_NAME: &str = "app.lock";
/// 记录持锁进程；收尾助手用它区分"我的实例死了"和"新实例已经接管"。
const OWNER_FILE_NAME: &str = "app.lock.owner.json";

/// 拿不到锁时的重试窗口。
///
/// 与代理端口同一个套路：占用者常常是"上一次的我们"——自动更新会先启动新进程、再退出
/// 旧进程，那几百毫秒里旧的还握着锁。立刻报"已经在运行"会让用户在更新后打不开应用。
const ACQUIRE_RETRY_WINDOW: Duration = Duration::from_secs(5);
const ACQUIRE_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// 进程存活期间持有的互斥锁；drop 或进程结束即释放。
pub struct InstanceLock {
    /// 只为了持有锁：句柄一关，内核就把锁还回去。
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// 用当前数据目录加锁。窗口内拿不到就说明已经有一个实例在跑。
    pub async fn acquire() -> Result<Self> {
        Self::acquire_in_with(&config::managed_data_dir()?, ACQUIRE_RETRY_WINDOW).await
    }

    /// 试一次，不等待：`Ok(None)` 表示另一个实例正持有它。
    pub(crate) fn try_acquire_in(directory: &Path) -> Result<Option<Self>> {
        let path = directory.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file, path })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    async fn acquire_in_with(directory: &Path, window: Duration) -> Result<Self> {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            if let Some(lock) = Self::try_acquire_in(directory)? {
                return Ok(lock);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Config(format!(
                    "another haxsd byok instance is already running with this data directory ({})",
                    directory.join(LOCK_FILE_NAME).display()
                )));
            }
            tokio::time::sleep(ACQUIRE_RETRY_INTERVAL).await;
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 记下持锁的进程，给收尾助手判断"我守的那个实例还在不在、有没有换成别人"。
    pub fn record_owner(&self) -> Result<()> {
        let owner = serde_json::json!({
            "pid": std::process::id(),
            "started_at_ms": crate::store::now_ms(),
        });
        std::fs::write(self.owner_path(), serde_json::to_vec_pretty(&owner)?)?;
        Ok(())
    }

    fn owner_path(&self) -> PathBuf {
        self.path.with_file_name(OWNER_FILE_NAME)
    }
}

/// 读出当前记录的数据目录持有者。文件可能在两次启动之间短暂缺失或损坏。
pub(crate) fn read_owner(directory: &Path) -> Option<u32> {
    let raw = std::fs::read(directory.join(OWNER_FILE_NAME)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    value
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .map(|pid| pid as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_second_instance_cannot_take_the_same_data_directory() {
        let directory = tempfile::tempdir().unwrap();
        let window = Duration::from_millis(50);
        let first = InstanceLock::acquire_in_with(directory.path(), window)
            .await
            .expect("first instance");
        assert!(first.path().ends_with("app.lock"));

        let error = match InstanceLock::acquire_in_with(directory.path(), window).await {
            Ok(_) => panic!("the second instance must be refused"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("already running"), "{error}");

        drop(first);
        InstanceLock::acquire_in_with(directory.path(), window)
            .await
            .expect("the lock follows the first instance");
    }

    /// 自动更新时旧进程还在退出：新进程必须在窗口内等到锁，而不是报错退出。
    #[tokio::test]
    async fn a_lock_released_during_the_window_is_taken_over() {
        let directory = tempfile::tempdir().unwrap();
        let first = InstanceLock::acquire_in_with(directory.path(), Duration::from_secs(1))
            .await
            .expect("first instance");
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(first);
        });

        InstanceLock::acquire_in_with(directory.path(), Duration::from_secs(2))
            .await
            .expect("the successor takes the lock over");
        release.await.unwrap();
    }
}
