//! 应用被强杀后的收尾：替它撤掉写进 Cursor 的代理配置、注入的本地账号、Codex 路由与
//! Devin 宿主插件补丁。
//!
//! 正常退出会走 `CursorHarness::disable()`、`CodexGateway::shutdown()` 与界面上的「恢复
//! 原文件」；`taskkill /F`、崩溃、断电都没有那个机会，而痕迹一旦留在那里，Cursor 就会
//! 一直对着一个没人监听的端口报"连不上代理"，Codex CLI 与 Devin 客户端则每个请求都连接
//! 失败。桌面壳因此另起一个**独立进程**（同一个可执行文件加 `--cleanup-after-exit`）守着：
//! 它等实例锁被释放（= 应用真的没了），把这些痕迹撤掉，然后自己退出。
//!
//! 判定"还在不在"用实例锁而不是进程句柄或心跳：锁跟着内核句柄走，强杀也会立刻释放，
//! 不需要额外的进程 API，也不会因为探活失败误判成"已经死了"。

use std::{path::Path, time::Duration};

use crate::{
    instance::{InstanceLock, Owner},
    store::Store,
    Result,
};

/// 探测间隔：一秒一次，代价可以忽略，而"应用死了到配置被撤掉"最多晚一秒。
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// 等应用退出，然后撤掉它留在用户机器上的接管痕迹。返回是否真的改动了文件。
///
/// "我守的那个实例"取自启动时读到的 `app.lock.owner.json`（`pid` + `started_at_ms`）：
/// 如果等锁期间发现**另一个**进程接管了数据目录（用户重启、自动更新），就什么都不做
/// 直接退出——那些文件归新实例管。
pub async fn cleanup_after_exit() -> Result<bool> {
    let directory = crate::config::managed_data_dir()?;
    let Some(owner) = crate::instance::read_owner(&directory) else {
        // 读不到归属就证明不了"这些痕迹是我守的实例留下的"。按本项目的约定，不能证明是
        // 自己的东西不碰；真被强杀时，下一次启动的 `cleanup_stale_settings` 仍会兜底。
        tracing::warn!("no instance owner record; the exit cleanup helper stands down");
        return Ok(false);
    };
    if !instance_has_exited(&directory, &owner, PROBE_INTERVAL).await? {
        return Ok(false);
    }
    // 确认死亡到动手之间已经有新实例接管（用户重启、自动更新）：那些文件归它管，
    // 助手这时候写只会擦掉它刚写好的东西。清理本身只有几毫秒到一秒，这里挡住的就是
    // "重启恰好落在这一刻"。
    if crate::instance::read_owner(&directory)
        .is_some_and(|current| !current.is_same_process(&owner))
    {
        tracing::info!("a new instance took over; the exit cleanup helper stands down");
        return Ok(false);
    }
    clean_leftovers().await
}

/// 等锁被释放，**确认**旧实例真的没了，然后把锁还回去。
///
/// 锁只用来确认死亡，不覆盖清理：清理要打开 Cursor 的 `state.vscdb`（Cursor 自己也在
/// 写那个库，最坏要等它的写锁）并改 `settings.json`。把这段算进持锁时间，新实例那
/// 5 秒的锁重试窗口就会被吃光，用户"退出后马上重开"拿到的是 "another instance is
/// already running"——一次静默的启动失败（`desktop.rs` 的 setup 把它变成 panic）。
/// 这里不把 `InstanceLock` 交出去，类型上就不可能有人握着它去清理。
async fn instance_has_exited(directory: &Path, owner: &Owner, interval: Duration) -> Result<bool> {
    match wait_for_lock(directory, owner, interval).await? {
        Some(lock) => {
            drop(lock);
            Ok(true)
        }
        None => Ok(false),
    }
}

/// 撤掉应用留在用户机器上的接管痕迹，返回是否真的改动了文件。
///
/// 账号先走、配置后走，与 `CursorHarness::disable()` 同一个顺序：配置撤不掉
/// （`settings.json` 被用户改出语法错误、文件被占）时不能再留下一个带着假 Token 的
/// 客户端——那是"登录着但什么都不通"，比一条撤不掉的代理配置难查得多。
///
/// 每一步失败都不该挡住后面的：助手没有第二次机会，能撤多少撤多少。
async fn clean_leftovers() -> Result<bool> {
    super::clear_injected_account().await?;
    let mut changed = super::settings::clear_proxy_settings()?;
    match restore_codex_routing().await {
        Ok(restored) => changed |= restored,
        Err(error) => tracing::warn!(
            error = %crate::diagnostics::error_chain(&error),
            "exit cleanup could not restore the Codex route"
        ),
    }
    changed |= restore_devin_settings();
    Ok(changed)
}

/// 把 Devin 用户设置里的两个端点与扩展读取开关还原回接管前的值。
///
/// 优雅退出**不**还原：界面上的「恢复配置」是用户可见的停用步骤，自动还原会让那一步
/// 失效。但被强杀时没人走那条路，设置会一直指向已经消失的端口，扩展也会停留在本应用
/// 放开的读取开关上。两边都只按各自落盘的收据恢复，收据不成立就什么都不做。
fn restore_devin_settings() -> bool {
    let settings = crate::devin::host_detect::settings_path()
        .is_ok_and(|path| crate::devin::host_config::restore_persisted(&path));
    let extension = crate::devin::host_detect::detect()
        .path()
        .is_some_and(crate::devin::host_patch::restore_persisted);
    settings || extension
}

/// 把用户 `~/.codex/config.toml` 里的路由还回去。
///
/// 正常退出由 `CodexGateway::shutdown()` 撤回；被强杀时没人做这件事，那份配置会一直
/// 指向一个没人监听的端口——用户在这期间跑 Codex CLI，每个请求都连接失败，而应用根本
/// 没在跑，界面与提示都不可达。这里按收据还原，与「撤掉 Cursor 接管」同一个决策点。
async fn restore_codex_routing() -> Result<bool> {
    restore_codex_routing_at(&crate::config::Config::desktop()?.database_url).await
}

/// 只在收据显示"受管字段确实是我们写下的那一份"时还原；读不到设置、没有收据、或配置
/// 被外部改过都什么都不做——不能证明是我们的东西就不碰（`codex::config::restore`
/// 自身也按收据判断，`inspect().applied` 是它的前置条件）。
async fn restore_codex_routing_at(database_url: &str) -> Result<bool> {
    let store = Store::connect(database_url).await?;
    let home = store.codex_settings().await?.resolved_home()?;
    if !crate::codex::config::inspect(&home)?.applied {
        return Ok(false);
    }
    crate::codex::config::restore(&home)?;
    tracing::info!(home = %home.display(), "exit cleanup restored the Codex route");
    Ok(true)
}

/// 等到实例锁空闲，或者等到它被另一个进程接管。
///
/// 拿到锁说明上一个实例已经结束；被接管说明新实例已经在跑，收尾助手没有事情可做。
/// 锁只用来读这个信号，拿到之后立刻还回去（见 `instance_has_exited`）。
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
    use crate::codex::{config::ConfigRoute, CodexSettings};

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

    /// 确认死亡之后必须立刻放开锁。
    ///
    /// 清理要打开 Cursor 的 `state.vscdb`（Cursor 自己也在写那个库），把这段算进持锁
    /// 时间，新实例 5 秒的锁重试窗口就会被吃光：用户"退出后马上重开"拿到的是
    /// "another instance is already running"，再往上就是 setup 失败即 panic。
    #[tokio::test]
    async fn the_janitor_hands_the_lock_back_before_cleaning() {
        let directory = tempfile::tempdir().unwrap();
        let running = InstanceLock::try_acquire_in(directory.path())
            .unwrap()
            .expect("the fixture holds the lock");
        running.record_owner().unwrap();
        let owner = crate::instance::read_owner(directory.path()).expect("the owner record");
        drop(running);

        assert!(
            instance_has_exited(directory.path(), &owner, Duration::from_millis(10))
                .await
                .unwrap(),
            "旧实例已经结束，助手应当确认死亡"
        );

        let successor = InstanceLock::try_acquire_in(directory.path())
            .unwrap()
            .expect("助手确认死亡后必须立刻放开锁，否则紧接着的重启会被单实例锁拒绝");
        assert!(successor.path().ends_with("app.lock"));
    }

    /// 强杀时没人走 `CodexGateway::shutdown()`：用户的 `config.toml` 会一直指着那个
    /// 已经没人监听的端口，而这段时间里应用根本没在跑——Codex CLI 每个请求都连接失败，
    /// 界面与提示都不可达。助手必须按收据把它还原。
    #[tokio::test]
    async fn the_janitor_restores_the_codex_route_a_killed_instance_left_behind() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}",
            directory.path().join("haxsd-byok.db").display()
        );
        let home = directory.path().join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        let original = "model = \"gpt-5\"\n";
        std::fs::write(home.join("config.toml"), original).unwrap();
        write_codex_settings(&database_url, &home).await;

        assert!(
            !restore_codex_routing_at(&database_url).await.unwrap(),
            "还没有收据（路由不是我们写的）时什么都不做"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            original
        );

        crate::codex::config::apply(&home, &route(&home)).unwrap();
        assert!(crate::codex::config::inspect(&home).unwrap().applied);

        assert!(
            restore_codex_routing_at(&database_url).await.unwrap(),
            "收据在，路由必须还原"
        );
        assert!(!crate::codex::config::inspect(&home).unwrap().applied);
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            original,
            "把用户原来的 config.toml 一字不动地还回去"
        );
    }

    /// 往设置表里写一份"Codex 网关已启用、home 在临时目录"的记录：助手就是从这里
    /// 学到该去哪个目录还原（真实值是设置页写下的）。
    async fn write_codex_settings(database_url: &str, home: &Path) {
        let store = Store::connect(database_url).await.unwrap();
        let settings = CodexSettings {
            enabled: true,
            port: 43_120,
            home: home.display().to_string(),
            model_hashes: vec!["test-model".into()],
            default_model_hash: Some("test-model".into()),
        };
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) \
             VALUES ('codex_gateway', ?, 0)",
        )
        .bind(serde_json::to_string(&settings).unwrap())
        .execute(store.pool())
        .await
        .unwrap();
    }

    fn route(home: &Path) -> ConfigRoute {
        ConfigRoute {
            model: "test-model".into(),
            base_url: "http://127.0.0.1:43120/v1".into(),
            token: "haxsd-test-token".into(),
            catalog_path: home.join("catalog.json"),
        }
    }
}
