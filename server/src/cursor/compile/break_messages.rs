//! Compiles runtime information that interrupts the current cycle before appending.
use std::collections::BTreeMap;

use chrono::{Offset, Utc};
use chrono_tz::Tz;

use crate::{
    cursor::{
        prompting::{Mode, PromptCompiler},
        protocol::proto::agent::v1 as pb,
        services::blob_sync::BlobSynchronizer,
    },
    model::{CanonicalMessage, ContentPart, MessageContent, Origin, Role},
    store::BlobId,
    Error, Result,
};

use super::{context, images};

pub(crate) enum RuntimeAction {
    Inject(pb::InjectContextAction),
    UserMessage(pb::UserMessageAction),
}

pub(crate) async fn compile_user_message_action(
    action: &pb::UserMessageAction,
    current_mode: i32,
    compiler: &PromptCompiler,
    blobs: &BlobSynchronizer,
) -> Result<CanonicalMessage> {
    let user = action
        .user_message
        .as_ref()
        .ok_or_else(|| Error::Protocol("Cursor user message action has no UserMessage".into()))?;
    if user.message_id.is_empty() {
        return Err(Error::Protocol(
            "Cursor user message action has no message_id".into(),
        ));
    }
    let mode = if user.mode == pb::AgentMode::Unspecified as i32 {
        current_mode
    } else {
        user.mode
    };
    let mut action_context = action
        .prepend_user_messages
        .iter()
        .map(|message| message.text.trim())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    action_context.extend(
        user.subagent_system_reminder
            .iter()
            .filter(|text| !text.is_empty())
            .cloned(),
    );
    let empty_context = pb::RequestContext::default();
    compile(
        format!("user-message:{}", user.message_id),
        super::run::mode_from_proto(mode)?,
        user,
        action.request_context.as_ref().unwrap_or(&empty_context),
        &action_context.join("\n\n"),
        compiler,
        blobs,
    )
    .await
}

pub(crate) async fn compile_injection(
    injection: &pb::InjectContextAction,
    mode: i32,
    compiler: &PromptCompiler,
    blobs: &BlobSynchronizer,
) -> Result<CanonicalMessage> {
    if injection.injection_id.is_empty() {
        return Err(Error::Protocol(
            "InjectContextAction has no injection_id".into(),
        ));
    }
    let event_id = format!("inject-context:{}", injection.injection_id);
    match injection.payload.as_ref() {
        Some(pb::inject_context_action::Payload::UserContext(context)) => {
            let user = context.user_message.as_ref().ok_or_else(|| {
                Error::Protocol("InjectContextAction UserContext has no UserMessage".into())
            })?;
            if user.message_id.is_empty() {
                return Err(Error::Protocol(
                    "InjectContextAction UserMessage has no message_id".into(),
                ));
            }
            let empty_context = pb::RequestContext::default();
            compile(
                event_id,
                super::run::mode_from_proto(mode)?,
                user,
                context.request_context.as_ref().unwrap_or(&empty_context),
                "",
                compiler,
                blobs,
            )
            .await
        }
        Some(pb::inject_context_action::Payload::SystemContext(context)) => {
            Ok(CanonicalMessage {
                message_id: format!("runtime:{event_id}"),
                role: Role::User,
                origin: Origin::Runtime,
                content: MessageContent::Parts {
                    parts: vec![crate::model::ContentPart::Text {
                        text: format!(
                            "<system_context_injection>\n<producer>{}</producer>\n{}\n</system_context_injection>",
                            context.producer, context.content
                        ),
                    }],
                },
                runtime_event_id: Some(event_id),
            })
        }
        None => Err(Error::Protocol(
            "InjectContextAction has no payload".into(),
        )),
    }
}

pub async fn compile(
    event_id: String,
    mode: Mode,
    user: &pb::UserMessage,
    request_context: &pb::RequestContext,
    action_context: &str,
    compiler: &PromptCompiler,
    blobs: &BlobSynchronizer,
) -> Result<CanonicalMessage> {
    let timestamp = Time::now(
        request_context
            .env
            .as_ref()
            .map(|env| env.time_zone.as_str()),
    )
    .timestamp;
    compile_with_timestamp(
        event_id,
        mode,
        user,
        request_context,
        action_context,
        timestamp,
        compiler,
        blobs,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn user_event_id(
    input_id: &str,
    mode: Mode,
    user: &pb::UserMessage,
    request_context: &pb::RequestContext,
    action_context: &str,
    projected_request_context: Option<&MessageContent>,
    compiler: &PromptCompiler,
    blobs: &BlobSynchronizer,
) -> Result<String> {
    let runtime = compile_with_timestamp(
        "identity".into(),
        mode,
        user,
        request_context,
        action_context,
        String::new(),
        compiler,
        blobs,
    )
    .await?;
    let semantic = serde_json::to_vec(&(projected_request_context, runtime.content))?;
    Ok(format!(
        "{input_id}:{}",
        BlobId::digest(&semantic).to_base64()
    ))
}

#[allow(clippy::too_many_arguments)]
async fn compile_with_timestamp(
    event_id: String,
    mode: Mode,
    user: &pb::UserMessage,
    request_context: &pb::RequestContext,
    action_context: &str,
    timestamp: String,
    compiler: &PromptCompiler,
    blobs: &BlobSynchronizer,
) -> Result<CanonicalMessage> {
    let mut values = BTreeMap::from([
        ("OPEN_FILES", section(open_files(user))),
        (
            "SELECTED_CONTEXT",
            section(
                context::selected_context(user)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("<selected_context>\n{value}\n</selected_context>"))
                    .unwrap_or_default(),
            ),
        ),
        ("ACTION_CONTEXT", section(action_context.to_string())),
        ("TIMESTAMP", timestamp),
        ("USER_QUERY", user.text.clone()),
        ("DEBUG_SERVER_ENDPOINT", String::new()),
        ("DEBUG_LOG_PATH", String::new()),
        ("DEBUG_SESSION_ID", String::new()),
    ]);
    if let Some(debug) = &request_context.debug_mode_config {
        values.insert("DEBUG_SERVER_ENDPOINT", debug.server_endpoint.clone());
        values.insert("DEBUG_LOG_PATH", debug.log_path.clone());
        values.insert("DEBUG_SESSION_ID", debug.session_id.clone());
    }
    message(
        event_id,
        user,
        compiler.runtime_message(mode, &values)?,
        blobs,
    )
    .await
}

pub(super) fn compile_request_context(
    event_id: &str,
    request_context: &pb::RequestContext,
    history: &[CanonicalMessage],
) -> Result<Option<CanonicalMessage>> {
    let time = Time::now(
        request_context
            .env
            .as_ref()
            .map(|env| env.time_zone.as_str()),
    );
    let text = context::compile_context(request_context, &time.today);
    if text.is_empty() {
        return Ok(None);
    }
    let message = CanonicalMessage::text(
        format!("request-context:{event_id}"),
        Role::User,
        Origin::Prompt,
        text,
    );
    Ok(should_project_request_context(history, &message).then_some(message))
}

fn should_project_request_context(
    history: &[CanonicalMessage],
    current: &CanonicalMessage,
) -> bool {
    history
        .iter()
        .rev()
        .find(|message| message.message_id.starts_with("request-context:"))
        .is_none_or(|previous| previous.content != current.content)
}

pub async fn compile_background(
    event_id: String,
    user: &pb::UserMessage,
    request_context: &pb::RequestContext,
    action_context: &str,
    blobs: &BlobSynchronizer,
) -> Result<(CanonicalMessage, String)> {
    let timestamp = Time::now(
        request_context
            .env
            .as_ref()
            .map(|env| env.time_zone.as_str()),
    )
    .timestamp;
    let text = format!(
        "<timestamp>{timestamp}</timestamp>\n{}\n<user_query>{}</user_query>",
        action_context.trim(),
        user.text
    );
    let message = message(event_id, user, text.clone(), blobs).await?;
    Ok((message, text))
}

async fn message(
    event_id: String,
    user: &pb::UserMessage,
    text: String,
    blobs: &BlobSynchronizer,
) -> Result<CanonicalMessage> {
    Ok(CanonicalMessage {
        message_id: format!("runtime:{event_id}"),
        role: Role::User,
        origin: Origin::Runtime,
        content: MessageContent::Parts {
            parts: images::parts(user, text, blobs).await?,
        },
        runtime_event_id: Some(event_id),
    })
}

/// 服务端自己发起的唤醒消息：完成上下文 + 那句 follow-up 指令。
///
/// 与客户端发来完成通知时的形状一致（同样以 `background-completed:<identity>` 作事件
/// 身份），区别只是没有请求上下文可依，时间戳按 UTC 记。
pub(crate) fn wakeup_message(event_id: &str, context: &str) -> Result<CanonicalMessage> {
    let timestamp = Time::now(None).timestamp;
    let text = format!(
        "<timestamp>{timestamp}</timestamp>\n{}\n<user_query>{}</user_query>",
        context.trim(),
        super::insert_messages::FOLLOW_UP
    );
    Ok(CanonicalMessage {
        message_id: format!("runtime:{event_id}"),
        role: Role::User,
        origin: Origin::Runtime,
        content: MessageContent::Parts {
            parts: vec![ContentPart::Text { text }],
        },
        runtime_event_id: Some(event_id.to_owned()),
    })
}

fn section(value: String) -> String {
    let value = value.trim();
    if value.is_empty() {
        String::new()
    } else {
        format!("{value}\n\n")
    }
}

fn open_files(user: &pb::UserMessage) -> String {
    let Some(ide) = user
        .selected_context
        .as_ref()
        .and_then(|selected| selected.invocation_context.as_ref())
        .and_then(|invocation| invocation.data.as_ref())
        .and_then(|data| match data {
            pb::invocation_context::Data::IdeState(ide) => Some(ide),
            _ => None,
        })
    else {
        return String::new();
    };
    if ide.visible_files.is_empty() && ide.recently_viewed_files.is_empty() {
        return String::new();
    }

    let mut output = String::from("<open_and_recently_viewed_files>\n");
    if !ide.recently_viewed_files.is_empty() {
        output.push_str("Recently viewed files (recent at the top, oldest at the bottom):\n");
        for file in &ide.recently_viewed_files {
            output.push_str(&format!(
                "- {} (total lines: {})\n",
                file.path, file.total_lines
            ));
        }
        output.push('\n');
    }
    if !ide.visible_files.is_empty() {
        output.push_str("Files that are currently open and visible in the user's IDE:\n");
        for (index, file) in ide.visible_files.iter().enumerate() {
            output.push_str(&format!("- {} (", file.path));
            if index == 0 {
                output.push_str("currently focused file");
                if let Some(cursor) = &file.cursor_position {
                    output.push_str(&format!(", cursor is on line {}", cursor.line));
                }
                output.push_str(&format!(", total lines: {}", file.total_lines));
            } else {
                output.push_str(&format!("total lines: {}", file.total_lines));
            }
            output.push_str(")\n");
        }
        output.push('\n');
    }
    output.push_str(
        "Note: these files may or may not be relevant to the current conversation. Use the read file tool if you need to get the contents of some of them.\n</open_and_recently_viewed_files>",
    );
    output
}

struct Time {
    timestamp: String,
    today: String,
}

impl Time {
    /// 客户端给的时区名不一定能被解析。
    ///
    /// 协议里 `time_zone` 是没有取值域约束的自由字符串（`agent_v1.proto` 只写 `string`），
    /// Windows 时区显示名（`China Standard Time`）、空串、任意垃圾都会进来。以前解析失败
    /// 直接返回 `Error::Protocol`，而三条路径（runtime 消息、request-context 消息、后台完成
    /// 消息）都带 `?`——整轮请求失败、会话完全不可用，用户看到的却只是
    /// "invalid Cursor time zone"。现在降级成 UTC：时间戳仍然可用，原值记一条 warn，
    /// 事后能确认客户端到底送了什么。
    fn now(time_zone: Option<&str>) -> Self {
        let zone = match time_zone.filter(|value| !value.is_empty()) {
            Some(value) => match value.parse::<Tz>() {
                Ok(zone) => zone,
                Err(_) => {
                    tracing::warn!(
                        time_zone = value,
                        "unknown Cursor time zone; using UTC for timestamps"
                    );
                    chrono_tz::UTC
                }
            },
            None => chrono_tz::UTC,
        };
        let now = Utc::now().with_timezone(&zone);
        let offset = now.offset().fix().local_minus_utc();
        let sign = if offset < 0 { '-' } else { '+' };
        let offset = offset.unsigned_abs();
        let hours = offset / 3600;
        let minutes = (offset % 3600) / 60;
        let utc = if minutes == 0 {
            format!("UTC{sign}{hours}")
        } else {
            format!("UTC{sign}{hours}:{minutes:02}")
        };
        Self {
            timestamp: format!("{} ({utc})", now.format("%A, %b %-d, %Y, %-I:%M %p")),
            today: now.format("%A %b %-d,\n%Y").to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        cursor::{
            prompting::PromptAssets,
            services::observability::CursorTraceService,
            transport::{OutputHub, TransportHandle},
        },
        store::Store,
    };

    #[tokio::test]
    async fn runtime_user_context_preserves_paragraph_separators() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let traces = CursorTraceService::new(store.clone());
        let (commands, _receiver) = tokio::sync::mpsc::channel(1);
        let handle = TransportHandle::new(
            "runtime-context".into(),
            commands,
            Arc::new(OutputHub::default()),
            traces.recorder("runtime-context"),
        );
        let blobs = BlobSynchronizer::new("runtime-context".into(), store, handle);
        let compiler = PromptCompiler::new(PromptAssets::embedded().unwrap());
        let action = pb::UserMessageAction {
            user_message: Some(pb::UserMessage {
                message_id: "followup".into(),
                text: "Continue the task.".into(),
                mode: pb::AgentMode::Agent as i32,
                subagent_system_reminder: Some("Third context paragraph.".into()),
                ..Default::default()
            }),
            prepend_user_messages: vec![
                pb::UserMessage {
                    text: "First context paragraph.".into(),
                    ..Default::default()
                },
                pb::UserMessage {
                    text: "Second context paragraph.".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let message =
            compile_user_message_action(&action, pb::AgentMode::Agent as i32, &compiler, &blobs)
                .await
                .unwrap();
        let MessageContent::Parts { parts } = message.content else {
            panic!("runtime message must contain text parts");
        };
        let text = parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            text.contains(
                "First context paragraph.\n\nSecond context paragraph.\n\nThird context paragraph."
            ),
            "{text}"
        );
    }

    /// 客户端给的时区名解析不了时，时间戳降级到 UTC，而不是整轮请求失败。
    ///
    /// 协议里 `time_zone` 是自由字符串：Windows 时区显示名（`China Standard Time`）真的会
    /// 出现在这里；以前它会让 runtime / request-context / 后台完成三条消息编译路径一起报
    /// "invalid Cursor time zone"，会话完全不可用。
    #[test]
    fn an_unknown_cursor_time_zone_falls_back_to_utc() {
        let utc = Time::now(None).timestamp;
        let windows_name = Time::now(Some("China Standard Time")).timestamp;
        let garbage = Time::now(Some("not a time zone")).timestamp;
        assert!(utc.contains("(UTC+0)"), "{utc}");
        assert_eq!(windows_name, utc, "解析不了的名字按 UTC 处理");
        assert_eq!(garbage, utc);
        // 合法的 IANA 名字仍然按它算偏移。
        let shanghai = Time::now(Some("Asia/Shanghai"));
        assert!(
            shanghai.timestamp.contains("(UTC+8)"),
            "{}",
            shanghai.timestamp
        );
        // 空串与缺省同义（客户端没填时不要报错）。
        assert_eq!(Time::now(Some("")).timestamp, utc);
    }
}
