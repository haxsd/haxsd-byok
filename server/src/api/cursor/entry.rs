//! 本机 Cursor 入口的调用方校验。
//!
//! 接管期间 Cursor 的 `http.proxy` 指着本机代理，代理把"归我们管"的请求转发到本进程的
//! Cursor 路由（转发前打上 [`UPSTREAM_URL_HEADER`] 标记原始地址，见
//! `local_app::proxy::CursorRelay`）。这些请求必须能说清自己是谁：
//!
//! - 本地模式：客户端带着我们注入的本地账号 token（`cursorAuth/accessToken`，由
//!   `local_app::account::inject_if_missing` 写进 `state.vscdb`）；
//! - 混合模式：客户端带着用户自己的账号 token（Cursor 自己签发，本地无法验证，只确认带着凭证）。
//!
//! 判据本体在 `local_app::local_entry_caller`。这里只做"打了代理标记却没有身份 → 401"：代理
//! 那道是第一道防线（就地拒绝，不进后端），这一道保证以后新增入口也走同一条判据，而
//! **不带标记的请求**（Docker/`wait-on` 的健康探测、web 缓存目录、协议级集成测试直接打的
//! 路由）不受影响——它们没有从本机代理进来。

use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::api::cursor::proxy::UPSTREAM_URL_HEADER;

/// 经本地代理进来的请求必须出示调用方身份。
pub(super) async fn require_local_cursor_caller(request: Request, next: Next) -> Response {
    if proxied_entry(&request) && crate::local_app::local_entry_caller(request.headers()).is_none()
    {
        tracing::warn!(
            path = request.uri().path(),
            "rejected a proxied Cursor request without a caller identity"
        );
        return (
            StatusCode::UNAUTHORIZED,
            "the local BYOK gateway only serves the Cursor client; this request carries no caller \
             identity\n",
        )
            .into_response();
    }
    next.run(request).await
}

/// 是不是从本机代理进来的：本地代理转发前会写下原始地址，见 `CursorRelay::route`。
fn proxied_entry(request: &Request) -> bool {
    request.headers().contains_key(UPSTREAM_URL_HEADER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, routing::post, Router};
    use tower::ServiceExt as _;

    fn app() -> Router {
        Router::new()
            .route(
                "/aiserver.v1.AiService/AvailableModels",
                post(|| async { "ok" }),
            )
            .layer(axum::middleware::from_fn(require_local_cursor_caller))
    }

    fn request(authorization: Option<&str>, proxied: bool) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/aiserver.v1.AiService/AvailableModels");
        if let Some(value) = authorization {
            builder = builder.header(axum::http::header::AUTHORIZATION, value);
        }
        if proxied {
            builder = builder.header(
                UPSTREAM_URL_HEADER,
                "https://api2.cursor.sh/aiserver.v1.AiService/AvailableModels",
            );
        }
        builder.body(Body::empty()).unwrap()
    }

    /// 经代理进来却没有身份的请求被拒；带本地账号 token 或客户端自己的账号都放行。
    #[tokio::test]
    async fn a_proxied_request_must_carry_a_caller_identity() {
        let anonymous = app().oneshot(request(None, true)).await.unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        for authorization in [
            crate::local_app::local_cursor_authorization(),
            "Bearer official-cursor-account-token".into(),
        ] {
            let accepted = app()
                .oneshot(request(Some(&authorization), true))
                .await
                .unwrap();
            assert_eq!(accepted.status(), StatusCode::OK, "{authorization}");
        }
    }

    /// 不是从代理进来的请求不受影响：健康探测、web 缓存目录与协议级集成测试直接打这些路由，
    /// 它们本来就不带凭证。
    #[tokio::test]
    async fn a_direct_request_is_left_alone() {
        let response = app().oneshot(request(None, false)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
