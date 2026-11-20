//! 官方 ChatGPT Responses 透传。
//!
//! 混合模式下，客户端把官方模型 slug 发到本机网关。这里只做三件事：校验调用方
//! 已经由 gateway 校验过、把请求原样转发到官方 Codex 后端、把响应原样流回去。
//!
//! 官方请求的语义不在本模块翻译：`use_responses_lite`、`code_mode_only`、托管工具、
//! 加密推理都由官方客户端和官方后端自己处理。任何本地桥接都只用于自有模型。

use crate::{Error, Result};
use axum::http::{HeaderMap, HeaderName};
use bytes::Bytes;
use serde_json::Value;
use std::path::Path;

/// 官方 ChatGPT 登录的 Codex 推理入口。API Key 模式不走这里。
pub(crate) const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// 本机网关用于识别官方请求的本地头；不转发给官方后端。
pub(crate) const LOCAL_HEADER: &str = "x-haxsd-gateway";

/// 读取 Codex 已有的 ChatGPT 登录凭据。只读，不刷新、不写回。
///
/// 0.171 的 1.x 自定义 provider 路径不会把 auth.json 里的 token 附加到我们伪造的
/// `OpenAI` provider 上；官方模型要工作，网关必须自己带上用户已经登录的凭据。
/// 凭据只用于官方转发，绝不进入日志或本地模型上游。
pub(crate) fn chatgpt_access_token(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join("auth.json")).ok()?;
    let auth: Value = serde_json::from_str(&text).ok()?;
    let mode = auth.get("auth_mode").and_then(Value::as_str);
    if mode.is_some_and(|mode| mode != "chatgpt" && mode != "chatgptAuthTokens") {
        return None;
    }
    auth.pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            auth.get("personal_access_token")
                .and_then(Value::as_str)
                .filter(|token| !token.trim().is_empty())
                .map(str::to_owned)
        })
}

pub(crate) fn target_url(base_url: &str) -> String {
    format!("{}/responses", base_url.trim_end_matches('/'))
}

/// 转发官方 Responses 请求。返回值保留官方状态码和响应头，正文由调用方流式转发。
pub(crate) async fn forward(
    client: &reqwest::Client,
    base_url: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<reqwest::Response> {
    let mut request = client.post(target_url(base_url));
    for (name, value) in headers {
        if should_skip_request_header(name) {
            continue;
        }
        request = request.header(name, value);
    }
    request.body(body).send().await.map_err(Error::Http)
}

fn should_skip_request_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "content-encoding"
            | "connection"
            | "transfer-encoding"
            | "keep-alive"
            | "proxy-connection"
            | "upgrade"
            | LOCAL_HEADER
    )
}

/// 官方响应头里不能直接透传的逐跳字段。正文已由 reqwest 解压，编码/长度头也不再成立。
pub(crate) fn should_skip_response_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "content-length"
            | "content-encoding"
            | "connection"
            | "transfer-encoding"
            | "keep-alive"
            | "proxy-connection"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_url_appends_one_responses_segment() {
        assert_eq!(
            target_url("https://chatgpt.com/backend-api/codex/"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            target_url("http://127.0.0.1:1234/v1"),
            "http://127.0.0.1:1234/v1/responses"
        );
    }

    #[test]
    fn the_local_gateway_header_never_reaches_the_official_backend() {
        assert!(should_skip_request_header(&HeaderName::from_static(
            LOCAL_HEADER
        )));
        assert!(!should_skip_request_header(&HeaderName::from_static(
            "authorization"
        )));
        assert!(!should_skip_request_header(&HeaderName::from_static(
            "chatgpt-account-id"
        )));
    }
}
