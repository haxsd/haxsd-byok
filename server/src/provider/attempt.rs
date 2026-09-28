//! Sends one Provider HTTP attempt, with a single recovery retry when the
//! outbound route turned out to be stale.

use tokio_util::sync::CancellationToken;

use crate::{network::OutboundClient, Error, Result};

use super::CallRecorder;

#[derive(Debug)]
pub(crate) enum Attempt {
    Response(reqwest::Response),
    Cancelled,
}

pub(crate) async fn send_once<F>(
    label: &str,
    outbound: &OutboundClient,
    build: F,
    cancellation: &CancellationToken,
    recorder: Option<&CallRecorder>,
) -> Result<Attempt>
where
    F: Fn(&reqwest::Client) -> reqwest::RequestBuilder + Send,
{
    let mut client = outbound.client().clone();
    // 只兜底一次：第二次失败即便配置又变了也直接上报，避免来回重发。
    let mut recovery_allowed = true;
    let response = loop {
        let attempt = tokio::select! {
            _ = cancellation.cancelled() => return Ok(Attempt::Cancelled),
            attempt = build(&client).send() => attempt,
        };
        match attempt {
            Ok(response) => break response,
            Err(error) => {
                let rebuilt = if recovery_allowed {
                    recovery_allowed = false;
                    outbound.rebuild_after(&error).await
                } else {
                    None
                };
                let Some(rebuilt) = rebuilt else {
                    return Err(error.into());
                };
                // 关掉代理之后最常见的一类失败：请求还发往一个已经没人监听的
                // 代理端口。换一个按当前配置重建的客户端再试一次，而不是把用户
                // 这一轮对话直接判死。
                tracing::warn!(
                    label,
                    %error,
                    "outbound connection failed; retrying once with a client rebuilt from the current settings"
                );
                client = rebuilt;
            }
        }
    };
    if let Some(recorder) = recorder {
        recorder
            .response_headers(response.status().as_u16())
            .await?;
    }
    if response.status().is_success() {
        return Ok(Attempt::Response(response));
    }
    let status = response.status();
    let bytes = tokio::select! {
        _ = cancellation.cancelled() => return Ok(Attempt::Cancelled),
        bytes = response.bytes() => bytes,
    }?;
    Err(Error::Provider(format!(
        "{label} {status}: {}",
        String::from_utf8_lossy(&bytes)
    )))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::{
        network::NetworkClients,
        store::{ProxyMode, ProxySettingsInput, Store},
    };

    async fn server(response: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket.write_all(response).await.unwrap();
        });
        format!("http://{address}")
    }

    async fn test_store() -> Store {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        Store::connect(&url).await.unwrap()
    }

    /// 绑定一个端口再放掉：地址确定，但上面肯定没人监听。
    async fn dead_address() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        address.to_string()
    }

    async fn set_proxy(store: &Store, mode: ProxyMode, address: &str) {
        store
            .set_proxy_settings(ProxySettingsInput {
                mode,
                address: address.into(),
                auth_enabled: false,
                username: String::new(),
                password: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn non_success_status_is_one_failed_attempt() {
        let url =
            server(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\n\r\ndown").await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let outbound = OutboundClient::fixed(client);
        let error = send_once(
            "test",
            &outbound,
            |client| client.get(&url),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, Error::Provider(message) if message.contains("503") && message.contains("down")),
            "unexpected error: {error:?}"
        );
    }

    #[tokio::test]
    async fn response_body_transport_failure_is_one_failed_attempt() {
        let url =
            server(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 100\r\n\r\nshort").await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let outbound = OutboundClient::fixed(client);
        let error = send_once(
            "test",
            &outbound,
            |client| client.get(&url),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, Error::Http(_)),
            "unexpected error: {error:?}"
        );
    }

    #[tokio::test]
    async fn request_transport_failure_is_one_failed_attempt() {
        let url = format!("http://{}/x", dead_address().await);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let outbound = OutboundClient::fixed(client);
        let error = send_once(
            "test",
            &outbound,
            |client| client.get(&url),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, Error::Http(_)),
            "unexpected error: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_connection_failure_retries_once_with_a_rebuilt_client() {
        let store = test_store().await;
        set_proxy(
            &store,
            ProxyMode::Custom,
            &format!("http://{}", dead_address().await),
        )
        .await;
        let clients = NetworkClients::new(store.clone());
        let outbound = OutboundClient::provider(clients.clone(), Duration::from_secs(5))
            .await
            .unwrap();

        // 缓存建好之后把代理换成一个能用的地址，等价于运行期间关掉 Clash：
        // 路由变了，缓存里的客户端还指着旧地址。
        let proxy =
            server(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
        set_proxy(&store, ProxyMode::Custom, &proxy).await;

        let attempt = send_once(
            "test",
            &outbound,
            |client| client.get("http://provider.invalid/v1/models"),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

        assert!(
            matches!(attempt, Attempt::Response(response) if response.status().is_success()),
            "the rebuilt client must carry the retried request"
        );
        assert!(
            !clients.outbound_route_changed().await,
            "the cache must now match the current settings"
        );
    }

    #[tokio::test]
    async fn a_connection_failure_without_a_route_change_stays_one_attempt() {
        let store = test_store().await;
        set_proxy(&store, ProxyMode::Direct, "").await;
        let clients = NetworkClients::new(store);
        let outbound = OutboundClient::provider(clients, Duration::from_secs(5))
            .await
            .unwrap();
        let url = format!("http://{}/x", dead_address().await);

        let error = send_once(
            "test",
            &outbound,
            |client| client.get(&url),
            &CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, Error::Http(_)),
            "a route that did not change must not be retried: {error:?}"
        );
    }
}
