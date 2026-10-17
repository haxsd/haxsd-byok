//! Defines the provider interface and exports provider implementations.
mod anthropic;
mod event;
mod normalize;
mod openai_chat;
mod openai_responses;
mod recorder;
mod request_template;
mod router;

use std::pin::Pin;

use futures_util::Stream;
use tokio_util::sync::CancellationToken;

use crate::{model::ModelInvocation, Result};

pub use anthropic::AnthropicProvider;
pub use event::*;
pub use openai_chat::OpenAiChatProvider;
pub use openai_responses::OpenAiResponsesProvider;
pub use recorder::CallRecorder;
pub use router::{build as build_provider, ProviderRouter};

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ModelEvent>> + Send>>;

pub trait Provider: Send + Sync {
    fn stream(
        &self,
        invocation: ModelInvocation,
        cancellation: CancellationToken,
    ) -> ProviderStream;
}

fn map_sse_error(
    label: &str,
    error: eventsource_stream::EventStreamError<crate::Error>,
) -> crate::Error {
    match error {
        eventsource_stream::EventStreamError::Transport(error) => error,
        eventsource_stream::EventStreamError::Utf8(error) => {
            crate::Error::Provider(format!("{label} SSE UTF-8 error: {error}"))
        }
        eventsource_stream::EventStreamError::Parser(error) => {
            crate::Error::Provider(format!("{label} SSE parse error: {error}"))
        }
    }
}

fn provider_event_error(label: &str, value: &serde_json::Value) -> Option<crate::Error> {
    let kind = value.get("type").and_then(serde_json::Value::as_str);
    let direct_error = value.get("error").filter(|error| !error.is_null());
    if !matches!(kind, Some("error" | "response.failed")) && direct_error.is_none() {
        return None;
    }

    let message = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            value
                .pointer("/response/error/message")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| direct_error.and_then(serde_json::Value::as_str))
        .or_else(|| {
            value
                .pointer("/response/error")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("provider returned an error event without a message");

    Some(crate::Error::Provider(format!("{label} error: {message}")))
}

fn merge_extra_params(body: &mut serde_json::Value, extra: &serde_json::Value) -> Result<()> {
    let extra = extra
        .as_object()
        .ok_or_else(|| crate::Error::Config("model extra params must be an object".into()))?;
    let body = body
        .as_object_mut()
        .ok_or_else(|| crate::Error::Provider("provider request body must be an object".into()))?;
    for (name, value) in extra {
        if matches!(
            name.as_str(),
            "model"
                | "stream"
                | "messages"
                | "input"
                | "tools"
                | "system"
                | "instructions"
                | "prompt_cache_key"
        ) {
            return Err(crate::Error::Config(format!(
                "model extra params cannot replace {name}"
            )));
        }
        body.insert(name.clone(), value.clone());
    }
    Ok(())
}

fn apply_openai_prompt_cache_key(body: &mut serde_json::Value, model_id: &str) -> Result<()> {
    if !model_id.to_ascii_lowercase().contains("gpt") {
        return Ok(());
    }
    body.as_object_mut()
        .ok_or_else(|| crate::Error::Provider("provider request body must be an object".into()))?
        .insert(
            "prompt_cache_key".into(),
            serde_json::Value::String("cursor-byok".into()),
        );
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing {
    //! 脚本化 SSE 上游：把一段 `data:` 事件喂给适配器，看它怎么收尾。
    //!
    //! 三个适配器的风险都集中在"结束条件"上——上游发了终止符、只发了 finish 就断开、
    //! 或者什么都没发就断开，三条路各自该产出什么。以前这些分支只能靠手工接真上游看，
    //! 于是改动很容易悄悄把"缺终止符"从报错变成静默成功。
    use super::*;
    use crate::config::{ProviderConfig, ProviderKind};
    use crate::model::{ModelInvocation, ModelRequest, ModelSpec, PromptSpec};

    /// 起一个只回一段 SSE 的本地上游，返回它的 URL。
    ///
    /// 响应带 Content-Length 且写完就关闭：这正是"上游没发终止符就断开"的形态。
    pub(crate) async fn sse_upstream(url_path: &str, events: Vec<serde_json::Value>) -> String {
        let body = events
            .into_iter()
            .map(|event| match event {
                // 裸字符串按原样写进 data 行（OpenAI 的 `[DONE]` 不是 JSON）。
                serde_json::Value::String(raw) => format!("data: {raw}\n\n"),
                event => format!("data: {event}\n\n"),
            })
            .collect::<String>();
        let app = axum::Router::new().fallback(move |_request: axum::extract::Request| {
            let body = body.clone();
            async move {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/event-stream; charset=utf-8",
                    )],
                    body,
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}{url_path}")
    }

    pub(crate) fn provider_config(kind: ProviderKind, request_url: String) -> ProviderConfig {
        ProviderConfig {
            kind,
            request_url,
            api_key: "test-key".into(),
            custom_headers: reqwest::header::HeaderMap::new(),
            max_output_tokens: None,
            // 全量套件一次并行跑 700+ 用例，机器是饱和的。请求超时给得短会把"CPU 排队"
            // 变成一次假的"流提前结束"，那正是这些用例要区分的东西，所以留足余量。
            request_timeout: std::time::Duration::from_secs(60),
        }
    }

    pub(crate) fn invocation(model_id: &str) -> ModelInvocation {
        ModelInvocation {
            client_source: crate::model::CallClient::Cursor,
            call_id: "sse-test-call".into(),
            run_id: "sse-test-run".into(),
            conversation_id: "sse-test-conversation".into(),
            provider_call_index: 0,
            request: ModelRequest {
                prompt: PromptSpec {
                    instructions: String::new(),
                    tools: Vec::new(),
                },
                model: ModelSpec::new(model_id.to_owned()),
                history: Vec::new(),
            },
        }
    }

    /// 收集整条流。返回 `Err` 表示适配器在流中途报了错（结束条件不满足就是这么报的）。
    pub(crate) async fn drain(stream: ProviderStream) -> Result<Vec<ModelEvent>> {
        futures_util::pin_mut!(stream);
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event?);
        }
        Ok(events)
    }

    pub(crate) fn text_of(events: &[ModelEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn finish_of(events: &[ModelEvent]) -> Option<FinishReason> {
        events.iter().find_map(|event| match event {
            ModelEvent::Done(finish) => Some(*finish),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_transport_errors_are_not_relabelled_as_parse_errors() {
        let error = map_sse_error(
            "test provider",
            eventsource_stream::EventStreamError::Transport(crate::Error::Provider(
                "connection closed".into(),
            )),
        );

        let crate::Error::Provider(message) = error else {
            panic!("transport error category must be preserved");
        };
        assert_eq!(message, "connection closed");
    }

    #[test]
    fn provider_error_events_extract_flat_and_nested_messages() {
        assert_provider_error(
            "OpenAI Responses",
            serde_json::json!({
                "type": "error",
                "message": "Internal error during token generation"
            }),
            "OpenAI Responses error: Internal error during token generation",
        );
        assert_provider_error(
            "OpenAI Chat",
            serde_json::json!({
                "error": {"message": "quota exceeded", "type": "server_error"}
            }),
            "OpenAI Chat error: quota exceeded",
        );
        assert_provider_error(
            "Anthropic",
            serde_json::json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"}
            }),
            "Anthropic error: Overloaded",
        );
        assert_provider_error(
            "OpenAI Responses",
            serde_json::json!({
                "type": "response.failed",
                "response": {"error": {"message": "generation failed"}}
            }),
            "OpenAI Responses error: generation failed",
        );
    }

    #[test]
    fn successful_provider_events_are_not_errors() {
        assert!(provider_event_error(
            "OpenAI Responses",
            &serde_json::json!({"type": "response.completed", "error": null})
        )
        .is_none());
    }

    fn assert_provider_error(label: &str, value: serde_json::Value, expected: &str) {
        let Some(crate::Error::Provider(message)) = provider_event_error(label, &value) else {
            panic!("expected provider error");
        };
        assert_eq!(message, expected);
    }
}
