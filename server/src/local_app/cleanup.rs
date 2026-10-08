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

use crate::{
    instance::{InstanceLock, Owner},
    Result,
};

/// 探测间隔：一秒一次，代价可以忽略，而"应用死了到配置被撤掉"最多晚一秒。
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// 等应用退出，然后撤掉它留在 Cursor 里的代理配置。返回是否真的改动了文件。
///
/// "我守的那个实例"取自启动时读到的 `app.lock.owner.json`（`pid` + `started_at_ms`）：
/// 如果等锁期间发现**另一个**进程接管了数据目录（用户重启、自动更新），就什么都不做
/// 直接退出——那份配置归新实例管。
pub async fn cleanup_after_exit() -> Result<bool> {
    let directory = crate::config::managed_data_dir()?;
    let Some(owner) = crate::instance::read_owner(&directory) else {
        // 读不到归属就证明不了"这份配置是我守的实例留下的"。按本项目的约定，不能证明是
        // 自己的东西不碰；真被强杀时，下一次启动的 `cleanup_stale_settings` 仍会兜底。
        tracing::warn!("no instance owner record; the exit cleanup helper stands down");
        return Ok(false);
    };
    match wait_for_lock(&directory, &owner, PROBE_INTERVAL).await? {
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
    owner: &Owner,
    interval: Duration,
) -> Result<Option<InstanceLock>> {
    loop {
        if let Some(lock) = InstanceLock::try_acquire_in(directory)? {
            return Ok(Some(lock));
        }
        // 记录里的编号与启动时刻都不是原来了：PID 被复用也算接管（见 `Owner`）。
        if crate::instance::read_owner(directory)
            .is_some_and(|current| !current.is_same_process(owner))
        {
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
        let owner = crate::instance::read_owner(directory.path()).expect("the owner record");
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(running);
        });

        let lock = wait_for_lock(directory.path(), &owner, Duration::from_millis(10))
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

        let outcome = wait_for_lock(
            directory.path(),
            &Owner {
                pid: u32::MAX,
                started_at_ms: Some(1),
            },
            Duration::from_millis(1),
        )
        .await
        .expect("waiting does not fail");

        assert!(outcome.is_none(), "the janitor must stand down");
    }

    /// PID 复用：编号一样、启动时刻不同，说明"我守的实例"已经死了，助手必须放手。
    #[tokio::test]
    async fn a_recycled_pid_with_a_different_start_time_ends_the_wait() {
        let directory = tempfile::tempdir().unwrap();
        let running = InstanceLock::try_acquire_in(directory.path())
            .unwrap()
            .expect("the fixture holds the lock");
        let started_at_ms = running.record_owner().unwrap();

        let outcome = wait_for_lock(
            directory.path(),
            &Owner {
                pid: std::process::id(),
                started_at_ms: Some(started_at_ms + 1_000),
            },
            Duration::from_millis(1),
        )
        .await
        .expect("waiting does not fail");

        assert!(
            outcome.is_none(),
            "同一个 PID 但启动时刻不同不能当成原来的实例"
        );
    }
}
