//! Selects local handling or the configured official Cursor upstream.
//!
//! 官方转发的出网与 BYOK provider 路径共用 [`crate::upstream::attempt`] 的发送语义：
//! 连接类失败按当前配置重建客户端（必要时绕开已经没人监听的系统代理直连）并重试，
//! 但只在还没拿到任何响应内容之前重试。请求体因此先整段读进内存——重发需要一个
//! 可重放的副本，而流只能被消费一次。
use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    extract::Extension,
    http::{header, HeaderMap, HeaderValue, Method, Request, Response},
};
use tokio_util::sync::CancellationToken;

use crate::{
    network::{NetworkClients, OutboundClient},
    upstream::{self, Attempt, UpstreamCall},
    Error, Result,
};

const CURSOR_UPSTREAM: &str = "https://api2.cursor.sh";
pub const UPSTREAM_URL_HEADER: &str = "x-server-upstream-url";

#[derive(Clone)]
pub struct CursorProxy {
    clients: NetworkClients,
    upstream: String,
}

pub struct BufferedResponse {
    pub status: axum::http::StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Bytes,
}

impl BufferedResponse {
    pub fn into_response(self) -> Response<Body> {
        let body = self.body.clone();
        self.with_body(body)
    }

    pub fn with_body(mut self, body: Bytes) -> Response<Body> {
        self.headers.insert(
            header::CONTENT_LENGTH,
            body.len()
                .to_string()
                .parse()
                .expect("body length is always a valid header value"),
        );
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = self.status;
        *response.headers_mut() = self.headers;
        response
    }
}

impl CursorProxy {
    pub fn cursor(clients: NetworkClients) -> Self {
        Self {
            clients,
            upstream: CURSOR_UPSTREAM.into(),
        }
    }
}

/// 官方转发的一次出网：与 BYOK 路径同一套发送语义。
///
/// 连接类失败时 [`OutboundClient::rebuild_after`] 会按当前配置重建客户端，配置没变
/// 却是连接失败就绕开系统代理直连；重建后的客户端交给 `attempt`，按 provider 路径
/// 同一份预算退避重试。重试日志、次数上限与退避时长都来自那一份实现，所以这条路径
/// 不会刷屏。
async fn send_upstream(
    proxy: &CursorProxy,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Result<reqwest::Response> {
    let outbound = OutboundClient::cursor(proxy.clients.clone()).await?;
    // 这条路径没有自己的取消源：句柄被丢弃时整个 future 一起结束，重试不会继续跑。
    let cancellation = CancellationToken::new();
    let outcome = upstream::attempt(
        &UpstreamCall {
            label: "Cursor",
            // 官方转发没有模型维度：日志靠 url 与 label 定位这次请求。
            model: "",
            url,
        },
        &outbound,
        |client| {
            client
                .request(method.clone(), url)
                .headers(headers.clone())
                .body(body.clone())
        },
        &cancellation,
        None,
    )
    .await?;
    match outcome {
        Attempt::Response(response) | Attempt::Rejected { response, .. } => Ok(response),
        Attempt::Failed { error, retries } => Err(upstream::send_failure("Cursor", error, retries)),
        Attempt::Cancelled => Err(Error::Cancelled),
    }
}

pub async fn forward(
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    forward_request(&proxy, request, None, crate::api::cursor::MAX_REQUEST_BODY).await
}

pub(crate) async fn forward_to_service(
    proxy: &CursorProxy,
    request: Request<Body>,
    service_url: &str,
) -> Result<Response<Body>> {
    forward_request(
        proxy,
        request,
        Some(service_url),
        crate::api::cursor::MAX_REQUEST_BODY,
    )
    .await
}

/// 转发失败返回 `Err`(交给 axum 渲染);**超限返回 `Ok(413)`**——那是一个完整的响应,
/// 不是转发失败,调用方不该把它再翻成 400/500。
async fn forward_request(
    proxy: &CursorProxy,
    request: Request<Body>,
    service_url: Option<&str>,
    body_limit: usize,
) -> Result<Response<Body>> {
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str())
        .to_owned();
    let url = match service_url {
        Some(service_url) => format!("{}{}", service_url.trim_end_matches('/'), path),
        None => upstream_url(&parts.headers, &proxy.upstream, &path)?,
    };

    let mut headers = parts.headers;
    headers.remove(UPSTREAM_URL_HEADER);
    headers.remove(header::HOST);
    remove_hop_by_hop_headers(&mut headers);

    // 请求体先整段读成字节：出网失败后的重试要重发同一个请求，而流只能被消费一次。
    // 上限一到就返回 413，而且不再继续读（见 `read_request_body`）。
    let body = match crate::api::cursor::read_request_body(body, body_limit).await {
        Ok(body) => body,
        Err(_) => return Ok(crate::api::cursor::body_too_large_response(body_limit)),
    };
    let upstream = match send_upstream(proxy, parts.method.clone(), &url, headers, body).await {
        Ok(upstream) => upstream,
        Err(error) => {
            tracing::error!(
                method = %parts.method,
                path,
                elapsed_ms = started.elapsed().as_millis(),
                error = %crate::diagnostics::error_chain(&error),
                "Cursor upstream request failed"
            );
            return Err(error);
        }
    };

    let status = upstream.status();
    let mut response_headers = upstream.headers().clone();
    remove_hop_by_hop_headers(&mut response_headers);
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;

    tracing::info!(
        method = %parts.method,
        path,
        %status,
        elapsed_ms = started.elapsed().as_millis(),
        "forwarded Cursor backend request"
    );
    Ok(response)
}

pub async fn forward_buffered(
    proxy: &CursorProxy,
    request: Request<Body>,
) -> Result<BufferedResponse> {
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str());
    let url = upstream_url(&parts.headers, &proxy.upstream, path)?;
    let mut headers = parts.headers;
    headers.remove(UPSTREAM_URL_HEADER);
    headers.remove(header::HOST);
    remove_hop_by_hop_headers(&mut headers);
    headers.insert(
        "connect-accept-encoding",
        axum::http::HeaderValue::from_static("identity"),
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        axum::http::HeaderValue::from_static("identity"),
    );
    let body =
        match crate::api::cursor::read_request_body(body, crate::api::cursor::MAX_REQUEST_BODY)
            .await
        {
            Ok(body) => body,
            // 缓冲转发没有自己的响应通道，所以这里把 413 装成 `BufferedResponse` 交回去；
            // 调用方照常 `with_body`/`into_response`，状态码与文案与直通路径一致。
            Err(status) => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                );
                return Ok(BufferedResponse {
                    status,
                    headers,
                    body: Bytes::from(crate::api::cursor::body_too_large_body(
                        crate::api::cursor::MAX_REQUEST_BODY,
                    )),
                });
            }
        };
    let upstream = send_upstream(proxy, parts.method, &url, headers, body).await?;
    let status = upstream.status();
    let mut headers = upstream.headers().clone();
    remove_hop_by_hop_headers(&mut headers);
    let body = upstream.bytes().await?;
    Ok(BufferedResponse {
        status,
        headers,
        body,
    })
}

fn upstream_url(headers: &axum::http::HeaderMap, fallback: &str, path: &str) -> Result<String> {
    let Some(value) = headers.get(UPSTREAM_URL_HEADER) else {
        return Ok(format!("{fallback}{path}"));
    };
    let value = value
        .to_str()
        .map_err(|error| crate::Error::Protocol(format!("invalid upstream URL header: {error}")))?;
    let url = reqwest::Url::parse(value)
        .map_err(|error| crate::Error::Protocol(format!("invalid upstream URL: {error}")))?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https" || !crate::local_app::proxy_host_allowed(host) {
        return Err(crate::Error::Protocol(
            "upstream URL must target a Cursor HTTPS host".into(),
        ));
    }
    Ok(url.into())
}

fn remove_hop_by_hop_headers(headers: &mut axum::http::HeaderMap) {
    let connection_headers = headers
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;
    use crate::store::Store;

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

    fn post(body: Vec<u8>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/aiserver.v1.AiService/BidiAppend")
            .body(Body::from(body))
            .unwrap()
    }

    /// 请求体超过上限：立刻 413，而且**一个字节都不往上游发**。
    ///
    /// 上游地址故意指向没人监听的端口：真发了就是连接失败（500/502），状态码会把两种
    /// 结局区分开。上限用小值注入，不必真的分配 64 MiB。
    #[tokio::test]
    async fn a_body_over_the_limit_is_answered_with_413_before_any_upstream_contact() {
        let (_directory, store) = test_store().await;
        let proxy = CursorProxy::cursor(NetworkClients::new(store));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let response = forward_request(&proxy, post(vec![b'x'; 128]), Some(&dead), 32)
            .await
            .expect("超限是一个完整的响应，不是转发失败");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("32 byte limit"),
            "文案要说清上限是多少: {body:?}"
        );
    }
}
