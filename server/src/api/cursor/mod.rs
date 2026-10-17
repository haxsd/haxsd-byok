//! Wires the Cursor-facing API routes.

use axum::{
    body::{Body, Bytes},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;

pub mod bidi;
/// 本机 Cursor 入口的调用方校验。
mod entry;
mod handlers;
pub mod proxy;
mod run_sse;

pub use handlers::router;

/// Cursor 侧入口的请求体上限。手写的 `to_bytes` 不受 `DefaultBodyLimit` 保护,所以真正
/// 卡住它的是这个常量,两处读 body 必须用同一个值。请求体先整段读进内存是既有取舍
/// (出网失败后要重发),更要有上界:解压层会丢掉 `content-length`,一段小 gzip 足以放大成
/// 几 GB,把整个进程拖垮。
pub(crate) const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

/// 读请求体:一到 `limit` 就返回 413,并且**立刻停止读**——剩下的正文不再进内存。
///
/// 这里不用 `axum::body::to_bytes`:它把"超过上限"与"这次传输坏了"混成同一个错误,而两者
/// 对客户端的意义不同(413 是请求本身太大,400 是传输失败)。`DefaultBodyLimit` 又只对
/// `Bytes`/`Json` 这类 extractor 生效,本模块是手写 `Request<Body>`,所以上限只能自己判。
pub(crate) async fn read_request_body(body: Body, limit: usize) -> Result<Bytes, StatusCode> {
    let mut stream = body.into_data_stream();
    let mut collected = bytes::BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
        if collected.len() + chunk.len() > limit {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected.freeze())
}

/// 超限响应的正文:把上限写进去,日志与客户端都能直接看到是多少。
pub(crate) fn body_too_large_body(limit: usize) -> String {
    format!("request body is larger than the {limit} byte limit")
}

/// 超限响应。[`crate::api::cursor::proxy::forward_buffered`] 那条路径要自己拼结构体,
/// 所以正文文案与状态码分成两个函数,避免两处各写一份再慢慢分叉。
pub(crate) fn body_too_large_response(limit: usize) -> Response<Body> {
    (StatusCode::PAYLOAD_TOO_LARGE, body_too_large_body(limit)).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;

    /// 一个永远不会结束的请求体:每拉一次就多 16 字节,并记账"被拉走了几块"。
    fn endless_body(pulled: Arc<AtomicUsize>) -> Body {
        Body::from_stream(futures_util::stream::repeat_with(move || {
            pulled.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from_static(&[b'x'; 16]))
        }))
    }

    /// 上限本身是 64 MiB，不是 `usize::MAX`：这条审计修的就是"三处读 body 都没有上界"，
    /// 所以把常量改回无上限必须有用例拦下。
    #[test]
    fn the_request_body_limit_stays_at_64_mib() {
        assert_eq!(MAX_REQUEST_BODY, 64 * 1024 * 1024);
    }

    /// 上限之内的正文原样收下(边界包含:正好等于上限也要通过)。
    #[tokio::test]
    async fn a_request_body_within_the_limit_is_read_whole() {
        let body = Body::from("0123456789");

        let collected = read_request_body(body, 10).await.unwrap();

        assert_eq!(collected, Bytes::from_static(b"0123456789"));
    }

    /// 超限:413,而且停在第一个越界的块上——不把剩下的正文读进内存。
    #[tokio::test]
    async fn a_request_body_over_the_limit_is_rejected_without_reading_the_rest() {
        let pulled = Arc::new(AtomicUsize::new(0));

        let status = read_request_body(endless_body(pulled.clone()), 32)
            .await
            .unwrap_err();

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            pulled.load(Ordering::SeqCst),
            3,
            "32 字节上限:两块刚好到界,第三块一到就该停手(多拉的每一块都是白读的内存)"
        );
    }

    /// 传输本身坏了(不是超限)仍然是 400:两件事不能混成一个状态码。
    #[tokio::test]
    async fn a_broken_body_is_a_bad_request_not_a_size_problem() {
        let broken = futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("cut"))]);

        let status = read_request_body(Body::from_stream(broken), 64)
            .await
            .unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
