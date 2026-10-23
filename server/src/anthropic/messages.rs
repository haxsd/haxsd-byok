//! Anthropic Messages 请求映射与 SSE 响应生成。
//!
//! 这里只做协议边界的翻译：请求变成共享的 `ModelInvocation`，共享的 `ModelEvent`
//! 流变回 Anthropic 的 `message_start` / `content_block_*` / `message_delta` /
//! `message_stop`。出站协议由 `crate::provider` 的适配器决定。
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    model::{
        ContentPart, ModelInvocation, ModelSpec, ProjectedContent, ProjectedMessage, PromptSpec,
        ProviderReplayState, Role, ToolCallContent, ToolDefinition, ToolResultContent, Usage,
    },
    network::NetworkClients,
    provider::{FinishReason, ModelEvent},
    store::Store,
};

use super::{models, ApiError, ClaudeSettings};

/// URL 图片的下载上限与时限。Claude Code 绝大多数时候发 base64，URL 只是协议允许的
/// 另一种来源；下载行为必须有明确边界，不能把任意地址变成无界的内存占用。
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const IMAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// SSE 心跳间隔。上游还没产出第一个事件时也要让客户端看到字节，否则连接会被
/// 客户端自己的请求超时掐断。
pub const PING_INTERVAL: Duration = Duration::from_secs(10);

/// 中途 `role: "system"` 转成 user 文本块时加的一行说明：模型需要知道这段话不是
/// 用户说的，而是客户端插入的操作方提醒。
const OPERATOR_NOTE: &str = "[operator note from the client]";

pub struct PreparedRequest {
    pub invocation: ModelInvocation,
    /// 回给客户端的 `message.model`：原样回显它请求的 ID（缺省时用解析出的哈希）。
    pub requested_model: String,
    /// 客户端是否要求 SSE。`stream` 缺省按 Anthropic 的默认（非流式）处理。
    pub stream: bool,
}

/// 把 `POST /v1/messages` 的正文投影成一次共享模型调用。
///
/// 流式与非流式共用同一条投影与同一个上游调用：非流式只是把事件流收起之后再写成
/// 一份完整消息，协议语义（思考签名、停止序列、stop_reason）只实现一次。
pub async fn prepare(
    store: &Store,
    settings: &ClaudeSettings,
    body: &Value,
    clients: &NetworkClients,
) -> Result<PreparedRequest, ApiError> {
    let object = body
        .as_object()
        .ok_or_else(|| ApiError::invalid("request body must be a JSON object"))?;
    let stream = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_tokens = object
        .get("max_tokens")
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens > 0)
        .ok_or_else(|| ApiError::invalid("max_tokens must be a positive integer"))?;
    let (invocation, requested_model) =
        build(store, settings, object, clients, Some(max_tokens), true).await?;
    Ok(PreparedRequest {
        invocation,
        requested_model,
        stream,
    })
}

/// `POST /v1/messages/count_tokens`：没有精确分词器，返回共享估算器的结果。
///
/// `max_tokens` 与 `stream` 在这个端点上都不是必填，所以复用同一个映射函数时只要求
/// 消息本身合法。
///
/// 这里**不下载 URL 图片**：估算只需要一个量级，而一次 20 秒的下载超时会把 Claude
/// Code 的计数请求变成网络副作用。图片按共享估算器的固定价格计入。
pub async fn count_tokens(
    store: &Store,
    settings: &ClaudeSettings,
    body: &Value,
    clients: &NetworkClients,
) -> Result<u64, ApiError> {
    let object = body
        .as_object()
        .ok_or_else(|| ApiError::invalid("request body must be a JSON object"))?;
    let (invocation, _) = build(store, settings, object, clients, None, false).await?;
    Ok(crate::model::estimate_context_tokens(
        &invocation.request.prompt,
        &invocation.request.history,
    ))
}

async fn build(
    store: &Store,
    settings: &ClaudeSettings,
    object: &serde_json::Map<String, Value>,
    clients: &NetworkClients,
    max_tokens: Option<u64>,
    fetch_images: bool,
) -> Result<(ModelInvocation, String), ApiError> {
    let requested = object.get("model").and_then(Value::as_str).unwrap_or("");
    let model = models::resolve(store, settings, requested).await?;
    let (history, leading_system) = history(object.get("messages"), clients, fetch_images).await?;
    // 对话开头的 system 消息属于顶层 system：它们比 `system` 参数晚出现，接在后面
    // 保持时间顺序。
    let mut instructions = system_text(object.get("system"))?;
    for text in leading_system {
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(&text);
    }
    let prompt = PromptSpec {
        instructions,
        tools: tool_definitions(object.get("tools"))?,
    };
    let stop_sequences = stop_sequences(object.get("stop_sequences"))?;

    let mut spec = ModelSpec::new(model.model_hash.clone());
    if let Some(requested) = max_tokens {
        spec.max_output_tokens = Some(match model.max_output_tokens() {
            Some(limit) => requested.min(limit),
            None => requested,
        });
    }
    spec.stop_sequences = stop_sequences;
    apply_thinking(object.get("thinking"), &mut spec)?;

    let call_id = format!("claude-{}", Uuid::new_v4());
    let conversation_id = object
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("claude:{value}"))
        .unwrap_or_else(|| call_id.clone());
    let invocation = ModelInvocation {
        purpose: crate::model::CallPurpose::Chat,
        client_source: crate::model::CallClient::Claude,
        run_id: call_id.clone(),
        call_id,
        conversation_id,
        provider_call_index: 0,
        request: crate::model::ModelRequest {
            prompt,
            model: spec,
            history,
        },
    };
    let echoed = if requested.trim().is_empty() {
        model.model_hash
    } else {
        requested.to_owned()
    };
    Ok((invocation, echoed))
}

fn system_text(value: Option<&Value>) -> Result<String, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            for block in blocks {
                let Some(kind) = block.get("type").and_then(Value::as_str) else {
                    continue;
                };
                if kind != "text" {
                    continue;
                }
                if let Some(part) = block.get("text").and_then(Value::as_str) {
                    if !part.is_empty() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(part);
                    }
                }
            }
            Ok(text)
        }
        Some(_) => Err(ApiError::invalid(
            "system must be a string or an array of text blocks",
        )),
    }
}

fn tool_definitions(value: Option<&Value>) -> Result<Vec<ToolDefinition>, ApiError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let tools = value
        .as_array()
        .ok_or_else(|| ApiError::invalid("tools must be an array"))?;
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| ApiError::invalid(format!("tools[{index}].name is required")))?;
            let parameters = tool
                .get("input_schema")
                .filter(|schema| schema.is_object())
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Ok(ToolDefinition {
                name: name.to_owned(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                parameters,
            })
        })
        .collect()
}

fn stop_sequences(value: Option<&Value>) -> Result<Vec<String>, ApiError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let sequences = value
        .as_array()
        .ok_or_else(|| ApiError::invalid("stop_sequences must be an array of strings"))?;
    Ok(sequences
        .iter()
        .filter_map(Value::as_str)
        .filter(|sequence| !sequence.is_empty())
        .map(str::to_owned)
        .collect())
}

fn apply_thinking(value: Option<&Value>, spec: &mut ModelSpec) -> Result<(), ApiError> {
    let mut enabled = false;
    match value {
        None | Some(Value::Null) => {}
        Some(Value::Object(object)) => match object.get("type").and_then(Value::as_str) {
            Some("enabled") => {
                enabled = true;
                spec.reasoning.thinking_budget_tokens =
                    object.get("budget_tokens").and_then(Value::as_u64);
            }
            Some("adaptive") => enabled = true,
            // `disabled` 与缺省都表示这一轮不要在网关侧再补思考：Claude Code 的请求
            // 是自己能力的声明，不能按模型配置替它打开。
            Some("disabled") | None => {}
            Some(other) => {
                return Err(ApiError::invalid(format!(
                    "unsupported thinking type: {other}"
                )))
            }
        },
        Some(_) => return Err(ApiError::invalid("thinking must be an object")),
    }
    spec.reasoning.enabled = Some(enabled);
    Ok(())
}

/// 把入站 `messages` 投影成共享历史，并挑出属于顶层 system 的前导 system 文本。
///
/// `role: "system"` 在 Anthropic 的会话数组里不是合法角色，但 Opus 4.8+ 与一些未
/// 识别模型会把 reminder / attachment 以这个角色塞进对话中间。处理方式（参照
/// LiteLLM）：
/// - 对话开头的连续 system 合并进顶层 `system`；
/// - 中途的 system 转成带 operator note 的 user 文本块；
/// - 中途 system 一律排在 tool_result 之后，绝不插在 tool_use 与它的结果之间。
///
/// 已经存在的 user / assistant 消息不做改写，只新增块，缓存前缀因此保持稳定。
async fn history(
    value: Option<&Value>,
    clients: &NetworkClients,
    fetch_images: bool,
) -> Result<(Vec<ProjectedMessage>, Vec<String>), ApiError> {
    let Some(value) = value else {
        return Err(ApiError::invalid("messages is required"));
    };
    let messages = value
        .as_array()
        .ok_or_else(|| ApiError::invalid("messages must be an array"))?;
    if messages.is_empty() {
        return Err(ApiError::invalid("messages must not be empty"));
    }
    let mut history = Vec::new();
    let mut leading_system = Vec::new();
    let mut pending_system: Vec<(usize, String)> = Vec::new();
    let mut leading = true;
    for (index, message) in messages.iter().enumerate() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::invalid(format!("messages[{index}].role is required")))?;
        let content = message
            .get("content")
            .ok_or_else(|| ApiError::invalid(format!("messages[{index}].content is required")))?;
        if role == "system" {
            let text = system_text(Some(content))?;
            if text.is_empty() {
                continue;
            }
            if leading {
                leading_system.push(text);
            } else {
                pending_system.push((index, text));
            }
            continue;
        }
        leading = false;
        match role {
            "user" => {
                // 含工具结果的用户消息必须紧跟在 assistant 的 tool_use 之后，所以
                // 待落位的 system 要等这条消息投影完再追加。
                let after_tool_result = has_tool_result(content);
                if !after_tool_result {
                    flush_system(&mut pending_system, &mut history);
                }
                user_message(index, content, clients, fetch_images, &mut history).await?;
                if after_tool_result {
                    flush_system(&mut pending_system, &mut history);
                }
            }
            "assistant" => {
                flush_system(&mut pending_system, &mut history);
                assistant_message(index, content, &mut history)?;
            }
            other => {
                return Err(ApiError::invalid(format!(
                    "unsupported message role: {other}"
                )))
            }
        }
    }
    flush_system(&mut pending_system, &mut history);
    Ok((history, leading_system))
}

fn has_tool_result(content: &Value) -> bool {
    content.as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
    })
}

/// 把待落位的中途 system 写成一条独立的 user 消息：每个 system 文本块一个文本块，
/// 各带一行 operator note。连续多条合并进同一条消息，块顺序保持原样。
fn flush_system(pending: &mut Vec<(usize, String)>, history: &mut Vec<ProjectedMessage>) {
    if pending.is_empty() {
        return;
    }
    let index = pending[0].0;
    let parts = pending
        .drain(..)
        .map(|(_, text)| {
            let mut content = String::from(OPERATOR_NOTE);
            content.push('\n');
            content.push_str(&text);
            ContentPart::Text { text: content }
        })
        .collect();
    history.push(ProjectedMessage {
        message_id: message_id(index, 0),
        role: Role::User,
        content: ProjectedContent::Parts(parts),
    });
}

async fn user_message(
    index: usize,
    content: &Value,
    clients: &NetworkClients,
    fetch_images: bool,
    history: &mut Vec<ProjectedMessage>,
) -> Result<(), ApiError> {
    if let Some(text) = content.as_str() {
        push_user_text(index, text, history);
        return Ok(());
    }
    let blocks = content.as_array().ok_or_else(|| {
        ApiError::invalid(format!(
            "messages[{index}].content must be a string or an array"
        ))
    })?;
    let mut parts = Vec::new();
    let mut sequence = 0_usize;
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        parts.push(ContentPart::Text {
                            text: text.to_owned(),
                        });
                    }
                }
            }
            Some("image") => parts.push(image_part(block, clients, fetch_images).await?),
            Some("tool_result") => {
                flush_parts(index, sequence, &mut parts, history);
                sequence += 1;
                history.push(
                    tool_result_message(index, sequence, block, clients, fetch_images).await?,
                );
                sequence += 1;
            }
            // 未识别的块（未来的协议扩展）跳过，而不是让整轮请求失败。
            Some(_) | None => {}
        }
    }
    flush_parts(index, sequence, &mut parts, history);
    Ok(())
}

fn assistant_message(
    index: usize,
    content: &Value,
    history: &mut Vec<ProjectedMessage>,
) -> Result<(), ApiError> {
    let blocks = match content {
        Value::String(text) => {
            if !text.is_empty() {
                history.push(ProjectedMessage {
                    message_id: message_id(index, 0),
                    role: Role::Assistant,
                    content: ProjectedContent::Assistant {
                        text: text.clone(),
                        thinking: String::new(),
                        replay_state: None,
                        calls: Vec::new(),
                    },
                });
            }
            return Ok(());
        }
        Value::Array(blocks) => blocks,
        _ => {
            return Err(ApiError::invalid(format!(
                "messages[{index}].content must be a string or an array"
            )))
        }
    };
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls = Vec::new();
    let mut replay_blocks = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(part) = block.get("text").and_then(Value::as_str) {
                    if !part.is_empty() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(part);
                    }
                }
            }
            Some("thinking") => {
                let part = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                thinking.push_str(part);
                let signature = block.get("signature").and_then(Value::as_str).unwrap_or("");
                if !signature.is_empty() {
                    replay_blocks.push(json!({
                        "type": "thinking",
                        "thinking": part,
                        "signature": signature,
                    }));
                }
            }
            Some("redacted_thinking") => {
                // 数据不透明，原样回放；没有 data 的块无法重组，丢掉。
                if block.get("data").and_then(Value::as_str).is_some() {
                    replay_blocks.push(block.clone());
                }
            }
            Some("tool_use") => {
                let call_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        ApiError::invalid(format!(
                            "messages[{index}].content tool_use.id is required"
                        ))
                    })?;
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        ApiError::invalid(format!(
                            "messages[{index}].content tool_use.name is required"
                        ))
                    })?;
                calls.push(ToolCallContent {
                    index: calls.len(),
                    call_id: call_id.to_owned(),
                    name: name.to_owned(),
                    arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                });
            }
            Some(_) | None => {}
        }
    }
    if text.is_empty() && thinking.is_empty() && calls.is_empty() && replay_blocks.is_empty() {
        return Ok(());
    }
    history.push(ProjectedMessage {
        message_id: message_id(index, 0),
        role: Role::Assistant,
        content: ProjectedContent::Assistant {
            text,
            thinking,
            replay_state: (!replay_blocks.is_empty()).then(|| ProviderReplayState {
                provider_kind: "anthropic".into(),
                value: json!({"blocks": replay_blocks}),
            }),
            calls,
        },
    });
    Ok(())
}

async fn tool_result_message(
    index: usize,
    ordinal: usize,
    block: &Value,
    clients: &NetworkClients,
    fetch_images: bool,
) -> Result<ProjectedMessage, ApiError> {
    let call_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ApiError::invalid(format!(
                "messages[{index}].content tool_result.tool_use_id is required"
            ))
        })?
        .to_owned();
    let mut text = String::new();
    let mut provider_parts = Vec::new();
    match block.get("content") {
        None | Some(Value::Null) => {}
        Some(Value::String(content)) => text.push_str(content),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(value) = part.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(value);
                            provider_parts.push(ContentPart::Text {
                                text: value.to_owned(),
                            });
                        }
                    }
                    Some("image") => {
                        provider_parts.push(image_part(part, clients, fetch_images).await?)
                    }
                    Some(_) | None => {}
                }
            }
        }
        Some(_) => {
            return Err(ApiError::invalid(format!(
                "messages[{index}].content tool_result.content must be a string or an array"
            )))
        }
    }
    Ok(ProjectedMessage {
        message_id: message_id(index, ordinal),
        role: Role::Tool,
        content: ProjectedContent::ToolResult(ToolResultContent {
            call_id,
            // tool_result 不回传工具名；出站 Anthropic 适配器只使用 call_id 与内容。
            name: "_".into(),
            content: text,
            is_error: block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            image: None,
            provider_parts,
        }),
    })
}

fn push_user_text(index: usize, text: &str, history: &mut Vec<ProjectedMessage>) {
    if text.is_empty() {
        return;
    }
    history.push(ProjectedMessage {
        message_id: message_id(index, 0),
        role: Role::User,
        content: ProjectedContent::Parts(vec![ContentPart::Text {
            text: text.to_owned(),
        }]),
    });
}

fn flush_parts(
    index: usize,
    ordinal: usize,
    parts: &mut Vec<ContentPart>,
    history: &mut Vec<ProjectedMessage>,
) {
    if parts.is_empty() {
        return;
    }
    history.push(ProjectedMessage {
        message_id: message_id(index, ordinal),
        role: Role::User,
        content: ProjectedContent::Parts(std::mem::take(parts)),
    });
}

fn message_id(index: usize, ordinal: usize) -> String {
    let mut id = String::from("claude-message-");
    id.push_str(&index.to_string());
    id.push('-');
    id.push_str(&ordinal.to_string());
    id
}

/// 图片块 → canonical 图片。`fetch_images` 为假时（`/count_tokens`）只做形状校验，
/// 不解码也不下载：估算器对空字节给出固定价格，正好是"知道这里有一张图"的量级。
async fn image_part(
    block: &Value,
    clients: &NetworkClients,
    fetch_images: bool,
) -> Result<ContentPart, ApiError> {
    let source = block
        .get("source")
        .ok_or_else(|| ApiError::invalid("image.source is required"))?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let mime_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ApiError::invalid("image.source.media_type is required"))?;
            let encoded = source
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::invalid("image.source.data is required"))?;
            if !fetch_images {
                if encoded.is_empty() {
                    return Err(ApiError::invalid("image.source.data is required"));
                }
                return Ok(ContentPart::Image {
                    mime_type: mime_type.to_owned(),
                    data: Vec::new(),
                });
            }
            let data = STANDARD.decode(encoded).map_err(|error| {
                ApiError::invalid(format!("image.source.data is not base64: {error}"))
            })?;
            Ok(ContentPart::Image {
                mime_type: mime_type.to_owned(),
                data,
            })
        }
        Some("url") => {
            let url = source
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::invalid("image.source.url is required"))?;
            if !fetch_images {
                // 校验 URL 形状但不联网：估算只需要"这里有一张图"。
                reqwest::Url::parse(url)
                    .map_err(|error| ApiError::invalid(format!("invalid image URL: {error}")))?;
                return Ok(ContentPart::Image {
                    mime_type: "image/unknown".into(),
                    data: Vec::new(),
                });
            }
            let (mime_type, data) = fetch_image(clients, url).await?;
            Ok(ContentPart::Image { mime_type, data })
        }
        Some(other) => Err(ApiError::invalid(format!(
            "unsupported image source type: {other}"
        ))),
        None => Err(ApiError::invalid("image.source.type is required")),
    }
}

/// 下载 URL 图片。共享消息类型只存字节，没有 URL 形态，所以这里必须把图片取回来——
/// 或者明确拒绝；让 URL 原样穿过投影会在出站适配器里变成一张空图片。
async fn fetch_image(clients: &NetworkClients, url: &str) -> Result<(String, Vec<u8>), ApiError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| ApiError::invalid(format!("invalid image URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::invalid("image URL must use http or https"));
    }
    let client = clients
        .default_client()
        .await
        .map_err(|error| models::internal(error.to_string()))?;
    let response = client
        .get(parsed)
        .timeout(IMAGE_FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|error| ApiError::invalid(format!("image download failed: {error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::invalid(format!(
            "image download returned HTTP {}",
            response.status()
        )));
    }
    let mime_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_owned())
        .filter(|value| value.starts_with("image/"))
        .ok_or_else(|| ApiError::invalid("image URL did not return an image content type"))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_IMAGE_BYTES as u64)
    {
        return Err(ApiError::invalid("image is larger than 20 MB"));
    }
    let mut data = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| ApiError::invalid(format!("image download failed: {error}")))?;
        if data.len() + chunk.len() > MAX_IMAGE_BYTES {
            return Err(ApiError::invalid("image is larger than 20 MB"));
        }
        data.extend_from_slice(&chunk);
    }
    Ok((mime_type, data))
}

/// 共享事件流 → Anthropic SSE 事件。
pub struct ResponseTranslator {
    message_id: String,
    model: String,
    next_index: usize,
    open: Option<OpenBlock>,
    /// 客户端内容块下标 → 已预留的工具块；参数缓冲到 `ToolCallEnd` 再整段交付。
    ///
    /// 以客户端块下标为键，交付顺序就等于调用顺序，与 provider 交错完成的顺序无关。
    tools: BTreeMap<usize, PendingTool>,
    /// `message_start` 里的输入估算；`message_delta` 的权威用量会覆盖它。
    estimated_input_tokens: u64,
    usage: Usage,
    finish: Option<FinishReason>,
    stopped_by: Option<String>,
    text: StopFilter,
    finished: bool,
}

struct OpenBlock {
    index: usize,
    kind: BlockKind,
    provider_index: usize,
}

/// 一个已经开始、但还没收完参数的工具调用。
struct PendingTool {
    /// provider 给的工具下标；`ToolCallArgumentsDelta` / `ToolCallEnd` 按它找回来。
    provider_index: usize,
    call_id: String,
    name: String,
    arguments: String,
    /// provider 已经发了 `ToolCallEnd`；等前面的工具先交付完再整段发出。
    ended: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

impl ResponseTranslator {
    pub fn new(
        message_id: impl Into<String>,
        model: impl Into<String>,
        stop_sequences: Vec<String>,
        estimated_input_tokens: u64,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            next_index: 0,
            open: None,
            tools: BTreeMap::new(),
            estimated_input_tokens,
            usage: Usage::default(),
            finish: None,
            stopped_by: None,
            text: StopFilter::new(stop_sequences),
            finished: false,
        }
    }

    pub fn start(&self) -> Value {
        json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                // 客户端在 `message_start` 就要显示上下文占用，而权威用量要到
                // `message_delta` 才到；这里给共享估算器的值，后者仍然覆盖它。
                "usage": {
                    "input_tokens": self.estimated_input_tokens,
                    "output_tokens": 0,
                },
            },
        })
    }

    pub fn push(&mut self, event: ModelEvent) -> Vec<Value> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if self.stopped_by.is_some() {
            // 停止序列命中之后不再产出内容；用量与终止原因仍要收下。
            match event {
                ModelEvent::Usage(usage) => self.usage = usage,
                ModelEvent::Done(reason) => self.finish = Some(reason),
                ModelEvent::ProviderReplayState(state) => self.redacted_blocks(&mut out, state),
                _ => {}
            }
            return out;
        }
        match event {
            ModelEvent::Start { .. } => {}
            ModelEvent::TextStart => {
                self.open_block(&mut out, BlockKind::Text, 0);
            }
            ModelEvent::TextDelta(delta) => {
                self.ensure_open(&mut out, BlockKind::Text, 0);
                let text = self.text.push(&delta);
                if !text.is_empty() {
                    self.delta(
                        &mut out,
                        BlockKind::Text,
                        0,
                        json!({
                            "type": "text_delta",
                            "text": text,
                        }),
                    );
                }
                if self.text.stopped.is_some() {
                    self.stopped_by = self.text.stopped.clone();
                    self.close(&mut out);
                }
            }
            ModelEvent::TextEnd => self.close_kind(&mut out, BlockKind::Text),
            ModelEvent::ThinkingStart => {
                self.open_block(&mut out, BlockKind::Thinking, 0);
            }
            ModelEvent::ThinkingDelta(delta) => {
                self.ensure_open(&mut out, BlockKind::Thinking, 0);
                self.delta(
                    &mut out,
                    BlockKind::Thinking,
                    0,
                    json!({
                        "type": "thinking_delta",
                        "thinking": delta,
                    }),
                );
            }
            ModelEvent::ThinkingSignature(signature) => {
                if self
                    .open
                    .as_ref()
                    .is_some_and(|b| b.kind == BlockKind::Thinking)
                {
                    self.delta(
                        &mut out,
                        BlockKind::Thinking,
                        0,
                        json!({
                            "type": "signature_delta",
                            "signature": signature,
                        }),
                    );
                }
            }
            ModelEvent::ThinkingEnd => self.close_kind(&mut out, BlockKind::Thinking),
            ModelEvent::ToolCallStart {
                index,
                call_id,
                name,
            } => {
                // 只预留块下标：参数可能交错到达（Anthropic 用 index 区分并行工具），
                // 而一个流里同时只能有一个打开的内容块。整段参数在 `ToolCallEnd`
                // 统一交付，见 `flush_tool`。
                if !self.tools.values().any(|tool| tool.provider_index == index) {
                    let block = self.next_index;
                    self.next_index += 1;
                    self.tools.insert(
                        block,
                        PendingTool {
                            provider_index: index,
                            call_id,
                            name,
                            arguments: String::new(),
                            ended: false,
                        },
                    );
                }
            }
            ModelEvent::ToolCallArgumentsDelta { index, delta } => {
                // 没有 start 的参数增量无法写进合法的 content block；丢掉它，
                // 而不是发一段没有块头的 delta。
                if let Some(tool) = self.pending_tool_mut(index) {
                    tool.arguments.push_str(&delta);
                }
            }
            ModelEvent::ToolCallEnd { index } => {
                if let Some(tool) = self.pending_tool_mut(index) {
                    tool.ended = true;
                }
                self.flush_finished_tools(&mut out);
            }
            ModelEvent::ProviderReplayState(state) => {
                self.redacted_blocks(&mut out, state);
            }
            ModelEvent::StopSequence(sequence) => {
                // 上游命中的停止序列是权威答案：本地过滤器只能在文本里看见序列时
                // 才命中，模型在序列前就收尾时它无从得知。以它为准并关掉当前块，
                // 后面不会再产出内容。
                if self.stopped_by.is_none() {
                    self.stopped_by = Some(sequence);
                    self.close(&mut out);
                }
            }
            ModelEvent::Usage(usage) => self.usage = usage,
            ModelEvent::Done(reason) => self.finish = Some(reason),
        }
        out
    }

    pub fn finish(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.finished = true;
        // provider 没发 `ToolCallEnd` 就结束时，缓冲的参数仍然要交付：块已经预留，
        // 丢掉等于让客户端看到一次没有参数的工具调用。顺序仍是调用顺序。
        for (block, tool) in std::mem::take(&mut self.tools) {
            self.flush_tool(&mut out, block, tool);
        }
        self.close(&mut out);
        out.push(json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": self.stop_reason(),
                "stop_sequence": self.stopped_by,
            },
            "usage": self.usage_json(),
        }));
        out.push(json!({"type": "message_stop"}));
        out
    }

    /// 流中途失败：Anthropic 用 `error` 事件终止，不发 `message_delta` / `message_stop`。
    pub fn fail(&mut self, error: &crate::Error) -> Value {
        self.finished = true;
        let error = ApiError::from_error(error);
        json!({
            "type": "error",
            "error": {"type": error.kind, "message": error.message},
        })
    }

    fn stop_reason(&self) -> &'static str {
        match self.finish {
            Some(FinishReason::Length) => "max_tokens",
            Some(FinishReason::ToolUse) => "tool_use",
            _ if self.stopped_by.is_some() => "stop_sequence",
            _ => "end_turn",
        }
    }

    fn usage_json(&self) -> Value {
        let cache_read = self.usage.cache_read_tokens.unwrap_or(0);
        let cache_write = self.usage.cache_write_tokens.unwrap_or(0);
        let input = self
            .usage
            .context_input_tokens
            .map(|total| total.saturating_sub(cache_read).saturating_sub(cache_write))
            .or(self.usage.input_tokens)
            .unwrap_or(0);
        let mut usage = json!({
            "input_tokens": input,
            "output_tokens": self.usage.output_tokens.unwrap_or(0),
        });
        if self.usage.cache_read_tokens.is_some() {
            usage["cache_read_input_tokens"] = json!(cache_read);
        }
        if self.usage.cache_write_tokens.is_some() {
            usage["cache_creation_input_tokens"] = json!(cache_write);
        }
        usage
    }

    fn ensure_open(&mut self, out: &mut Vec<Value>, kind: BlockKind, provider_index: usize) {
        if self.open.as_ref().is_some_and(|block| block.kind == kind) {
            return;
        }
        self.open_block(out, kind, provider_index);
    }

    fn open_block(
        &mut self,
        out: &mut Vec<Value>,
        kind: BlockKind,
        provider_index: usize,
    ) -> usize {
        self.close(out);
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(OpenBlock {
            index,
            kind,
            provider_index,
        });
        let content_block = match kind {
            BlockKind::Text => json!({"type": "text", "text": ""}),
            BlockKind::Thinking => json!({"type": "thinking", "thinking": ""}),
        };
        out.push(json!({
            "type": "content_block_start",
            "index": index,
            "content_block": content_block,
        }));
        index
    }

    /// 一个工具调用收尾：关掉打开中的块，再写完整的三段（start → delta → stop）。
    ///
    /// 参数一次性交付而不是边到边转：并行工具的参数在流里可能交错，而协议一次只允许
    /// 一个打开的内容块，边转必然给交错的那一方丢字节（以前非流式会得到 `input: {}`）。
    fn flush_tool(&mut self, out: &mut Vec<Value>, block: usize, tool: PendingTool) {
        self.close(out);
        out.push(json!({
            "type": "content_block_start",
            "index": block,
            "content_block": {
                "type": "tool_use",
                "id": tool.call_id,
                "name": tool.name,
                "input": {},
            },
        }));
        if !tool.arguments.is_empty() {
            out.push(json!({
                "type": "content_block_delta",
                "index": block,
                "delta": {"type": "input_json_delta", "partial_json": tool.arguments},
            }));
        }
        out.push(json!({
            "type": "content_block_stop",
            "index": block,
        }));
    }

    /// 按块下标（= 调用顺序）依次交付已经收完参数的工具。
    ///
    /// 后开始的工具可能先收完（并行工具交错完成）；先交付它会让客户端收到
    /// `index: 1` 的块头却没有 `index: 0`，按数组累积的 SDK 会错位。所以只要
    /// 前一个工具还没收完，后面收完的也先压着。
    fn flush_finished_tools(&mut self, out: &mut Vec<Value>) {
        while self.tools.values().next().is_some_and(|tool| tool.ended) {
            let Some(block) = self.tools.keys().next().copied() else {
                return;
            };
            if let Some(tool) = self.tools.remove(&block) {
                self.flush_tool(out, block, tool);
            }
        }
    }

    fn pending_tool_mut(&mut self, provider_index: usize) -> Option<&mut PendingTool> {
        self.tools
            .values_mut()
            .find(|tool| tool.provider_index == provider_index)
    }

    fn delta(&self, out: &mut Vec<Value>, kind: BlockKind, provider_index: usize, delta: Value) {
        let Some(block) = self.open.as_ref() else {
            return;
        };
        if block.kind != kind || block.provider_index != provider_index {
            return;
        }
        out.push(json!({
            "type": "content_block_delta",
            "index": block.index,
            "delta": delta,
        }));
    }

    fn close_kind(&mut self, out: &mut Vec<Value>, kind: BlockKind) {
        if self.open.as_ref().is_some_and(|block| block.kind == kind) {
            self.close(out);
        }
    }

    fn close(&mut self, out: &mut Vec<Value>) {
        let Some(block) = self.open.take() else {
            return;
        };
        if block.kind == BlockKind::Text {
            let rest = self.text.flush();
            if !rest.is_empty() {
                out.push(json!({
                    "type": "content_block_delta",
                    "index": block.index,
                    "delta": {"type": "text_delta", "text": rest},
                }));
            }
        }
        out.push(json!({
            "type": "content_block_stop",
            "index": block.index,
        }));
    }

    fn redacted_blocks(&mut self, out: &mut Vec<Value>, state: ProviderReplayState) {
        if state.provider_kind != "anthropic" {
            return;
        }
        let Some(blocks) = state.value.get("blocks").and_then(Value::as_array) else {
            return;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("redacted_thinking") {
                continue;
            }
            let index = self.next_index;
            self.next_index += 1;
            self.close(out);
            out.push(json!({
                "type": "content_block_start",
                "index": index,
                "content_block": block,
            }));
            out.push(json!({
                "type": "content_block_stop",
                "index": index,
            }));
        }
    }
}

/// 把 [`ResponseTranslator`] 产出的事件折叠成一条完整的 Anthropic 消息。
///
/// 非流式响应与流式响应必须同源：停止序列、思考签名、工具参数拼接都只在 translator
/// 里实现一次，这里只负责「事件 → 最终文档」，不重新解释 provider 事件。
#[derive(Default)]
pub struct MessageAssembler {
    message: Value,
    blocks: BTreeMap<usize, Value>,
    partial_json: HashMap<usize, String>,
}

impl MessageAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, event: &Value) -> Result<(), ApiError> {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.message = event.get("message").cloned().unwrap_or(Value::Null);
                self.message["content"] = Value::Array(Vec::new());
            }
            Some("content_block_start") => {
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                self.blocks.insert(block_index(event), block);
            }
            Some("content_block_delta") => {
                let index = block_index(event);
                let Some(block) = self.blocks.get_mut(&index) else {
                    return Ok(());
                };
                let Some(delta) = event.get("delta") else {
                    return Ok(());
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => append(block, "text", delta.get("text")),
                    Some("thinking_delta") => append(block, "thinking", delta.get("thinking")),
                    Some("signature_delta") => {
                        if let Some(signature) = delta.get("signature") {
                            block["signature"] = signature.clone();
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            self.partial_json
                                .entry(index)
                                .or_default()
                                .push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = block_index(event);
                let Some(partial) = self.partial_json.remove(&index) else {
                    return Ok(());
                };
                if let Some(block) = self.blocks.get_mut(&index) {
                    // 参数解析失败不能折成 `{}`：那会把"工具收到了空参数"当成成功交给
                    // 客户端，工具会带着错误的理解执行下去。上游协议错误 -> 502。
                    block["input"] = parse_tool_arguments(&partial)?;
                }
            }
            Some("message_delta") => {
                if let Some(delta) = event.get("delta") {
                    if let Some(stop_reason) = delta.get("stop_reason") {
                        self.message["stop_reason"] = stop_reason.clone();
                    }
                    if let Some(stop_sequence) = delta.get("stop_sequence") {
                        self.message["stop_sequence"] = stop_sequence.clone();
                    }
                }
                if let Some(usage) = event.get("usage") {
                    self.message["usage"] = usage.clone();
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Value, ApiError> {
        let mut message = self.message;
        if !message.is_object() {
            // 上游没发 message_start 时给客户端一个形状完整的空消息，而不是 null。
            message = json!({
                "id": "",
                "type": "message",
                "role": "assistant",
                "model": "",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            });
        }
        message["content"] = Value::Array(self.blocks.into_values().collect());
        if message.get("stop_reason").is_none() {
            message["stop_reason"] = Value::Null;
        }
        if message.get("stop_sequence").is_none() {
            message["stop_sequence"] = Value::Null;
        }
        Ok(message)
    }
}

/// 工具参数是一段完整的 JSON 对象；解析不出来或不是对象都是上游协议错误。
fn parse_tool_arguments(partial: &str) -> Result<Value, ApiError> {
    let arguments: Value = serde_json::from_str(partial).map_err(|error| {
        ApiError::from_error(&crate::Error::Provider(format!(
            "tool call arguments are not valid JSON: {error}"
        )))
    })?;
    if !arguments.is_object() {
        return Err(ApiError::from_error(&crate::Error::Provider(
            "tool call arguments must be a JSON object".into(),
        )));
    }
    Ok(arguments)
}

fn block_index(event: &Value) -> usize {
    event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or(0)
}

fn append(block: &mut Value, field: &str, value: Option<&Value>) {
    let Some(text) = value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    else {
        return;
    };
    let mut combined = block
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    combined.push_str(text);
    block[field] = Value::String(combined);
}

/// 逐字符缓冲，保证停止序列跨 `text_delta` 也能命中，同时不把半截序列发给客户端。
struct StopFilter {
    sequences: Vec<String>,
    buffer: String,
    stopped: Option<String>,
}

impl StopFilter {
    fn new(sequences: Vec<String>) -> Self {
        Self {
            sequences,
            buffer: String::new(),
            stopped: None,
        }
    }

    fn push(&mut self, delta: &str) -> String {
        if self.stopped.is_some() {
            return String::new();
        }
        self.buffer.push_str(delta);
        if let Some((position, sequence)) = earliest_match(&self.buffer, &self.sequences) {
            let emitted = self.buffer[..position].to_owned();
            self.stopped = Some(sequence);
            self.buffer.clear();
            return emitted;
        }
        let keep = pending_suffix(&self.buffer, &self.sequences);
        let split = self.buffer.len() - keep;
        let emitted = self.buffer[..split].to_owned();
        self.buffer.drain(..split);
        emitted
    }

    fn flush(&mut self) -> String {
        std::mem::take(&mut self.buffer)
    }
}

fn earliest_match(text: &str, sequences: &[String]) -> Option<(usize, String)> {
    let mut best: Option<(usize, &str)> = None;
    for sequence in sequences {
        if let Some(position) = text.find(sequence.as_str()) {
            if best.is_none_or(|(current, _)| position < current) {
                best = Some((position, sequence));
            }
        }
    }
    best.map(|(position, sequence)| (position, sequence.to_owned()))
}

/// `text` 末尾有多长的后缀是某条停止序列的前缀；这段先不发。
fn pending_suffix(text: &str, sequences: &[String]) -> usize {
    let mut keep = 0;
    for (start, _) in text.char_indices() {
        let suffix = &text[start..];
        if suffix.is_empty() {
            continue;
        }
        if sequences
            .iter()
            .any(|sequence| sequence.starts_with(suffix))
        {
            keep = suffix.len();
            break;
        }
    }
    keep
}

pub fn sse_bytes(value: &Value) -> Bytes {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    format!("event: {kind}\ndata: {value}\n\n").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::ClaudeSettings;
    use crate::model::{ModelConfigInput, ModelType};

    async fn fixture() -> (tempfile::TempDir, Store, ClaudeSettings) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("claude-messages.db").display()
        ))
        .await
        .unwrap();
        let model = store
            .create_model(&ModelConfigInput {
                sort_order: 0,
                display_name: "Claude".into(),
                group_name: None,
                model_type: ModelType::Anthropic,
                base_url: "https://api.anthropic.com".into(),
                use_full_url: false,
                api_key: "key".into(),
                tooltip_data: "Claude".into(),
                model_id: "claude-sonnet-4".into(),
                reasoning_effort: None,
                openai_endpoint: String::new(),
                openai_extra_params_enabled: false,
                openai_extra_params: json!({}),
                custom_headers_enabled: false,
                custom_headers: json!({}),
                anthropic_extra_params_enabled: false,
                anthropic_extra_params: json!({}),
                context_window_tokens: Some(200_000),
                max_completion_tokens: None,
                anthropic_max_tokens: Some(8_192),
                anthropic_thinking_effort: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap();
        let settings = ClaudeSettings {
            enabled: true,
            default_model_hash: Some(model.model_hash.clone()),
            sonnet_model_hash: Some(model.model_hash),
            ..ClaudeSettings::default()
        };
        store.set_claude_settings(settings.clone()).await.unwrap();
        (directory, store, settings)
    }

    fn clients(store: &Store) -> NetworkClients {
        NetworkClients::new(store.clone())
    }

    #[tokio::test]
    async fn system_blocks_tools_and_stop_sequences_map_to_the_shared_request() {
        let (_directory, store, settings) = fixture().await;
        let body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 4_096,
            "stream": true,
            "system": [{"type": "text", "text": "first", "cache_control": {"type": "ephemeral"}},
                       {"type": "text", "text": "second"}],
            "stop_sequences": ["STOP", ""],
            "tools": [{"name": "Bash", "description": "run", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "hello"}],
        });
        let prepared = prepare(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();
        let request = &prepared.invocation.request;
        assert_eq!(request.prompt.instructions, "first\nsecond");
        assert_eq!(request.prompt.tools[0].name, "Bash");
        assert_eq!(request.model.stop_sequences, vec!["STOP"]);
        assert_eq!(request.model.max_output_tokens, Some(4_096));
        assert_eq!(
            prepared.invocation.client_source,
            crate::model::CallClient::Claude
        );
        assert_eq!(prepared.requested_model, "claude-sonnet-4-5");
    }

    #[tokio::test]
    async fn tool_round_trips_keep_their_ids_arguments_and_error_flag() {
        let (_directory, store, settings) = fixture().await;
        let body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1_024,
            "messages": [
                {"role": "user", "content": "list files"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "running"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true,
                     "content": [{"type": "text", "text": "exit 1"}]},
                ]},
            ],
        });
        let prepared = prepare(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();
        let history = &prepared.invocation.request.history;
        assert_eq!(history.len(), 3);
        let ProjectedContent::Assistant { text, calls, .. } = &history[1].content else {
            panic!("assistant message expected");
        };
        assert_eq!(text, "running");
        assert_eq!(calls[0].call_id, "toolu_1");
        assert_eq!(calls[0].arguments, json!({"command": "ls"}));
        let ProjectedContent::ToolResult(result) = &history[2].content else {
            panic!("tool result expected");
        };
        assert_eq!(result.call_id, "toolu_1");
        assert!(result.is_error);
        assert_eq!(result.content, "exit 1");
        assert_eq!(result.provider_parts.len(), 1);
    }

    #[tokio::test]
    async fn signed_thinking_becomes_replay_state_and_unsigned_thinking_is_only_text() {
        let (_directory, store, settings) = fixture().await;
        let body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1_024,
            "thinking": {"type": "enabled", "budget_tokens": 2_048},
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "plan", "signature": "sig-1"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}},
                ]},
            ],
        });
        let prepared = prepare(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();
        let request = &prepared.invocation.request;
        assert_eq!(request.model.reasoning.thinking_budget_tokens, Some(2_048));
        assert!(request.model.reasoning.is_enabled());
        let ProjectedContent::Assistant {
            thinking,
            replay_state,
            ..
        } = &request.history[1].content
        else {
            panic!("assistant message expected");
        };
        assert_eq!(thinking, "plan");
        let replay = replay_state.as_ref().expect("signature must be replayed");
        assert_eq!(replay.provider_kind, "anthropic");
        assert_eq!(replay.value["blocks"][0]["signature"], "sig-1");
    }

    #[tokio::test]
    async fn base64_images_are_decoded_and_url_images_are_fetched() {
        let (_directory, store, settings) = fixture().await;
        let body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1_024,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}},
            ]}],
        });
        let prepared = prepare(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();
        let ProjectedContent::Parts(parts) = &prepared.invocation.request.history[0].content else {
            panic!("user parts expected");
        };
        let ContentPart::Image { mime_type, data } = &parts[0] else {
            panic!("image expected");
        };
        assert_eq!(mime_type, "image/png");
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn url_images_are_downloaded_into_the_canonical_image_part() {
        use axum::{http::header, routing::get, Router};
        let (_directory, store, settings) = fixture().await;
        let app = Router::new().route(
            "/image.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], "png-bytes") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url = String::from("http://") + &address.to_string() + "/image.png";
        let body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1_024,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "url", "url": url}},
            ]}],
        });
        let prepared = prepare(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();
        let ProjectedContent::Parts(parts) = &prepared.invocation.request.history[0].content else {
            panic!("user parts expected");
        };
        let ContentPart::Image { mime_type, data } = &parts[0] else {
            panic!("image expected");
        };
        assert_eq!(mime_type, "image/png");
        assert_eq!(data, b"png-bytes");
    }

    /// `/count_tokens` 只需要一个量级：URL 图片不能真的下载（20 秒超时会变成一次
    /// 网络副作用），按共享估算器的固定价格计入即可。
    #[tokio::test]
    async fn count_tokens_estimates_url_images_without_downloading_them() {
        use axum::{http::header, routing::get, Router};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let (_directory, store, settings) = fixture().await;
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let app = Router::new().route(
            "/image.png",
            get(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    ([(header::CONTENT_TYPE, "image/png")], "png-bytes")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url = String::from("http://") + &address.to_string() + "/image.png";
        let body = json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "url", "url": url}},
                {"type": "text", "text": "what is this?"},
            ]}],
        });

        let tokens = count_tokens(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();

        assert!(tokens > 0);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "count_tokens 不能下载 URL 图片"
        );
    }

    #[tokio::test]
    async fn a_missing_max_tokens_is_rejected_for_messages_but_not_for_count_tokens() {
        let (_directory, store, settings) = fixture().await;
        let body = json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}],
        });
        let error = prepare(&store, &settings, &body, &clients(&store))
            .await
            .err()
            .expect("max_tokens is required by /v1/messages");
        assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);

        let tokens = count_tokens(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();
        assert!(tokens > 0, "估算不能是 0");
    }

    #[tokio::test]
    async fn the_stream_flag_defaults_to_false_and_is_carried_to_the_handler() {
        let (_directory, store, settings) = fixture().await;
        let mut body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hello"}],
        });
        let prepared = prepare(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();
        assert!(!prepared.stream, "缺省按 Anthropic 的非流式处理");

        body["stream"] = json!(true);
        let prepared = prepare(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();
        assert!(prepared.stream);

        body["stream"] = json!(false);
        let prepared = prepare(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();
        assert!(!prepared.stream);
    }

    async fn project(body: Value) -> (Vec<ProjectedMessage>, String) {
        let (_directory, store, settings) = fixture().await;
        let prepared = prepare(&store, &settings, &body, &clients(&store))
            .await
            .unwrap();
        (
            prepared.invocation.request.history,
            prepared.invocation.request.prompt.instructions,
        )
    }

    fn user_text(message: &ProjectedMessage) -> String {
        match &message.content {
            ProjectedContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    ContentPart::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }

    #[tokio::test]
    async fn a_leading_system_run_joins_the_top_level_instructions() {
        let (history, instructions) = project(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 100,
            "system": "top",
            "messages": [
                {"role": "system", "content": "lead one"},
                {"role": "system", "content": [
                    {"type": "text", "text": "lead two", "cache_control": {"type": "ephemeral"}},
                ]},
                {"role": "user", "content": "hi"},
            ],
        }))
        .await;
        assert_eq!(instructions, "top\n\nlead one\n\nlead two");
        assert_eq!(history.len(), 1, "前导 system 不是对话消息");
        assert_eq!(history[0].role, Role::User);
    }

    #[tokio::test]
    async fn a_mid_conversation_system_message_becomes_a_user_note() {
        let (history, _) = project(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "ok"},
                {"role": "system", "content": "be careful"},
                {"role": "user", "content": "next"},
            ],
        }))
        .await;
        let roles = history.iter().map(|m| m.role.clone()).collect::<Vec<_>>();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::User, Role::User]
        );
        assert_eq!(
            user_text(&history[2]),
            String::from(OPERATOR_NOTE) + "\nbe careful"
        );
        assert_eq!(user_text(&history[3]), "next");
    }

    #[tokio::test]
    async fn a_system_message_between_tool_use_and_tool_result_lands_after_the_result() {
        let (history, _) = project(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "list files"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}},
                ]},
                {"role": "system", "content": "reminder"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt"},
                ]},
            ],
        }))
        .await;
        let roles = history.iter().map(|m| m.role.clone()).collect::<Vec<_>>();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::Tool, Role::User],
            "system 必须排在 tool_result 之后"
        );
        let ProjectedContent::Assistant { calls, .. } = &history[1].content else {
            panic!("assistant tool call expected");
        };
        assert_eq!(calls[0].call_id, "toolu_1");
        let ProjectedContent::ToolResult(result) = &history[2].content else {
            panic!("tool result expected");
        };
        assert_eq!(result.call_id, "toolu_1");
        assert_eq!(
            user_text(&history[3]),
            String::from(OPERATOR_NOTE) + "\nreminder"
        );
    }

    /// `message_start` 先给估算值（客户端一开始就要显示上下文占用），
    /// `message_delta` 再用上游报的权威用量覆盖它。
    #[test]
    fn message_start_carries_the_estimate_and_message_delta_the_authority() {
        let mut translator = ResponseTranslator::new("msg_1", "m", Vec::new(), 1_234);
        assert_eq!(
            translator.start()["message"]["usage"]["input_tokens"],
            1_234
        );
        translator.push(ModelEvent::Usage(Usage {
            input_tokens: Some(7),
            context_input_tokens: Some(7),
            output_tokens: Some(2),
            total_tokens: Some(9),
            ..Default::default()
        }));
        let finish = translator.finish();
        assert_eq!(finish[0]["delta"]["stop_reason"], "end_turn");
        assert_eq!(
            finish[0]["usage"]["input_tokens"], 7,
            "权威用量必须覆盖估算值"
        );
        assert_eq!(finish[0]["usage"]["output_tokens"], 2);
    }

    /// 停止序列、思考签名与工具参数拼接都只在 translator 里实现一次；非流式折叠必须
    /// 得到与流式完全相同的语义。
    fn assemble(events: Vec<ModelEvent>, stop_sequences: Vec<String>) -> Value {
        let mut translator =
            ResponseTranslator::new("msg_1", "claude-sonnet-4-5", stop_sequences, 0);
        let mut assembler = MessageAssembler::new();
        assembler.push(&translator.start()).unwrap();
        for event in events {
            for value in translator.push(event) {
                assembler.push(&value).unwrap();
            }
        }
        for value in translator.finish() {
            assembler.push(&value).unwrap();
        }
        assembler.finish().unwrap()
    }

    #[test]
    fn a_non_streaming_text_response_is_one_complete_message() {
        let message = assemble(
            vec![
                ModelEvent::Start {
                    model_call_id: "call".into(),
                },
                ModelEvent::TextStart,
                ModelEvent::TextDelta("hel".into()),
                ModelEvent::TextDelta("lo".into()),
                ModelEvent::TextEnd,
                ModelEvent::Usage(Usage {
                    input_tokens: Some(7),
                    context_input_tokens: Some(30),
                    output_tokens: Some(2),
                    total_tokens: Some(32),
                    cache_read_tokens: Some(20),
                    cache_write_tokens: Some(3),
                    reasoning_tokens: None,
                }),
                ModelEvent::Done(FinishReason::Stop),
            ],
            Vec::new(),
        );
        assert_eq!(message["type"], "message");
        assert_eq!(message["id"], "msg_1");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["model"], "claude-sonnet-4-5");
        assert_eq!(message["content"][0]["type"], "text");
        assert_eq!(message["content"][0]["text"], "hello");
        assert_eq!(message["stop_reason"], "end_turn");
        assert!(message["stop_sequence"].is_null());
        assert_eq!(message["usage"]["input_tokens"], 7);
        assert_eq!(message["usage"]["output_tokens"], 2);
        assert_eq!(message["usage"]["cache_read_input_tokens"], 20);
        assert_eq!(message["usage"]["cache_creation_input_tokens"], 3);
    }

    #[test]
    fn a_non_streaming_tool_response_keeps_signature_and_parsed_input() {
        let message = assemble(
            vec![
                ModelEvent::Start {
                    model_call_id: "call".into(),
                },
                ModelEvent::ThinkingStart,
                ModelEvent::ThinkingDelta("plan".into()),
                ModelEvent::ThinkingSignature("sig-1".into()),
                ModelEvent::ThinkingEnd,
                ModelEvent::ToolCallStart {
                    index: 0,
                    call_id: "toolu_1".into(),
                    name: "Bash".into(),
                },
                ModelEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: "{\"command\":".into(),
                },
                ModelEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: "\"ls\"}".into(),
                },
                ModelEvent::ToolCallEnd { index: 0 },
                ModelEvent::Done(FinishReason::ToolUse),
            ],
            Vec::new(),
        );
        assert_eq!(message["content"][0]["type"], "thinking");
        assert_eq!(message["content"][0]["thinking"], "plan");
        assert_eq!(message["content"][0]["signature"], "sig-1");
        assert_eq!(message["content"][1]["type"], "tool_use");
        assert_eq!(message["content"][1]["id"], "toolu_1");
        assert_eq!(message["content"][1]["name"], "Bash");
        assert_eq!(message["content"][1]["input"], json!({"command": "ls"}));
        assert_eq!(message["stop_reason"], "tool_use");
    }

    #[test]
    fn a_non_streaming_stop_sequence_sets_stop_sequence_reason() {
        let message = assemble(
            vec![
                ModelEvent::TextStart,
                ModelEvent::TextDelta("hello ST".into()),
                ModelEvent::TextDelta("OP ignored".into()),
                ModelEvent::Done(FinishReason::Stop),
            ],
            vec!["STOP".into()],
        );
        assert_eq!(message["content"][0]["text"], "hello ");
        assert_eq!(message["stop_reason"], "stop_sequence");
        assert_eq!(message["stop_sequence"], "STOP");
    }

    /// 上游报的停止序列是权威答案：模型在序列前就收尾时，本地过滤器在文本里根本
    /// 看不到序列，以前只能回 `end_turn` + `stop_sequence: null`。
    #[test]
    fn the_upstream_stop_sequence_sets_the_reason_and_the_matched_text() {
        let message = assemble(
            vec![
                ModelEvent::TextStart,
                ModelEvent::TextDelta("hello ".into()),
                ModelEvent::StopSequence("STOP".into()),
                ModelEvent::Done(FinishReason::Stop),
            ],
            vec!["STOP".into()],
        );
        assert_eq!(message["content"][0]["text"], "hello ");
        assert_eq!(message["stop_reason"], "stop_sequence");
        assert_eq!(message["stop_sequence"], "STOP");
    }

    /// 两个工具的参数在流里交错到达：每个工具都必须拿到完整的三段序列，参数不能
    /// 因为"当前打开的是另一个块"被丢掉。
    fn interleaved_tool_events() -> Vec<ModelEvent> {
        vec![
            ModelEvent::ToolCallStart {
                index: 0,
                call_id: "toolu_a".into(),
                name: "Read".into(),
            },
            ModelEvent::ToolCallStart {
                index: 1,
                call_id: "toolu_b".into(),
                name: "Bash".into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"path\":".into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index: 1,
                delta: "{\"command\":\"ls ".into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "\"a.txt\"}".into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index: 1,
                delta: "-la\"}".into(),
            },
            ModelEvent::ToolCallEnd { index: 0 },
            ModelEvent::ToolCallEnd { index: 1 },
            ModelEvent::Done(FinishReason::ToolUse),
        ]
    }

    #[test]
    fn interleaved_tool_arguments_are_buffered_per_index_in_the_stream() {
        let output = collect(interleaved_tool_events());
        let types = output
            .iter()
            .map(|value| value["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let blocks = output
            .iter()
            .filter(|value| value["type"] == "content_block_start")
            .map(|value| {
                (
                    value["content_block"]["id"].as_str().unwrap().to_owned(),
                    value["content_block"]["name"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            blocks,
            vec![
                ("toolu_a".to_string(), "Read".to_string()),
                ("toolu_b".to_string(), "Bash".to_string()),
            ],
            "每个工具都要有自己的块头，顺序与调用顺序一致"
        );
        let arguments = output
            .iter()
            .filter(|value| value["delta"]["type"] == "input_json_delta")
            .map(|value| value["delta"]["partial_json"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            vec![
                r#"{"path":"a.txt"}"#.to_string(),
                r#"{"command":"ls -la"}"#.to_string()
            ],
            "交错到达的参数按 tool index 归位"
        );
    }

    /// 后开始的工具先收完时不能抢跑：否则客户端会先收到 `index: 1` 的块头，
    /// 按数组累积的内容与 `index: 0` 的后续事件都会错位。
    #[test]
    fn a_later_tool_that_finishes_first_waits_for_block_order() {
        let output = collect(vec![
            ModelEvent::ToolCallStart {
                index: 0,
                call_id: "toolu_a".into(),
                name: "Read".into(),
            },
            ModelEvent::ToolCallStart {
                index: 1,
                call_id: "toolu_b".into(),
                name: "Bash".into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index: 1,
                delta: "{\"command\":\"ls\"}".into(),
            },
            ModelEvent::ToolCallEnd { index: 1 },
            ModelEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"path\":\"a.txt\"}".into(),
            },
            ModelEvent::ToolCallEnd { index: 0 },
            ModelEvent::Done(FinishReason::ToolUse),
        ]);
        let steps = output
            .iter()
            .filter(|value| value["type"] != "message_start")
            .map(|value| {
                (
                    value["type"].as_str().unwrap().to_owned(),
                    value["index"].as_u64(),
                    value["content_block"]["id"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| value["delta"]["partial_json"].as_str().map(str::to_owned)),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            steps,
            vec![
                (
                    "content_block_start".into(),
                    Some(0),
                    Some("toolu_a".into())
                ),
                (
                    "content_block_delta".into(),
                    Some(0),
                    Some("{\"path\":\"a.txt\"}".into())
                ),
                ("content_block_stop".into(), Some(0), None),
                (
                    "content_block_start".into(),
                    Some(1),
                    Some("toolu_b".into())
                ),
                (
                    "content_block_delta".into(),
                    Some(1),
                    Some("{\"command\":\"ls\"}".into())
                ),
                ("content_block_stop".into(), Some(1), None),
                ("message_delta".into(), None, None),
                ("message_stop".into(), None, None),
            ]
        );
    }

    #[test]
    fn interleaved_tool_arguments_survive_the_non_streaming_fold() {
        let message = assemble(interleaved_tool_events(), Vec::new());
        assert_eq!(message["content"][0]["type"], "tool_use");
        assert_eq!(message["content"][0]["id"], "toolu_a");
        assert_eq!(message["content"][0]["input"], json!({"path": "a.txt"}));
        assert_eq!(message["content"][1]["type"], "tool_use");
        assert_eq!(message["content"][1]["id"], "toolu_b");
        assert_eq!(message["content"][1]["input"], json!({"command": "ls -la"}));
        assert_eq!(message["stop_reason"], "tool_use");
    }

    /// 参数拼不成 JSON 是上游协议错误：非流式必须报错（502 可重试），而不是把
    /// `{}` 当成工具真的收到了空参数发给客户端。
    #[test]
    fn unparsable_tool_arguments_fail_the_non_streaming_fold() {
        let mut translator = ResponseTranslator::new("msg_1", "m", Vec::new(), 0);
        let mut assembler = MessageAssembler::new();
        assembler.push(&translator.start()).unwrap();
        let events = [
            ModelEvent::ToolCallStart {
                index: 0,
                call_id: "toolu_a".into(),
                name: "Read".into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"path\":".into(),
            },
            ModelEvent::ToolCallEnd { index: 0 },
            ModelEvent::Done(FinishReason::ToolUse),
        ];
        let mut failure = None;
        for event in events {
            for value in translator.push(event) {
                if let Err(error) = assembler.push(&value) {
                    failure = Some(error);
                }
            }
        }
        let error = failure.expect("截断的参数必须让折叠失败");
        assert_eq!(error.status, axum::http::StatusCode::BAD_GATEWAY);
        assert!(error.should_retry);
    }

    fn collect(events: Vec<ModelEvent>) -> Vec<Value> {
        let mut translator = ResponseTranslator::new("msg_1", "claude-sonnet-4-5", Vec::new(), 0);
        let mut output = vec![translator.start()];
        for event in events {
            output.extend(translator.push(event));
        }
        output.extend(translator.finish());
        output
    }

    #[test]
    fn the_sse_sequence_is_well_formed_for_text_thinking_and_tools() {
        let output = collect(vec![
            ModelEvent::Start {
                model_call_id: "call".into(),
            },
            ModelEvent::ThinkingStart,
            ModelEvent::ThinkingDelta("plan".into()),
            ModelEvent::ThinkingSignature("sig".into()),
            ModelEvent::ThinkingEnd,
            ModelEvent::TextStart,
            ModelEvent::TextDelta("hello".into()),
            ModelEvent::TextEnd,
            ModelEvent::ToolCallStart {
                index: 0,
                call_id: "toolu_1".into(),
                name: "Bash".into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"command\":\"ls\"}".into(),
            },
            ModelEvent::ToolCallEnd { index: 0 },
            ModelEvent::Usage(Usage {
                input_tokens: Some(10),
                context_input_tokens: Some(30),
                output_tokens: Some(5),
                total_tokens: Some(35),
                cache_read_tokens: Some(20),
                cache_write_tokens: Some(0),
                reasoning_tokens: None,
            }),
            ModelEvent::Done(FinishReason::ToolUse),
        ]);

        let types = output
            .iter()
            .map(|value| value["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(output[3]["delta"]["type"], "signature_delta");
        assert_eq!(output[4]["index"], 0);
        assert_eq!(output[6]["index"], 1);
        assert_eq!(output[8]["content_block"]["type"], "tool_use");
        assert_eq!(output[9]["delta"]["type"], "input_json_delta");
        assert_eq!(output[11]["delta"]["stop_reason"], "tool_use");
        assert_eq!(output[11]["usage"]["input_tokens"], 10);
        assert_eq!(output[11]["usage"]["cache_read_input_tokens"], 20);
    }

    #[test]
    fn a_stop_sequence_split_across_deltas_truncates_before_it() {
        let mut translator = ResponseTranslator::new("msg_1", "m", vec!["STOP".into()], 0);
        let mut output = vec![translator.start()];
        for event in [
            ModelEvent::TextStart,
            ModelEvent::TextDelta("hello ST".into()),
            ModelEvent::TextDelta("OP ignored".into()),
            ModelEvent::Done(FinishReason::Stop),
        ] {
            output.extend(translator.push(event));
        }
        output.extend(translator.finish());
        let text = output
            .iter()
            .filter(|value| value["delta"]["type"] == "text_delta")
            .map(|value| value["delta"]["text"].as_str().unwrap())
            .collect::<String>();
        assert_eq!(text, "hello ");
        assert_eq!(output.last().unwrap()["type"], "message_stop");
        assert_eq!(
            output[output.len() - 2]["delta"]["stop_reason"],
            "stop_sequence"
        );
        assert_eq!(output[output.len() - 2]["delta"]["stop_sequence"], "STOP");
    }

    #[test]
    fn a_partial_stop_prefix_is_flushed_when_the_response_ends() {
        let output = collect(vec![
            ModelEvent::TextStart,
            ModelEvent::TextDelta("almost ST".into()),
            ModelEvent::TextEnd,
            ModelEvent::Done(FinishReason::Stop),
        ]);
        let text = output
            .iter()
            .filter(|value| value["delta"]["type"] == "text_delta")
            .map(|value| value["delta"]["text"].as_str().unwrap())
            .collect::<String>();
        assert_eq!(text, "almost ST");
    }
}
