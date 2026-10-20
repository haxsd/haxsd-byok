//! Codex 的桌面管理接口；客户端配置的写入仅发生在显式应用/撤销操作。
use crate::{
    codex::{CodexGateway, CodexSettings, CodexStatus},
    store::Store,
    Result,
};
use axum::{
    extract::State,
    middleware,
    routing::{get, post},
    Json, Router,
};

use super::{desktop_cors, reject_cross_site};

#[derive(Clone)]
struct CodexControl {
    gateway: CodexGateway,
    store: Store,
}

/// Codex 管理接口。
///
/// 这里的 apply/restore 会改写 `~/.codex/config.toml`，是有副作用的写操作——管理面的来源
/// 准入必须和 `api_router` 一样套在最外层：CORS 只在响应上把关，跨站页面发一个简单请求
/// 时副作用照样发生（审计 11/P2-1 点名的就是这两个端点没接准入）。
pub fn router(gateway: CodexGateway, store: Store) -> Router {
    Router::new()
        .route("/__byok-api__/api/codex/settings", get(settings).put(save))
        .route("/__byok-api__/api/codex/status", get(status))
        .route("/__byok-api__/api/codex/apply", post(apply))
        .route("/__byok-api__/api/codex/restore", post(restore))
        .with_state(CodexControl { gateway, store })
        .layer(desktop_cors())
        .layer(middleware::from_fn(reject_cross_site))
}
async fn settings(State(state): State<CodexControl>) -> Result<Json<CodexSettings>> {
    Ok(Json(state.store.codex_settings().await?))
}
async fn save(
    State(state): State<CodexControl>,
    Json(settings): Json<CodexSettings>,
) -> Result<Json<CodexSettings>> {
    // 端口准入在写库之前：被自己或别的程序占着时，宁可拒绝保存，也不要留下
    // "已保存、未监听、重启也不管用"的状态。
    super::ports::ensure_codex_port_usable(
        &state.store,
        &settings,
        state.gateway.listening_port().await,
    )
    .await?;
    Ok(Json(state.gateway.save_settings(settings).await?))
}
async fn status(State(state): State<CodexControl>) -> Result<Json<CodexStatus>> {
    Ok(Json(state.gateway.status().await?))
}
async fn apply(State(state): State<CodexControl>) -> Result<Json<CodexStatus>> {
    Ok(Json(state.gateway.apply().await?))
}
async fn restore(State(state): State<CodexControl>) -> Result<Json<CodexStatus>> {
    Ok(Json(state.gateway.restore().await?))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{header, Method, Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::*;

    async fn fixture() -> Router {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("codex-control.db").display()
        ))
        .await
        .unwrap();
        let clients = crate::network::NetworkClients::new(store.clone());
        router(CodexGateway::new(store.clone(), clients), store)
    }

    /// 审计 11/P2-1：codex 的写端点（apply/restore 会改 `~/.codex/config.toml`）也必须过
    /// 来源准入——CORS 只在响应上把关，跨站页面的简单请求照样会触发副作用。
    #[tokio::test]
    async fn codex_writes_refuse_cross_site_callers() {
        let app = fixture().await;
        for (path, name, value) in [
            (
                "/__byok-api__/api/codex/apply",
                header::ORIGIN.as_str(),
                "https://evil.example",
            ),
            (
                "/__byok-api__/api/codex/apply",
                "sec-fetch-site",
                "cross-site",
            ),
            (
                "/__byok-api__/api/codex/restore",
                header::ORIGIN.as_str(),
                "null",
            ),
            (
                "/__byok-api__/api/codex/restore",
                header::ORIGIN.as_str(),
                "http://192.168.1.10",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .header(name, value)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{path} {name}: {value}"
            );
        }
    }

    /// 本机页面与本机原生调用照常放行。只测读端点：写端点会真的改 `~/.codex`。
    #[tokio::test]
    async fn codex_reads_stay_open_to_local_callers() {
        let app = fixture().await;
        for origin in [
            None,
            Some("http://127.0.0.1:19387"),
            Some("tauri://localhost"),
        ] {
            let mut request = Request::builder()
                .method(Method::GET)
                .uri("/__byok-api__/api/codex/settings");
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{origin:?}");
        }
    }
}
