//! Codex 的桌面管理接口；客户端配置的写入仅发生在显式应用/撤销操作。
use crate::{
    codex::{CodexGateway, CodexSettings, CodexStatus},
    store::Store,
    Result,
};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};

#[derive(Clone)]
struct CodexControl {
    gateway: CodexGateway,
    store: Store,
}

pub fn router(gateway: CodexGateway, store: Store) -> Router {
    Router::new()
        .route("/__byok-api__/api/codex/settings", get(settings).put(save))
        .route("/__byok-api__/api/codex/status", get(status))
        .route("/__byok-api__/api/codex/apply", post(apply))
        .route("/__byok-api__/api/codex/restore", post(restore))
        .with_state(CodexControl { gateway, store })
}
async fn settings(State(state): State<CodexControl>) -> Result<Json<CodexSettings>> {
    Ok(Json(state.store.codex_settings().await?))
}
async fn save(
    State(state): State<CodexControl>,
    Json(settings): Json<CodexSettings>,
) -> Result<Json<CodexSettings>> {
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
