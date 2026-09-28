//! 应用被强杀后的收尾：替它撤掉写进 Cursor 的代理配置。
//!
//! 正常退出会走 `CursorHarness::disable()`；`taskkill /F`、崩溃、断电都没有那个机会，
//! 而配置一旦留在那里，Cursor 就会一直对着一个没人监听的端口报"连不上代理"。桌面壳
//! 因此另起一个**独立进程**（同一个可执行文件加 `--cleanup-after-exit`）守着：它等
//! 实例锁被释放（= 应用真的没了），把那份配置撤掉，然后自己退出。
//!
//! 判定"还在不在"用实例锁而不是进程句柄或心跳：锁跟着内核句柄走，强杀也会立刻释放，
//! 不需要额外的进程 API，也不会因为探活失败误判成"已经死了"。

use std::{path::Path, time::Duration};

use crate::{instance::InstanceLock, Result};

/// 探测间隔：一秒一次，代价可以忽略，而"应用死了到配置被撤掉"最多晚一秒。
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// 等应用退出，然后撤掉它留在 Cursor 里的代理配置。返回是否真的改动了文件。
///
/// `owner_pid` 是启动这个助手的进程：如果等锁期间发现**另一个**进程接管了数据目录
/// （用户重启、自动更新），就什么都不做直接退出——那份配置归新实例管。
pub async fn cleanup_after_exit(owner_pid: u32) -> Result<bool> {
    let directory = crate::config::managed_data_dir()?;
    match wait_for_lock(&directory, owner_pid, PROBE_INTERVAL).await? {
        Some(_lock) => {
            // 持有锁的这几毫秒里，新实例的启动会在自己的重试窗口内等一等；这样"收拾旧
            // 配置"与"新实例写新配置"不会交错。
            super::settings::clear_proxy_settings()
        }
        None => Ok(false),
    }
}

/// 等到实例锁空闲，或者等到它被另一个进程接管。
///
/// 拿到锁说明上一个实例已经结束（清理期间由本进程持有）；被接管说明新实例已经在跑，
/// 收尾助手没有事情可做。
async fn wait_for_lock(
    directory: &Path,
    owner_pid: u32,
    interval: Duration,
) -> Result<Option<InstanceLock>> {
    loop {
        if let Some(lock) = InstanceLock::try_acquire_in(directory)? {
            return Ok(Some(lock));
        }
        if crate::instance::read_owner(directory).is_some_and(|pid| pid != owner_pid) {
            return Ok(None);
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waiting_ends_when_the_running_instance_releases_the_lock() {
        let directory = tempfile::tempdir().unwrap();
        let running = InstanceLock::try_acquire_in(directory.path())
            .unwrap()
            .expect("the fixture holds the lock");
        running.record_owner().unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(running);
        });

        let lock = wait_for_lock(
            directory.path(),
            std::process::id(),
            Duration::from_millis(10),
        )
        .await
        .expect("the janitor takes over once the instance is gone")
        .expect("the lock is free");

        assert!(lock.path().ends_with("app.lock"));
        release.await.unwrap();
    }

    /// 用户重启（或自动更新）之后，旧助手必须放手：那份配置归新实例管。
    #[tokio::test]
    async fn a_new_instance_taking_over_ends_the_wait() {
        let directory = tempfile::tempdir().unwrap();
        let running = InstanceLock::try_acquire_in(directory.path())
            .unwrap()
            .expect("the fixture holds the lock");
        running.record_owner().unwrap();

        let outcome = wait_for_lock(directory.path(), u32::MAX, Duration::from_millis(1))
            .await
            .expect("waiting does not fail");

        assert!(outcome.is_none(), "the janitor must stand down");
    }
}
