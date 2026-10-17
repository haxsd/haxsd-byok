//! Verifies the Devin gateway is a separate listener that cannot take Cursor down.
//!
//! The Devin design claims two things that unit tests alone do not prove: the
//! gateway owns its own listeners, and stopping it leaves the Cursor service
//! listener serving. This test owns a stand-in Cursor listener, runs the real
//! gateway next to it, and checks both halves of that claim.

#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::{net::TcpListener as StdTcpListener, sync::Arc, time::Duration};

use cursor_server::{
    devin::{gateway::DevinGateway, wire, DevinModelBinding, DevinSettings},
    network::NetworkClients,
    provider::Provider,
    store::{ProxyMode, ProxySettingsInput, Store},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

/// Picks three loopback ports by binding and releasing them.
///
/// They are only *probably* free: between the release and the gateway's bind,
/// another test in this file or another process can take one. `Gateway::start`
/// therefore retries on fresh ports instead of trusting this once.
fn free_ports() -> (u16, u16, u16) {
    let first = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let second = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let third = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let ports = (
        first.local_addr().unwrap().port(),
        second.local_addr().unwrap().port(),
        third.local_addr().unwrap().port(),
    );
    drop(first);
    drop(second);
    drop(third);
    ports
}

/// Serializes port acquisition across the tests in this file: two tests that
/// each release three ports can otherwise be handed the same ones.
///
/// `tokio::sync::Mutex`，不是 `std::sync::Mutex`：锁在 `Gateway::start` 里跨着好几个
/// `await`（设置落库、等 `/health` 应答），标准库的守卫不能跨 await 持有。
static STARTUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const START_ATTEMPTS: usize = 5;

/// A gateway that is known to be listening on `ports`.
struct Gateway {
    ports: (u16, u16, u16),
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<cursor_server::Result<()>>,
}

impl Gateway {
    /// Starts the real gateway and waits until `/health` answers.
    ///
    /// `serve` returns immediately with `AddrInUse` when a port was taken
    /// between the probe and the bind, so every attempt takes fresh ports and a
    /// failed attempt is retried rather than reported as a product failure.
    async fn start(
        store: &Store,
        provider: Arc<dyn Provider>,
        configure: impl Fn(DevinSettings) -> DevinSettings,
    ) -> Self {
        let _guard = STARTUP_LOCK.lock().await;
        for attempt in 1..=START_ATTEMPTS {
            let ports = free_ports();
            store
                .set_devin_settings(configure(settings(ports.0, ports.1, ports.2)))
                .await
                .unwrap();
            let shutdown = CancellationToken::new();
            let gateway = DevinGateway::new(
                store.clone(),
                provider.clone(),
                NetworkClients::new(store.clone()),
            );
            let task = tokio::spawn(gateway.serve(shutdown.clone()));
            if started(ports.0, &task).await {
                return Self {
                    ports,
                    shutdown,
                    task,
                };
            }
            shutdown.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
            eprintln!("gateway start attempt {attempt} lost its ports; retrying");
        }
        panic!("the Devin gateway never started on free loopback ports");
    }

    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("the gateway must stop when its shutdown token is cancelled")
            .unwrap()
            .unwrap();
    }
}

/// Waits for the gateway to answer, and gives up early when it already exited
/// (which is what a lost port looks like).
async fn started(port: u16, task: &tokio::task::JoinHandle<cursor_server::Result<()>>) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if task.is_finished() {
            return false;
        }
        if let Ok((200, _)) = get(port, "/health").await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn settings(api_port: u16, inference_port: u16, local_api_port: u16) -> DevinSettings {
    DevinSettings {
        enabled: true,
        api_port,
        inference_port,
        local_api_port,
        bindings: vec![DevinModelBinding::new("MODEL_ISOLATION", "hash-isolation")],
        ..DevinSettings::default()
    }
}

/// A stand-in for the Cursor service listener: answers any request with 200.
async fn cursor_listener_stub() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                let _ = socket.read(&mut buffer).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                    .await;
                let _ = socket.flush().await;
            });
        }
    });
    (port, task)
}

async fn get(port: u16, path: &str) -> Result<(u16, String), reqwest::Error> {
    // A loopback probe must not be routed through whatever system proxy the
    // machine happens to run, otherwise the assertion tests the proxy.
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .timeout(Duration::from_secs(5))
        .send()
        .await?;
    let status = response.status().as_u16();
    Ok((status, response.text().await?))
}

/// Waits until the health endpoint answers. `serve` is spawned, so binding is
/// asynchronous and a bare request can race it on a slow machine.
async fn await_health(port: u16) -> (u16, String) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(response) = get(port, "/health").await {
            return response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the gateway never became reachable on port {port}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Reports whether anything still answers on the port. Rebinding is not a
/// reliable probe: a freshly closed listener can sit in TIME_WAIT and refuse
/// the rebind even though the service is gone.
async fn serving(port: u16) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match get(port, "/health").await {
            Ok(_) => return true,
            // A refused connection proves nothing is listening; only an
            // ambiguous failure is worth retrying.
            Err(error) if error.is_connect() => return false,
            Err(_) if std::time::Instant::now() >= deadline => return false,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

/// Posts one Connect-unary RPC to the gateway's shared HTTP surface.
async fn post_rpc(port: u16, method: &str, body: Vec<u8>) -> (u16, String) {
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!(
            "http://127.0.0.1:{port}/devin.v1.DevinService/{method}"
        ))
        .header("content-type", "application/proto")
        .body(body)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

/// `AssignModel` 属于模型流量,必须在本地分支回答:它不在白名单里时整条本地令牌链路
/// (`assignment.rs` 的签发 + 会话表)就是死代码,而且客户端显式调用会被送到厂商上游,
/// 与 `docs/devin-integration.md` 写的行为相反。未知 method 则一律透传上游,不再本地 404。
#[tokio::test]
async fn model_assignment_is_answered_locally_and_unknown_methods_go_upstream() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider: Arc<dyn Provider> = Arc::new(fake_provider::FakeProvider::default());
    // 上游指向本地桩:未知 method 要原样透传到这里,而不是发去真实厂商服务。
    // 出网固定走直连,否则机器上的系统代理会把环回请求也带走(测试就变成在测代理)。
    let (stub_port, _stub_task) = cursor_listener_stub().await;
    store
        .set_proxy_settings(ProxySettingsInput {
            mode: ProxyMode::Direct,
            address: String::new(),
            auth_enabled: false,
            username: String::new(),
            password: None,
        })
        .await
        .unwrap();

    let gateway = Gateway::start(&store, provider, |mut settings| {
        settings.upstream_api_url = format!("http://127.0.0.1:{stub_port}");
        settings
    })
    .await;
    let api_port = gateway.ports.0;
    assert_eq!(await_health(api_port).await.0, 200);

    // 绑定的 UID 落在协议的模型字段(21)上。
    let payload = wire::serialize_fields(&[wire::Field::bytes(21, "MODEL_ISOLATION")]).unwrap();
    let framed = wire::frame(&payload, false).unwrap();

    let (status, body) = post_rpc(api_port, "AssignModel", framed.clone()).await;
    assert_eq!(status, 200, "AssignModel 应由本地分支回答: {body}");
    assert!(
        body.contains("MODEL_ISOLATION"),
        "本地签发的令牌响应里应带模型 UID: {body}"
    );

    let (status, body) = post_rpc(api_port, "ListSessions", framed).await;
    assert_eq!(status, 200, "未知 method 应透传上游(实际 {status}): {body}");
    assert_eq!(body, "ok", "上游桩的应答要原样回来,说明请求确实转发出去了");

    gateway.stop().await;
}

#[tokio::test]
async fn a_disabled_gateway_binds_no_port_and_leaves_cursor_serving() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider: Arc<dyn Provider> = Arc::new(fake_provider::FakeProvider::default());
    let (api_port, inference_port, local_api_port) = free_ports();
    // Default settings are disabled, which is what a fresh install looks like.
    let mut configured = settings(api_port, inference_port, local_api_port);
    configured.enabled = false;
    store.set_devin_settings(configured).await.unwrap();

    let (cursor_port, _cursor_task) = cursor_listener_stub().await;
    let gateway = DevinGateway::new(store.clone(), provider, NetworkClients::new(store.clone()));
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    gateway.serve(shutdown).await.unwrap();

    assert_eq!(get(cursor_port, "/").await.unwrap().0, 200);
    for port in [api_port, inference_port, local_api_port] {
        assert!(
            !serving(port).await,
            "a disabled gateway must not answer on port {port}"
        );
    }
}

#[tokio::test]
async fn stopping_the_gateway_keeps_the_cursor_listener_serving() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider: Arc<dyn Provider> = Arc::new(fake_provider::FakeProvider::default());

    let (cursor_port, _cursor_task) = cursor_listener_stub().await;

    let gateway = Gateway::start(&store, provider, |settings| settings).await;
    let (api_port, inference_port, local_api_port) = gateway.ports;

    let (status, body) = await_health(api_port).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("\"service\":\"devin\""),
        "unexpected health body: {body}"
    );

    gateway.stop().await;

    // The Cursor listener is untouched by the gateway's exit.
    assert_eq!(get(cursor_port, "/").await.unwrap().0, 200);
    // And the gateway really stopped answering on its own ports.
    for port in [api_port, inference_port, local_api_port] {
        assert!(
            !serving(port).await,
            "a stopped gateway must not answer on port {port}"
        );
    }
}
