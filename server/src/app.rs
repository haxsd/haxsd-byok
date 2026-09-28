//! Assembles server dependencies and starts the application services.
use std::{future::IntoFuture, net::SocketAddr, time::Duration};

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::{
    api,
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

pub struct App {
    config: Config,
    router: axum::Router,
    registry: TransportRegistry,
    harness: CursorHarness,
    store: Store,
    clients: crate::network::NetworkClients,
    devin_gateway: DevinGateway,
    /// 这份数据目录的进程互斥锁，随 `App` 活到进程结束。
    _instance: crate::instance::InstanceLock,
}

impl App {
    pub async fn new(mut config: Config) -> Result<Self> {
        // 先占坑再碰端口与数据库：第二个实例必须在改动任何共享状态之前被挡住。
        let instance = crate::instance::InstanceLock::acquire().await?;
        // 记下持有者：收尾助手靠它区分"我守的实例死了"与"新实例已经接管"。
        instance.record_owner()?;
        let store = Store::connect(&config.database_url).await?;
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
        let devin_gateway = DevinGateway::new(store.clone(), provider.clone(), clients.clone());
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
        )?;
        let harness = control.cursor_harness().clone();
        let mut router = api::router(registry.clone(), clients.clone())?;
        // The status endpoint needs both the control service and the gateway's
        // own listening flag, so it is merged here rather than inside either one.
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
            }
        };
        tokio::pin!(maintenance);

        let result = tokio::select! {
            () = &mut maintenance => Ok(()),
            result = &mut server => {
                if let Err(error) = harness.disable().await {
                    tracing::warn!(%error, "failed to disable Cursor harness after server stop");
                }
                result?;
                Ok(())
            },
            () = shutdown.cancelled() => {
                if let Err(error) = harness.disable().await {
                    tracing::warn!(%error, "failed to disable Cursor harness during shutdown");
                }
                registry.shutdown().await;
                match tokio::time::timeout(Duration::from_secs(10), &mut server).await {
                    Ok(result) => result?,
                    Err(_) => {
                        tracing::warn!("graceful shutdown timed out; forcing server close");
                    }
                }
                Ok(())
            }
        };
        gateway_task.abort();
        let _ = gateway_task.await;
        proxy_watch.abort();
        let _ = proxy_watch.await;
        result
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
