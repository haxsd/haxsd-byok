//! Verifies the Cursor official forwarding path self-heals when the system proxy dies
//! while the configuration still points at it (Clash updates restart the core; the
//! environment variables and the Windows registry keep advertising the dead port).
//!
//! 这个文件只放一个用例：兜底窗口是进程级状态，且 `tracing` 的 callsite 兴趣按首次
//! 求值缓存——同一个进程里别的用例先在没有订阅者的线程上触发同一行日志，这一行就
//! 不会再进捕获。
#[path = "support/cursor_official.rs"]
mod cursor_official;
#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use axum::{body::to_bytes, http::StatusCode};
use cursor_official::{
    capture_logs, dead_address, post_tab, set_proxy, set_tab_service, tab_router, CapturedLogs,
    Stub,
};
use cursor_server::{network::NetworkClients, store::ProxyMode};

/// 系统代理这一跳断了：官方转发首次失败后自己绕开它直连并成功，而不是一路失败到
/// 用户重启代理或重启本应用；这次自愈只留一条 warn，重试也只留一条。
///
/// 默认模式读的就是这些环境变量（reqwest 的判定顺序：先环境变量，再 Windows 注册表），
/// 所以测试不用碰用户的注册表。
#[tokio::test]
async fn an_unreachable_system_proxy_is_bypassed_after_the_first_failure() {
    let dead = dead_address().await;
    std::env::set_var("HTTP_PROXY", format!("http://{dead}"));
    std::env::set_var("HTTPS_PROXY", format!("http://{dead}"));
    std::env::set_var("NO_PROXY", "example.invalid");

    let logs = CapturedLogs::default();
    let _captured = capture_logs(&logs);

    let (_directory, store) = fixtures::temp_store().await;
    set_proxy(&store, ProxyMode::Default, "").await;
    let stub = Stub::start().await;
    set_tab_service(&store, &stub.origin).await;
    let clients = NetworkClients::new(store.clone());
    let router = tab_router(store, clients).await;

    let response = post_tab(router, "hello").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(body.as_ref(), b"ok");
    assert_eq!(stub.attempts(), 1, "直连兜底只发一次请求");
    assert!(
        stub.request_line(0)
            .starts_with("POST /aiserver.v1.AiService/StreamCpp"),
        "直连时请求行是原始路径: {}",
        stub.request_line(0)
    );
    assert!(
        cursor_server::network::system_proxy_bypassed(),
        "失败之后必须记下「系统代理连不上」"
    );
    let text = logs.text();
    assert_eq!(
        text.matches("system proxy is unreachable").count(),
        1,
        "兜底只判一次: {text}"
    );
    assert_eq!(
        text.matches("upstream request failed before any response; retrying")
            .count(),
        1,
        "重试只记一条 warn: {text}"
    );
}
