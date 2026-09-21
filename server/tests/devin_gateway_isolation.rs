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
    devin::{gateway::DevinGateway, DevinModelBinding, DevinSettings},
    provider::Provider,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

/// Reserves three free loopback ports for the gateway under test.
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
    let gateway = DevinGateway::new(store.clone(), provider);
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
    let (api_port, inference_port, local_api_port) = free_ports();
    store
        .set_devin_settings(settings(api_port, inference_port, local_api_port))
        .await
        .unwrap();

    let (cursor_port, _cursor_task) = cursor_listener_stub().await;

    let shutdown = CancellationToken::new();
    let gateway = DevinGateway::new(store.clone(), provider);
    let gateway_task = tokio::spawn(gateway.serve(shutdown.clone()));

    let (status, body) = await_health(api_port).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("\"service\":\"devin\""),
        "unexpected health body: {body}"
    );

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), gateway_task)
        .await
        .expect("the gateway must stop when its shutdown token is cancelled")
        .unwrap()
        .unwrap();

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
