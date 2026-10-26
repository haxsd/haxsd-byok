//! Direct mode must not inherit the machine proxy for any outbound role.
//!
//! The environment proxy is injected into this test process only; it never reads or
//! writes the user's Windows proxy settings.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use cursor_server::{
    network::NetworkClients,
    store::{ProxyMode, ProxySettingsInput, Store},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn test_store() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::connect(&format!(
        "sqlite://{}",
        directory.path().join("test.db").display()
    ))
    .await
    .unwrap();
    (directory, store)
}

async fn store_with_mode(mode: ProxyMode) -> (tempfile::TempDir, Store) {
    let (directory, store) = test_store().await;
    store
        .set_proxy_settings(ProxySettingsInput {
            mode,
            address: String::new(),
            auth_enabled: false,
            username: String::new(),
            password: None,
        })
        .await
        .unwrap();
    (directory, store)
}

async fn http_stub(body: &'static str) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            let mut request = [0_u8; 2048];
            let _ = socket.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (address, connections)
}

#[tokio::test]
async fn direct_mode_bypasses_the_system_proxy_for_every_role_and_default_still_uses_it() {
    let (proxy, proxy_connections) = http_stub("proxied").await;
    std::env::set_var("HTTP_PROXY", format!("http://{proxy}"));
    std::env::set_var("HTTPS_PROXY", format!("http://{proxy}"));
    std::env::set_var("NO_PROXY", "example.invalid");

    let (_directory, store) = store_with_mode(ProxyMode::Direct).await;
    let clients = NetworkClients::new(store);
    let (upstream, upstream_connections) = http_stub("direct").await;
    let url = format!("http://{upstream}/v1/models");

    for (role, client) in [
        ("default", clients.default_client().await.unwrap()),
        (
            "provider",
            clients
                .provider_client(std::time::Duration::from_secs(5))
                .await
                .unwrap(),
        ),
        ("cursor", clients.cursor_client().await.unwrap()),
        ("devin", clients.devin_client().await.unwrap()),
    ] {
        let response = client.get(&url).send().await.unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::OK, "{role} failed: {body}");
        assert_eq!(body, "direct", "{role} was routed through the system proxy");
    }

    assert_eq!(upstream_connections.load(Ordering::SeqCst), 4);
    assert_eq!(
        proxy_connections.load(Ordering::SeqCst),
        0,
        "direct mode must not contact the system proxy"
    );

    let (_default_directory, default_store) = store_with_mode(ProxyMode::Default).await;
    let default_clients = NetworkClients::new(default_store);
    let response = default_clients
        .default_client()
        .await
        .unwrap()
        .get(&url)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "proxied");
    assert_eq!(
        proxy_connections.load(Ordering::SeqCst),
        1,
        "default mode must still use the system proxy"
    );
    assert_eq!(
        upstream_connections.load(Ordering::SeqCst),
        4,
        "the default-mode request must not reach the upstream directly"
    );
}
