//! Defines the server-wide error type and error conversions.
use std::sync::OnceLock;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use regex::Regex;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// 对话错误的稳定码。
///
/// 只覆盖经过本地对话链路的失败；客户端字段里的 `ConnectCode` 等协议枚举仍是
/// 各自的协议状态，这里表达的是"本地这一层为什么失败"。
pub mod code {
    pub const UPSTREAM_HTTP: &str = "BYOK-UPSTREAM_HTTP";
    pub const NETWORK: &str = "BYOK-NETWORK";
    pub const TIMEOUT: &str = "BYOK-TIMEOUT";
    pub const STREAM: &str = "BYOK-STREAM";
    pub const REQUEST: &str = "BYOK-REQUEST";
    pub const INTERNAL: &str = "BYOK-INTERNAL";
    pub const CANCELLED: &str = "BYOK-CANCELLED";
}

/// 诊断文本的上限：够定位一次失败，又不会把整份上游正文塞进日志和调用记录。
const MAX_DIAGNOSTIC_CHARS: usize = 4096;

/// 定位编号的展示上限：调用编号由本地生成，这里只防异常来源写进超长内容。
const MAX_REFERENCE_CHARS: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    /// 上游直接给出的错误文本（SSE error 事件、适配器自己拼的消息）。
    ///
    /// 保留原文供本地诊断；面向客户端时只取本地拼的前缀（见 `public_message`），
    /// 上游正文不会进入提示或诊断复制。
    #[error("provider error: {0}")]
    Provider(String),
    /// 上游用非成功 HTTP 状态拒绝了请求。
    ///
    /// 与 `Provider(String)` 分开是为了保留真实的 HTTP 状态：本地回给客户端的
    /// 502 不等于上游实际返回 502，错误码需要能区分这两件事。
    #[error("upstream rejected the request: {label} returned HTTP {status}: {body}")]
    UpstreamRejected {
        label: String,
        status: u16,
        body: String,
    },
    #[error("network error: {0}")]
    Network(String),
    #[error("timeout: {0}")]
    Timeout(String),
    /// 已面向客户端成文的错误：稳定码 + 安全摘要 + 定位编号。
    #[error("{code}: {summary} (ref {reference})")]
    Conversation {
        code: &'static str,
        summary: String,
        reference: String,
        status: Option<u16>,
        /// 脱敏前的本地诊断正文；只经 `diagnostic_message` 输出，Display 不包含它。
        detail: String,
    },
    #[error("store error: {0}")]
    Store(String),
    #[error("run was cancelled")]
    Cancelled,
    #[error("run not found: {0}")]
    RunNotFound(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error(
        "database migration stage '{stage}' timed out after {timeout_seconds} seconds; close other haxsd byok processes and try again"
    )]
    MigrationTimeout { stage: String, timeout_seconds: u64 },
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("protobuf decode error: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("protobuf encode error: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// 上游拒绝的统一文本：`<label> <status>: <body>`，保留旧的判定形状。
///
/// run 层按"冒号前的 3 位状态数字"识别确定性 4xx；HTTP 状态与正文都保留在这里，
/// 客户端可见文案另走 `public_message`。
pub(crate) fn upstream_rejection_message(label: &str, status: u16, body: &str) -> String {
    let status_text = reqwest::StatusCode::from_u16(status)
        .ok()
        .map(|status| {
            format!(
                "{} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            )
        })
        .unwrap_or_else(|| format!("HTTP {status}"));
    let status_text = status_text.trim();
    if body.trim().is_empty() {
        format!("{label} {status_text}")
    } else {
        format!("{label} {status_text}: {body}")
    }
}

/// `Provider(String)` 的客户端安全摘要：只取本地拼的前缀。
///
/// 没有前缀或前缀里带 URL 时给固定文案，不把 reqwest 的请求 URL、上游回显的
/// 令牌或任意英文正文当摘要。
pub(crate) fn provider_summary(message: &str) -> String {
    let head = message
        .split_once(": ")
        .map(|(head, _)| head)
        .filter(|head| !head.is_empty())
        .unwrap_or("the provider rejected the request");
    if head.contains("http://") || head.contains("https://") {
        "the provider request could not be completed".into()
    } else {
        bounded(&redact(head))
    }
}

impl Error {
    /// 稳定错误码；未知来源也归到一个明确的分类，不靠英文正文猜。
    pub fn code(&self) -> &'static str {
        match self {
            Self::UpstreamRejected { .. } => code::UPSTREAM_HTTP,
            Self::Network(_) => code::NETWORK,
            Self::Timeout(_) => code::TIMEOUT,
            Self::Http(error) if error.is_timeout() => code::TIMEOUT,
            Self::Http(error) if error.status().is_some() => code::UPSTREAM_HTTP,
            Self::Http(error) if error.is_builder() => code::REQUEST,
            Self::Http(error) if error.is_body() || error.is_decode() => code::STREAM,
            Self::Http(_) => code::NETWORK,
            Self::Cancelled => code::CANCELLED,
            Self::Config(_)
            | Self::Protocol(_)
            | Self::RunNotFound(_)
            | Self::Decode(_)
            | Self::Encode(_)
            | Self::Json(_) => code::REQUEST,
            // Provider 保存的是上游/流自己的失败；字符串本身不再参与分类。
            Self::Provider(_) => code::STREAM,
            Self::Conversation { code, .. } => code,
            Self::Store(_)
            | Self::Database(_)
            | Self::Migration(_)
            | Self::MigrationTimeout { .. }
            | Self::Io(_) => code::INTERNAL,
        }
    }

    /// 上游真实 HTTP 状态；本地回给客户端的状态不冒充它。
    pub fn upstream_status(&self) -> Option<u16> {
        match self {
            Self::UpstreamRejected { status, .. } => Some(*status),
            Self::Conversation { status, .. } => *status,
            Self::Http(error) => error.status().map(|status| status.as_u16()),
            _ => None,
        }
    }

    /// 把错误封装成面向客户端的报告：保留稳定码与定位编号。
    ///
    /// 已经封装过的错误原样返回，避免重复加前缀或改掉第一次的编号。
    pub fn conversation(self, reference: &str) -> Self {
        if matches!(self, Self::Conversation { .. }) {
            return self;
        }
        let status = self.upstream_status();
        let summary = self.safe_summary();
        let detail = self.diagnostic_text();
        Self::conversation_parts(reference, self.code(), status, summary, detail)
    }

    /// 直接按已知分类成文。
    ///
    /// `RunFailure` 只保存字符串与分类，拿不到原始错误对象时用它恢复同一条报告；
    /// 这不是第二条错误路径，字段与 `conversation` 完全一致。
    pub(crate) fn conversation_parts(
        reference: &str,
        code: &'static str,
        status: Option<u16>,
        summary: String,
        detail: String,
    ) -> Self {
        Self::Conversation {
            code,
            summary,
            reference: reference_label(reference),
            status,
            detail,
        }
    }

    /// 客户端可见文本：稳定码 + 安全摘要 + 定位编号。
    ///
    /// 不包含上游正文、URL、密钥或请求头；上游原文仍留在本地诊断里。
    pub fn public_message(&self) -> String {
        match self {
            Self::Conversation {
                code,
                summary,
                reference,
                ..
            } => format!("{code}: {summary} (ref {reference})"),
            _ => format!("{}: {} (ref unknown)", self.code(), self.safe_summary()),
        }
    }

    /// 指定请求编号的客户端文本；已经封装过的错误保留自己的编号。
    pub(crate) fn public_message_with_reference(&self, reference: &str) -> String {
        match self {
            Self::Conversation { .. } => self.public_message(),
            _ => format!(
                "{}: {} (ref {})",
                self.code(),
                self.safe_summary(),
                reference_label(reference)
            ),
        }
    }

    /// 可落日志的诊断文本：脱敏、限长，保留真实原因与错误链。
    pub fn diagnostic_message(&self) -> String {
        let status = self
            .upstream_status()
            .map(|status| format!(" status={status}"))
            .unwrap_or_default();
        let reference = match self {
            Self::Conversation { reference, .. } => format!(" ref={reference}"),
            _ => String::new(),
        };
        bounded(&format!(
            "{}{status}{reference} detail={}",
            self.code(),
            redact(&self.diagnostic_text())
        ))
    }

    /// 客户端可见的短摘要：只取本地能自证的那一段。
    fn safe_summary(&self) -> String {
        match self {
            Self::Conversation { summary, .. } => summary.clone(),
            Self::UpstreamRejected { label, status, .. } => {
                format!("{label} returned HTTP {status}")
            }
            Self::Provider(message) => crate::error::provider_summary(message),
            Self::Protocol(message) => bounded(&redact(message)),
            Self::Config(_) => "the request configuration was rejected".into(),
            Self::Network(_) => "network error while contacting the provider".into(),
            Self::Timeout(_) => "the provider did not respond before the timeout".into(),
            Self::Http(error) if error.is_timeout() => {
                "the provider did not respond before the timeout".into()
            }
            Self::Http(error) if error.status().is_some() => format!(
                "the provider returned HTTP {}",
                error.status().unwrap().as_u16()
            ),
            Self::Http(error) if error.is_builder() => {
                "the request configuration was rejected".into()
            }
            Self::Http(error) if error.is_body() || error.is_decode() => {
                "the provider response stream could not be read".into()
            }
            Self::Http(_) => "network error while contacting the provider".into(),
            Self::Cancelled => "run was cancelled".into(),
            Self::RunNotFound(_) => "run not found".into(),
            Self::Store(_)
            | Self::Database(_)
            | Self::Migration(_)
            | Self::MigrationTimeout { .. }
            | Self::Decode(_)
            | Self::Encode(_)
            | Self::Json(_)
            | Self::Io(_) => "the local server could not complete the request".into(),
        }
    }

    /// 本地诊断正文；Display 不一定包含错误链（`Provider` 只是字符串）。
    fn diagnostic_text(&self) -> String {
        match self {
            Self::Conversation { detail, .. } => detail.clone(),
            Self::Provider(message) => message.clone(),
            Self::UpstreamRejected {
                label,
                status,
                body,
            } => upstream_rejection_message(label, *status, body),
            Self::Network(message) | Self::Timeout(message) | Self::Protocol(message) => {
                message.clone()
            }
            Self::Cancelled => "run was cancelled".into(),
            Self::Http(error) => crate::diagnostics::error_chain(error),
            Self::Database(error) => crate::diagnostics::error_chain(error),
            Self::Migration(error) => crate::diagnostics::error_chain(error),
            Self::MigrationTimeout {
                stage,
                timeout_seconds,
            } => format!(
                "database migration stage '{stage}' timed out after {timeout_seconds} seconds"
            ),
            Self::Decode(error) => crate::diagnostics::error_chain(error),
            Self::Encode(error) => crate::diagnostics::error_chain(error),
            Self::Json(error) => crate::diagnostics::error_chain(error),
            Self::Io(error) => crate::diagnostics::error_chain(error),
            Self::Config(message) | Self::Store(message) | Self::RunNotFound(message) => {
                message.clone()
            }
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Config(_) | Self::Protocol(_) | Self::Decode(_) | Self::Json(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::RunNotFound(_) => StatusCode::NOT_FOUND,
            Self::Provider(_)
            | Self::Http(_)
            | Self::UpstreamRejected { .. }
            | Self::Network(_)
            | Self::Timeout(_)
            | Self::Conversation { .. } => StatusCode::BAD_GATEWAY,
            Self::Cancelled => StatusCode::CONFLICT,
            Self::Store(_)
            | Self::Database(_)
            | Self::Migration(_)
            | Self::MigrationTimeout { .. }
            | Self::Encode(_)
            | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // 所有回给 UI 的错误统一落日志,否则失败原因只出现在前端提示里。
        // 用错误链而不是 Display：`http error: ...` 这种只有最外层，真正的原因
        // （连不上、TLS 失败、代理拒绝）都在 source() 里。
        tracing::warn!(
            %status,
            code = self.code(),
            error = %self.diagnostic_message(),
            "request failed"
        );
        let code = match status {
            StatusCode::BAD_REQUEST => "invalid_argument",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "aborted",
            StatusCode::BAD_GATEWAY => "unavailable",
            _ => "internal",
        };
        // 仅已绑定本地对话的错误输出 BYOK 文案；管理 API 和官方转发保留其诊断语义，
        // 同样脱敏，但不能被误包装成一个没有调用编号的本地对话错误。
        let message = match &self {
            Self::Conversation { .. } => self.public_message(),
            _ => redact(&self.to_string()),
        };
        (
            status,
            Json(serde_json::json!({ "code": code, "message": message })),
        )
            .into_response()
    }
}

fn reference_label(reference: &str) -> String {
    let reference = reference.trim();
    let reference = if reference.is_empty() {
        "unknown"
    } else {
        reference
    };
    reference.chars().take(MAX_REFERENCE_CHARS).collect()
}

fn bounded(text: &str) -> String {
    if text.chars().count() <= MAX_DIAGNOSTIC_CHARS {
        return text.to_owned();
    }
    let mut text: String = text.chars().take(MAX_DIAGNOSTIC_CHARS).collect();
    text.push_str(" …<truncated>");
    text
}

/// 去掉 URL 内的凭据、敏感查询值、认证头与常见令牌。
///
/// 错误链里经常带请求 URL（`error sending request for url (...)`）；有的自建网关把
/// 密钥写进查询参数。这里在落日志和诊断前统一处理，不依赖调用点自觉。
pub(crate) fn redact(text: &str) -> String {
    let text = redact_urls(text);
    let text = quoted_credentials()
        .replace_all(&text, "$1\"[REDACTED]\"")
        .into_owned();
    let text = bearer_token()
        .replace_all(&text, "Bearer [REDACTED]")
        .into_owned();
    let text = authorization_header()
        .replace_all(&text, "$1: [REDACTED]")
        .into_owned();
    bounded(&secret_token().replace_all(&text, "[REDACTED]"))
}

fn quoted_credentials() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r#"(?i)("(?:api[-_]?key|access[-_]?token|refresh[-_]?token|token|secret|password|authorization|proxy-authorization|x-api-key|x-goog-api-key|cookie|set-cookie)"\s*:\s*)"[^"]*""#).unwrap())
}

fn bearer_token() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]+").unwrap())
}

fn authorization_header() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)\b(x-api-key|x-goog-api-key|api[-_]?key|authorization|proxy-authorization|x-auth-token|cookie|set-cookie)\b\s*[:=]\s*[^\r\n]+")
            .unwrap()
    })
}

fn secret_token() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?:sk|pk|xai|ant)-[A-Za-z0-9._-]{8,}").unwrap())
}

fn redact_urls(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = find_url_start(rest) {
        output.push_str(&rest[..index]);
        let tail = &rest[index..];
        let end = tail
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(character, ')' | '"' | '\'' | '>' | ']' | '}' | ',')
            })
            .unwrap_or(tail.len());
        let candidate = &tail[..end];
        match reqwest::Url::parse(candidate) {
            Ok(mut url) => {
                if !url.username().is_empty() || url.password().is_some() {
                    let _ = url.set_username("***");
                    let _ = url.set_password(Some("***"));
                }
                url.set_fragment(None);
                let pairs: Vec<(String, String)> = url
                    .query_pairs()
                    .map(|(key, value)| {
                        if is_sensitive_query_key(&key) {
                            (key.into_owned(), "***".to_owned())
                        } else {
                            (key.into_owned(), value.into_owned())
                        }
                    })
                    .collect();
                url.query_pairs_mut().clear().extend_pairs(pairs);
                output.push_str(url.as_str());
            }
            Err(_) => output.push_str("[REDACTED-URL]"),
        }
        rest = &tail[end..];
    }
    output.push_str(rest);
    output
}

fn find_url_start(text: &str) -> Option<usize> {
    let lowered = text.to_ascii_lowercase();
    let http = lowered.find("http://");
    let https = lowered.find("https://");
    match (http, https) {
        (Some(http), Some(https)) => Some(http.min(https)),
        (Some(http), None) => Some(http),
        (None, Some(https)) => Some(https),
        (None, None) => None,
    }
}

fn is_sensitive_query_key(key: &str) -> bool {
    const SENSITIVE: [&str; 8] = [
        "key",
        "token",
        "secret",
        "password",
        "auth",
        "signature",
        "credential",
        "sig",
    ];
    let lowered = key.to_ascii_lowercase();
    SENSITIVE.iter().any(|needle| lowered.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_failures_keep_their_typed_network_and_timeout_categories() {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", dead.local_addr().unwrap());
        drop(dead);
        let source = client.get(&url).send().await.unwrap_err();
        assert!(source.is_connect());
        let error = Error::Http(source).conversation("network-call");
        assert_eq!(error.code(), code::NETWORK);
        assert!(!error.public_message().contains("127.0.0.1"));
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source = client
            .get(format!("http://{}", silent.local_addr().unwrap()))
            .timeout(std::time::Duration::from_millis(100))
            .send()
            .await
            .unwrap_err();
        assert!(source.is_timeout());
        assert_eq!(
            Error::Http(source).conversation("timeout-call").code(),
            code::TIMEOUT
        );
    }

    #[tokio::test]
    async fn management_errors_keep_their_detail_without_conversation_branding() {
        let response = Error::Provider("unknown model: missing-hash".into()).into_response();
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("missing-hash"));
        assert!(!text.contains("BYOK-"));
        let response = Error::Provider("OpenAI error: external error body".into())
            .conversation("fixture-call")
            .into_response();
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("BYOK-STREAM"));
        assert!(text.contains("fixture-call"));
        assert!(!text.contains("external error body"));
    }

    #[test]
    fn upstream_rejections_keep_the_real_status_but_show_no_body_to_clients() {
        let error = Error::UpstreamRejected {
            label: "OpenAI Chat".into(),
            status: 400,
            body: "invalid api key sk-live-secret-123".into(),
        }
        .conversation("cursor-run:42");

        assert_eq!(error.code(), code::UPSTREAM_HTTP);
        assert_eq!(error.upstream_status(), Some(400));

        let public = error.public_message();
        assert!(public.contains(code::UPSTREAM_HTTP), "{public}");
        assert!(public.contains("HTTP 400"), "{public}");
        assert!(public.contains("cursor-run:42"), "{public}");
        assert!(!public.contains("sk-live-secret-123"), "{public}");
        assert!(!public.contains("invalid api key"), "{public}");

        let diagnostic = error.diagnostic_message();
        assert!(diagnostic.contains(code::UPSTREAM_HTTP), "{diagnostic}");
        assert!(!diagnostic.contains("sk-live-secret-123"), "{diagnostic}");
    }

    /// 已有定位编号的错误不能再包一层：第二次编号既不是这次调用的，也会让前缀重复。
    #[test]
    fn conversation_wrapping_is_idempotent() {
        let error = Error::Provider("Anthropic 400 Bad Request: upstream body".into())
            .conversation("run-1")
            .conversation("run-2");

        assert!(error.public_message().contains("run-1"));
        assert!(!error.public_message().contains("run-2"));
    }

    #[test]
    fn diagnostics_are_bounded() {
        let diagnostic = Error::Provider("x".repeat(5_000)).diagnostic_message();

        assert!(
            diagnostic.chars().count() <= MAX_DIAGNOSTIC_CHARS + " …<truncated>".len(),
            "diagnostic must stay bounded"
        );
        assert!(diagnostic.ends_with(" …<truncated>"), "{diagnostic}");
    }

    #[test]
    fn diagnostics_redact_quoted_credentials_cookies_and_url_fragments() {
        let body = r#"{"api_key":"opaque-key-value","Authorization":"Basic encoded-credential","cookie":"sid=session-secret"} HTTPS://name:password@example.com/model#token=fragment-secret
Cookie: sid=plain-cookie-secret
Authorization: Basic plain-basic-secret"#;
        let diagnostic = Error::Provider(body.into()).diagnostic_message();
        for secret in [
            "opaque-key-value",
            "encoded-credential",
            "session-secret",
            "password",
            "fragment-secret",
            "plain-cookie-secret",
            "plain-basic-secret",
        ] {
            assert!(
                !diagnostic.contains(secret),
                "leaked {secret}: {diagnostic}"
            );
        }
    }

    #[test]
    fn diagnostics_drop_credentials_and_tokens() {
        let body = "upstream said: https://user:pass@example.com/v1/chat?key=sk-query-secret&model=x Authorization: Bearer abc.def.ghi";
        let diagnostic = Error::Provider(body.into()).diagnostic_message();

        assert!(!diagnostic.contains("pass"), "{diagnostic}");
        assert!(!diagnostic.contains("sk-query-secret"), "{diagnostic}");
        assert!(!diagnostic.contains("abc.def.ghi"), "{diagnostic}");
        assert!(diagnostic.contains("[REDACTED]"), "{diagnostic}");
    }

    #[test]
    fn missing_source_facts_are_marked_unknown_instead_of_guessed() {
        let error = Error::Provider("something broke".into());

        assert_eq!(error.upstream_status(), None);
        assert!(error.public_message().contains("ref unknown"));
        assert!(error.public_message().contains(code::STREAM));
    }

    #[test]
    fn public_summaries_never_include_urls_or_tokens() {
        let error = Error::Provider(
            "error sending request for url (https://user:pass@example.com/v1) sk-live-secret-1"
                .into(),
        );
        let public = error.public_message();
        assert!(!public.contains("https://"), "{public}");
        assert!(!public.contains("sk-live-secret-1"), "{public}");
        assert!(!public.contains("pass"), "{public}");
    }

    #[test]
    fn typed_network_and_timeout_errors_carry_their_own_codes() {
        assert_eq!(
            Error::Network("connect refused".into()).code(),
            code::NETWORK
        );
        assert_eq!(Error::Timeout("no response".into()).code(), code::TIMEOUT);
        assert_eq!(Error::Cancelled.code(), code::CANCELLED);
    }
}
