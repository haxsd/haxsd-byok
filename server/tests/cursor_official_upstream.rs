//! Verifies the Cursor official forwarding path recovers from outbound connection
//! failures the same way the BYOK provider path does.
//!
//! 「代理进程崩了、配置还在」的现场：官方转发的客户端把"用哪个代理"钉在构建那一刻，
//! 出网失败时如果不重建客户端、不绕开已经没人监听的代理，用户的每一次对话都会以
//! 502 结束，直到他重启代理或重启本应用。
#[path = "support/cursor_official.rs"]
mod cursor_official;
#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::time::{Duration, Instant};

use axum::{body::to_bytes, http::StatusCode};
use cursor_official::{dead_address, post_tab, set_proxy, set_tab_service, tab_router, Stub};
use cursor_server::{network::NetworkClients, store::ProxyMode};

/// 出网配置中途换掉（用户关掉代理又换了一个能用的）：官方转发在第一次连接失败后
/// 按当前配置重建客户端，同一个请求重试一次就成功，而不是把这一轮判死。
#[tokio::test]
async fn a_changed_outbound_route_is_retried_with_the_rebuilt_client() {
    let (_directory, store) = fixtures::temp_store().await;
    let dead = dead_address().await;
    set_proxy(&store, ProxyMode::Custom, &format!("http://{dead}")).await;
    // 目标地址不由本机解析：请求必须经过代理，测试才能确定失败来自代理那一跳。
    set_tab_service(&store, "http://cursor-upstream.invalid").await;
    let clients = NetworkClients::new(store.clone());
    // 先把指向死端口的客户端缓存起来，等价于"应用启动时代理还活着"。
    clients.cursor_client().await.unwrap();
    let router = tab_router(store.clone(), clients).await;

    let stub = Stub::start().await;
    set_proxy(&store, ProxyMode::Custom, &stub.origin).await;

    let response = post_tab(router, "hello").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(body.as_ref(), b"ok");
    assert_eq!(stub.attempts(), 1, "重建后的客户端只连一次新代理");
    assert!(
        stub.request_line(0)
            .contains("http://cursor-upstream.invalid/aiserver.v1.AiService/StreamCpp"),
        "经代理转发时请求行是绝对地址: {}",
        stub.request_line(0)
    );
}

/// 配置真的是坏的（自定义代理地址指向没人监听的端口）：重试有上限，错误里写清
/// 重试次数与真实原因，而不是空转到超时。
#[tokio::test]
async fn a_broken_outbound_proxy_reports_a_readable_error_after_bounded_retries() {
    let (_directory, store) = fixtures::temp_store().await;
    let dead = dead_address().await;
    set_proxy(&store, ProxyMode::Custom, &format!("http://{dead}")).await;
    set_tab_service(&store, "http://cursor-official.invalid").await;
    let router = tab_router(store.clone(), NetworkClients::new(store)).await;

    let started = Instant::now();
    let response = post_tab(router, "hello").await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("Cursor upstream request failed (after 2 retries)"),
        "错误要写清路径与重试次数: {text}"
    );
    assert!(
        text.contains("cursor-official.invalid"),
        "错误里要有失败的地址: {text}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "重试必须有限: {:?}",
        started.elapsed()
    );
}
