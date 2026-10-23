//! 入站 Anthropic Messages 网关。
//!
//! Claude Code 是这里的客户端：它把对话与工具定义发到本机回环端口，工具调用仍由
//! 它自己执行，网关只负责把请求投影成共享的模型调用、再把共享事件流写回 Anthropic
//! 的 SSE 协议。出站协议（Anthropic / OpenAI Chat / OpenAI Responses / 插件）由
//! `crate::provider` 决定，本模块不复制任何一家的请求构造或 SSE 解析。
pub mod gateway;
pub mod messages;
pub mod models;

pub use gateway::ClaudeGateway;

use axum::{
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// 网关默认端口。与 Codex（43120）相邻，避免两者默认撞在一起。
pub const DEFAULT_PORT: u16 = 43_121;

/// Claude Code 网关设置。
///
/// 模型字段存的是模型库里的 `model_hash`（稳定身份），不是显示名：改名或轮换凭据
/// 之后引用继续有效。别名缺省时回落到默认模型——Claude Code 会发送它自带的
/// `claude-sonnet-4-5-…` 这类 ID，网关按家族名解析。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClaudeSettings {
    pub enabled: bool,
    pub port: u16,
    pub default_model_hash: Option<String>,
    pub sonnet_model_hash: Option<String>,
    pub opus_model_hash: Option<String>,
    pub haiku_model_hash: Option<String>,
}

impl Default for ClaudeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_PORT,
            default_model_hash: None,
            sonnet_model_hash: None,
            opus_model_hash: None,
            haiku_model_hash: None,
        }
    }
}

impl ClaudeSettings {
    pub fn validate(&self) -> Result<()> {
        if self.port == 0 {
            return Err(Error::Config("Claude port must be non-zero".into()));
        }
        for (label, id) in self.model_references() {
            if id.trim().is_empty() {
                return Err(Error::Config(format!(
                    "Claude {label} model must not be blank"
                )));
            }
        }
        if self.enabled && self.configured_default().is_none() {
            return Err(Error::Config(
                "select a default Claude model before enabling the gateway".into(),
            ));
        }
        Ok(())
    }

    /// 默认模型的哈希；启用状态下由 `validate` 保证存在。
    pub fn configured_default(&self) -> Option<&str> {
        self.default_model_hash
            .as_deref()
            .filter(|id| !id.is_empty())
    }

    /// 每个非空引用连同它的用途标签，供保存前逐个确认模型仍然存在。
    pub fn model_references(&self) -> Vec<(&'static str, &str)> {
        [
            ("default", self.default_model_hash.as_deref()),
            ("sonnet", self.sonnet_model_hash.as_deref()),
            ("opus", self.opus_model_hash.as_deref()),
            ("haiku", self.haiku_model_hash.as_deref()),
        ]
        .into_iter()
        .filter_map(|(label, id)| id.filter(|id| !id.is_empty()).map(|id| (label, id)))
        .collect()
    }

    /// 别名家族 → 模型哈希；缺省不是错误，解析时回落到默认模型。
    pub fn family(&self, family: ModelFamily) -> Option<&str> {
        match family {
            ModelFamily::Sonnet => self.sonnet_model_hash.as_deref(),
            ModelFamily::Opus => self.opus_model_hash.as_deref(),
            ModelFamily::Haiku => self.haiku_model_hash.as_deref(),
        }
        .filter(|id| !id.is_empty())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFamily {
    Sonnet,
    Opus,
    Haiku,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClaudeStatus {
    pub enabled: bool,
    pub listening: bool,
    pub port: u16,
    /// Claude Code 侧应填的 `ANTHROPIC_BASE_URL`。
    pub base_url: String,
    /// 客户端侧应填的 `ANTHROPIC_AUTH_TOKEN`。管理接口只在本机可用，界面需要它来
    /// 生成启动命令；设置 JSON 本身不携带令牌。
    pub token: String,
    /// 当前映射里实际存在、且 `GET /v1/models` 会返回的模型数。
    pub models: usize,
    pub default_model_hash: Option<String>,
    pub calls: usize,
    pub last_error: Option<String>,
}

/// Anthropic 协议形状的错误。
///
/// `x-should-retry` / `retry-after` 是 Claude Code 的重试判据；上游适配器只把非成功
/// 状态写进错误文案，取不到原始响应头，所以这里按状态码给出可重试性。`retry-after`
/// 只在拿到明确秒数时才写（当前不写），不编造等待时间。
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
    pub should_retry: bool,
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "invalid_request_error",
            message: message.into(),
            should_retry: false,
            retry_after: None,
        }
    }

    pub fn authentication(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            kind: "authentication_error",
            message: message.into(),
            should_retry: false,
            retry_after: None,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            kind: "not_found_error",
            message: message.into(),
            should_retry: false,
            retry_after: None,
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            kind: "api_error",
            message: message.into(),
            should_retry: true,
            retry_after: None,
        }
    }

    /// 把共享层的错误折成 Anthropic 的错误。上游状态码只存在于适配器写好的文案里
    /// （`Anthropic 429 Too Many Requests: …`），按它恢复可重试性。
    pub fn from_error(error: &Error) -> Self {
        if matches!(error, Error::Cancelled) {
            return Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                kind: "api_error",
                message: "the request was cancelled".into(),
                should_retry: true,
                retry_after: None,
            };
        }
        if let Error::Protocol(message) = error {
            // 这条链路上的 `Error::Protocol` 来自适配器对上游 SSE 的解读（例如工具参数
            // 拼不出合法 JSON）或插件协议，不是客户端请求本身不合法：回 502 并标记可
            // 重试，Claude Code 会重发；以前的 400 让它按"请求写错了"处理，重发也不会好。
            // 客户端请求的 400 在 `messages` 里直接构造 `ApiError::invalid`。
            return Self {
                status: StatusCode::BAD_GATEWAY,
                kind: "api_error",
                message: format!("upstream protocol error: {message}"),
                should_retry: true,
                retry_after: None,
            };
        }
        if matches!(
            error,
            Error::Config(_) | Error::Store(_) | Error::Database(_) | Error::Migration(_)
        ) {
            return Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                kind: "api_error",
                message: error.to_string(),
                should_retry: false,
                retry_after: None,
            };
        }
        if let Error::Http(source) = error {
            if source.is_timeout() {
                return Self {
                    status: StatusCode::GATEWAY_TIMEOUT,
                    kind: "api_error",
                    message: error.to_string(),
                    should_retry: true,
                    retry_after: None,
                };
            }
        }
        let message = error.to_string();
        for (code, kind, retry) in [
            (400_u16, "invalid_request_error", false),
            (401, "authentication_error", false),
            (403, "permission_error", false),
            (404, "not_found_error", false),
            (413, "request_too_large", false),
            (429, "rate_limit_error", true),
            (500, "api_error", true),
            (502, "api_error", true),
            (503, "api_error", true),
            (504, "api_error", true),
            (529, "overloaded_error", true),
        ] {
            if message.contains(&format!(" {code} ")) {
                return Self {
                    status: StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                    kind,
                    message,
                    should_retry: retry,
                    retry_after: None,
                };
            }
        }
        let retryable = matches!(error, Error::Provider(_) | Error::Http(_));
        Self {
            status: StatusCode::BAD_GATEWAY,
            kind: "api_error",
            message,
            should_retry: retryable,
            retry_after: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({
            "type": "error",
            "error": {"type": self.kind, "message": self.message},
        }));
        let mut response = (self.status, body).into_response();
        let retry = if self.should_retry { "true" } else { "false" };
        response.headers_mut().insert(
            HeaderName::from_static("x-should-retry"),
            HeaderValue::from_static(retry),
        );
        if let Some(seconds) = self.retry_after {
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabling_requires_a_default_model_but_disabled_settings_are_permissive() {
        let mut settings = ClaudeSettings::default();
        assert!(settings.validate().is_ok(), "默认关闭时不要求选模型");

        settings.enabled = true;
        assert!(settings.validate().is_err());

        settings.default_model_hash = Some("  ".into());
        assert!(settings.validate().is_err(), "空白哈希不算选了模型");

        settings.default_model_hash = Some("abcd1234".into());
        settings.validate().unwrap();

        settings.port = 0;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn model_references_skip_unset_aliases_and_keep_their_labels() {
        let settings = ClaudeSettings {
            default_model_hash: Some("default".into()),
            sonnet_model_hash: None,
            opus_model_hash: Some("opus".into()),
            haiku_model_hash: Some(String::new()),
            ..ClaudeSettings::default()
        };
        assert_eq!(
            settings.model_references(),
            vec![("default", "default"), ("opus", "opus")]
        );
        assert_eq!(settings.family(ModelFamily::Sonnet), None);
        assert_eq!(settings.family(ModelFamily::Opus), Some("opus"));
    }

    /// 上游 SSE 解读失败不是客户端的错：回 502 可重试，而不是 400。
    #[test]
    fn an_upstream_protocol_error_is_a_gateway_error() {
        let error = ApiError::from_error(&Error::Protocol(
            "tool call arguments are not valid JSON".into(),
        ));
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.kind, "api_error");
        assert!(error.should_retry);
    }

    #[test]
    fn upstream_status_is_recovered_from_the_provider_message() {
        let overloaded =
            ApiError::from_error(&Error::Provider("Anthropic 529 Overloaded: busy".into()));
        assert_eq!(overloaded.status, StatusCode::from_u16(529).unwrap());
        assert_eq!(overloaded.kind, "overloaded_error");
        assert!(overloaded.should_retry);

        let rate = ApiError::from_error(&Error::Provider(
            "Anthropic 429 Too Many Requests: slow down".into(),
        ));
        assert_eq!(rate.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rate.kind, "rate_limit_error");
        assert!(rate.should_retry);

        let auth = ApiError::from_error(&Error::Provider(
            "Anthropic 401 Unauthorized: bad key".into(),
        ));
        assert_eq!(auth.status, StatusCode::UNAUTHORIZED);
        assert!(!auth.should_retry);

        let unknown = ApiError::from_error(&Error::Provider("connection reset".into()));
        assert_eq!(unknown.status, StatusCode::BAD_GATEWAY);
        assert!(unknown.should_retry);
    }
}
