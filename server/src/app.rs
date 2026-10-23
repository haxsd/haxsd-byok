//! Assembles server dependencies and starts the application services.
use std::{future::IntoFuture, net::SocketAddr, time::Duration};

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::{
    anthropic::ClaudeGateway,
    api,
    codex::CodexGateway,
    config::{Config, ConsoleSource},
    control,
    cursor::{
        prompting::{PromptAssets, PromptCompiler},
        transport::TransportRegistry,
    },
    devin::gateway::DevinGateway,
    local_app::CursorHarness,
    plugin::{PluginRegistry, PluginRuntime},
    provider::ProviderRouter,
    search::WebCache,
    store::Store,
    Result,
};

/// "关键清理"阶段的上界：Codex 路由撤回（它的监听器排水最多 10 秒）+ Cursor 撤回
/// （本机代理 stop 最多 5 秒）。这一段决定用户机器上还留不留下指向死端口的配置，
/// 不能被退出预算砍掉。
pub const CRITICAL_CLEANUP_BUDGET: Duration = Duration::from_secs(15);

/// 排水阶段的上界：主服务与 Codex 监听器各自最多 10 秒（两者并行）。
pub const DRAIN_BUDGET: Duration = Duration::from_secs(10);

/// 上面两段之外再留的余量：文件写入与 SQLite 无法逐一给出超时，得给它们一个上界。
const SHUTDOWN_BUDGET_MARGIN: Duration = Duration::from_secs(5);

/// 桌面壳等 `serve_on` 返回的上界。
///
/// 壳一到点就 `app.exit(0)`（tauri 直接 `process::exit`，不 drop 任何任务），所以它必须
/// 盖住服务端自己声明的全部阶段：**小于关键清理 + 排水就会在收尾中途把进程硬结束**，
/// 排在后面的清理再没机会跑（被强杀时留给用户的就是一份指向死端口的 Codex 路由）。
/// 与服务端共用这一个数，免得两处各写一个又能各自漂移。
pub const SHUTDOWN_BUDGET: Duration = CRITICAL_CLEANUP_BUDGET
    .saturating_add(DRAIN_BUDGET)
    .saturating_add(SHUTDOWN_BUDGET_MARGIN);

pub struct App {
    config: Config,
    router: axum::Router,
    registry: TransportRegistry,
    harness: CursorHarness,
    store: Store,
    clients: crate::network::NetworkClients,
    devin_gateway: DevinGateway,
    codex_gateway: CodexGateway,
    claude_gateway: ClaudeGateway,
    /// 这份数据目录的进程互斥锁，随 `App` 活到进程结束。
    _instance: crate::instance::InstanceLock,
}

impl App {
    pub async fn new(mut config: Config) -> Result<Self> {
        // 先占坑再碰端口与数据库：第二个实例必须在改动任何共享状态之前被挡住。
        let instance = crate::instance::InstanceLock::acquire().await?;
        // 记下持有者：收尾助手靠它区分"我守的实例死了"与"新实例已经接管"；下面那笔
        // 启动清算也用它区分"上一个进程留下的 running 行"与"本进程自己的行"。
        let started_at_ms = instance.record_owner()?;
        let store = Store::connect(&config.database_url).await?;
        // 崩溃与强杀留下的 running 行在这里收尾：它们会让存储回收停摆，也会让调用
        // 列表永远显示"进行中"。只清算严格早于本进程启动的行。
        store.reconcile_interrupted_runs(started_at_ms).await?;
        if config.use_persisted_ports {
            config
                .listen_addr
                .set_port(store.port_settings().await?.service_port);
        }
        let assets = PromptAssets::embedded()?;
        let compiler = PromptCompiler::new(assets);
        let plugin_runtime = PluginRuntime::managed()?;
        let plugins = PluginRegistry::managed(
            store.clone(),
            plugin_runtime.clone(),
            config.app_version.clone(),
        )?;
        let clients = crate::network::NetworkClients::new(store.clone());
        let provider = std::sync::Arc::new(ProviderRouter::new(
            store.clone(),
            plugins.clone(),
            clients.clone(),
            config.provider_request_timeout,
            config.provider_stream_idle_timeout,
        ));
        let codex_gateway = CodexGateway::with_timeouts(
            store.clone(),
            clients.clone(),
            config.provider_request_timeout,
            config.provider_stream_idle_timeout,
        );
        let devin_gateway = DevinGateway::new(store.clone(), provider.clone(), clients.clone());
        let claude_gateway = ClaudeGateway::new(store.clone(), provider.clone(), clients.clone());
        let registry = TransportRegistry::with_plugins(
            store.clone(),
            provider.clone(),
            compiler,
            WebCache::managed()?,
            plugins.clone(),
            crate::config::managed_data_dir()?.join("rules"),
        );
        let control = control::ControlService::new(
            store.clone(),
            provider,
            plugin_runtime,
            plugins,
            clients.clone(),
            devin_gateway.listening(),
        )?;
        let harness = control.cursor_harness().clone();
        let mut router = api::router(registry.clone(), clients.clone(), harness.clone())?;
        // The status endpoint needs both the control service and the gateway's
        // own listening flag, so it is merged here rather than inside either one.
        router = router.merge(control::codex::router(codex_gateway.clone(), store.clone()));
        router = router.merge(control::claude::router(
            claude_gateway.clone(),
            store.clone(),
        ));
        router = router.merge(control::devin_status_router(
            control.clone(),
            devin_gateway.listening(),
        ));
        router = match &config.console {
            Some(ConsoleSource::Directory(directory)) => {
                router.merge(control::web_router(control.clone(), directory))
            }
            Some(ConsoleSource::Proxy(target)) => {
                router.merge(control::proxy_web_router(control.clone(), target.clone()))
            }
            None => router.merge(control::api_router(control.clone())),
        };
        Ok(Self {
            router,
            registry,
            harness,
            store,
            clients,
            devin_gateway,
            codex_gateway,
            claude_gateway,
            config,
            _instance: instance,
        })
    }

    pub fn merge_router(mut self, router: axum::Router) -> Self {
        self.router = self.router.merge(router);
        self
    }

    pub async fn bind(&self) -> Result<TcpListener> {
        let requested = self.config.listen_addr;
        let listener = bind_service_listener(requested, self.config.use_persisted_ports).await?;
        if self.config.use_persisted_ports {
            self.store
                .set_service_port(listener.local_addr()?.port())
                .await?;
        }
        Ok(listener)
    }

    pub fn harness(&self) -> CursorHarness {
        self.harness.clone()
    }

    pub fn store(&self) -> Store {
        self.store.clone()
    }

    pub async fn serve(self) -> Result<()> {
        let listener = self.bind().await?;
        let shutdown = CancellationToken::new();
        let signal_shutdown = shutdown.clone();
        let running = self.serve_on(listener, shutdown);
        tokio::pin!(running);
        tokio::select! {
            result = &mut running => result,
            () = shutdown_signal() => {
                tracing::info!("shutdown signal received; cancelling active runs");
                signal_shutdown.cancel();
                running.await
            }
        }
    }

    pub async fn serve_on(self, listener: TcpListener, shutdown: CancellationToken) -> Result<()> {
        let address = listener.local_addr()?;
        self.registry.web_cache().set_service_addr(address);
        self.harness.set_backend_addr(address);
        tracing::info!(%address, "cursor server listening");
        let registry = self.registry;
        let harness = self.harness;
        let clients = self.clients;
        let devin_gateway = self.devin_gateway;
        let codex_gateway = self.codex_gateway;
        let claude_gateway = self.claude_gateway;
        if let Err(error) = codex_gateway.start().await {
            tracing::error!(%error, "Codex gateway could not start; other integrations remain active");
        }
        if let Err(error) = claude_gateway.start().await {
            tracing::error!(%error, "Claude gateway could not start; other integrations remain active");
        }
        let graceful = shutdown.clone();
        let server = axum::serve(listener, self.router)
            .with_graceful_shutdown(async move {
                graceful.cancelled().await;
            })
            .into_future();
        tokio::pin!(server);

        let gateway_shutdown = shutdown.clone();
        let gateway_task = tokio::spawn(async move {
            if let Err(error) = devin_gateway.serve(gateway_shutdown).await {
                // Devin is an optional integration; a binding or port failure
                // must not stop the Cursor listener that owns this process.
                tracing::error!(%error, "Devin gateway stopped; Cursor listener remains active");
            }
        });

        // 默认模式下出网客户端在构建时就钉住了当时的系统代理；这个任务负责在用户
        // 中途开关代理、换端口或改例外时丢掉缓存，让下一次请求按新配置重建。
        let proxy_watch = tokio::spawn(crate::network::watch_system_proxy(
            clients,
            shutdown.clone(),
        ));

        let maintenance = async {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Err(error) = registry.maintain_storage_if_idle().await {
                    tracing::warn!(%error, "storage retention failed; retained data for retry");
                }
                // 日志清理与数据库无关，放在同一拍里：跟着维护循环走就不会出现"谁忘了调"。
                match crate::diagnostics::prune_logs() {
                    Ok(0) => {}
                    Ok(removed) => tracing::info!(removed, "old log files removed"),
                    Err(error) => tracing::warn!(%error, "log retention failed"),
                }
            }
        };
        tokio::pin!(maintenance);

        // Codex 配置是否已经收回：两条结束路径（服务器自己停下、收到退出信号）各收
        // 一次，收过的路径要告诉下面那句"别再收一遍"。
        let mut codex_cleaned = false;
        let result = async {
            tokio::select! {
                () = &mut maintenance => Ok(()),
                result = &mut server => {
                    clean_up_user_files(&codex_gateway, &harness).await;
                    codex_cleaned = true;
                    result?;
                    Ok(())
                },
                () = shutdown.cancelled() => {
                    clean_up_user_files(&codex_gateway, &harness).await;
                    codex_cleaned = true;
                    registry.shutdown().await;
                    drain_server(&mut server, DRAIN_BUDGET).await?;
                    Ok(())
                }
            }
        }
        .await;
        if !codex_cleaned {
            if let Err(error) = codex_gateway.shutdown().await {
                tracing::warn!(%error, "Codex gateway cleanup failed");
            }
        }
        if let Err(error) = claude_gateway.shutdown().await {
            tracing::warn!(%error, "Claude gateway cleanup failed");
        }
        gateway_task.abort();
        let _ = gateway_task.await;
        proxy_watch.abort();
        let _ = proxy_watch.await;
        result
    }
}

/// 退出时收回写在用户机器上的东西，固定顺序：Codex 路由先走，Cursor 的接管后走。
///
/// Codex 那份是用户**下一次在本应用之外**跑 Codex CLI 时要读的配置，撤回几乎是纯文件
/// 操作；而 `CursorHarness::disable()` 要清客户端的 `state.vscdb`、再停本机代理
/// （最多 5 秒）。预算不够时被砍掉的应该是"给客户端撤代理"，不该是"让用户的 Codex
/// 指向一个没人监听的端口"。
///
/// 两步都失败只记日志：进程已经要结束了，这里没有第二次机会，能收多少收多少。
async fn clean_up_user_files(codex_gateway: &CodexGateway, harness: &CursorHarness) {
    if let Err(error) = codex_gateway.shutdown().await {
        tracing::warn!(%error, "Codex gateway cleanup failed");
    }
    if let Err(error) = harness.disable().await {
        tracing::warn!(%error, "failed to disable Cursor harness during shutdown");
    }
}

/// 等主服务把活跃连接排干，超时就强制关闭。
///
/// 超时不是错误：连接可以被硬断，进程必须能退出——被砍掉的是正在收尾的请求，而
/// 关键清理已经在这之前做完了。`axum::serve` 自己的结束结果是 `io::Result`。
async fn drain_server<F>(server: F, budget: Duration) -> Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    match tokio::time::timeout(budget, server).await {
        Ok(result) => Ok(result?),
        Err(_) => {
            tracing::warn!("graceful shutdown timed out; forcing server close");
            Ok(())
        }
    }
}

async fn bind_service_listener(
    requested: SocketAddr,
    allow_random_fallback: bool,
) -> Result<TcpListener> {
    match TcpListener::bind(requested).await {
        Ok(listener) => Ok(listener),
        Err(error) if allow_random_fallback && requested.port() != 0 => {
            tracing::warn!(%requested, %error, "configured service port unavailable; selecting a random port");
            Ok(TcpListener::bind(SocketAddr::new(requested.ip(), 0)).await?)
        }
        Err(error) => Err(error.into()),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 桌面壳按 `SHUTDOWN_BUDGET` 等 `serve_on`；它必须盖得住服务端声明的每一段。
    ///
    /// 壳超时后走的是 `app.exit(0)`（`process::exit`，不 drop 任务），预算小一截就等于
    /// 在收尾中途把进程硬结束：排在后面的清理再也跑不到，用户拿到的是一个指向死端口的
    /// Codex 路由。
    #[test]
    fn the_shell_shutdown_budget_covers_every_phase() {
        assert!(
            SHUTDOWN_BUDGET >= CRITICAL_CLEANUP_BUDGET + DRAIN_BUDGET,
            "壳的退出预算必须盖住关键清理与排水: {SHUTDOWN_BUDGET:?}"
        );
    }

    /// 排水不吃满：活跃连接排不干时，超时到点就强制关闭，而不是把退出挂在那里。
    #[tokio::test(start_paused = true)]
    async fn the_drain_gives_up_when_the_budget_is_spent() {
        let never_ends = std::future::pending::<std::io::Result<()>>();
        let started = tokio::time::Instant::now();

        drain_server(never_ends, Duration::from_secs(10))
            .await
            .expect("超时不是错误");

        assert_eq!(started.elapsed(), Duration::from_secs(10));
    }

    /// 正常结束（服务自己停下）时排水原样传递结果，不做多余的等待。
    #[tokio::test]
    async fn the_drain_passes_a_finished_server_through() {
        drain_server(async { Ok(()) }, Duration::from_secs(10))
            .await
            .expect("已经结束的服务不该被改写");
    }
}
