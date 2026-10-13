//! Compiles non-interrupting runtime information into append-only Messages.
use std::collections::BTreeMap;

use crate::{cursor::protocol::proto::agent::v1 as pb, Error, Result};

pub(super) const FOLLOW_UP: &str = concat!(
    "Perform any necessary follow-up actions in response to the subagent completion above. ",
    "If no follow-up work is needed, no further action is required. ",
    "If you mention an agent or subagent in your response, link it with the `[Name](id)` ",
    "Don't use generic label such as `[agent]`, `[worker]`, or `[subagent]`. ",
    "For cloud subagents, when the agent has edited code, link to `[Review](bc-id#changes)`, ",
    "or, if you know the exact added and deleted line counts, `[Review +A −D](bc-id#changes)`, ",
    "replacing A and D with those counts. Never write A or D literally. ",
    "Use `[Try Live](bc-id#desktop)` only when the agent used computer use. ",
    "Don't repeat the same confirmation every time."
);

pub(super) const SHELL_FOLLOW_UP: &str = concat!(
    "Briefly inform the user about the task result and perform any follow-up actions (if needed). ",
    "If there's no follow-ups needed, don't explicitly say that."
);

#[derive(Debug)]
pub(crate) struct Projection {
    pub context: String,
    pub turn_user: pb::UserMessage,
}

/// 服务端自己发起唤醒时用的完成动作：字段与客户端发来的通知同源。
///
/// 身份由 `subagent_id` + `tool_call_id` 决定（`KIND:subagent_id:tool_call_id`），
/// 与客户端自己发送时的算法一致，所以两边天然去重；`title` 是父会话里那次 Task 的
/// 描述，`detail` 是子代理的收尾文本。
pub(crate) fn subagent_completion(
    subagent_id: &str,
    tool_call_id: &str,
    title: &str,
    detail: Option<&str>,
    status: pb::BackgroundTaskStatus,
) -> pb::BackgroundTaskCompletionAction {
    pb::BackgroundTaskCompletionAction {
        completions: vec![pb::BackgroundTaskCompletion {
            task_id: subagent_id.to_owned(),
            kind: pb::BackgroundTaskKind::Subagent as i32,
            status: status as i32,
            title: title.to_owned(),
            detail: detail.map(str::to_owned),
            output_path: None,
            thread_id: None,
            reason: pb::BackgroundTaskCompletionReason::TaskFinished as i32,
            subagent_id: Some(subagent_id.to_owned()),
            tool_call_id: Some(tool_call_id.to_owned()),
            notification_context: pb::BackgroundTaskNotificationContext::Unspecified as i32,
        }],
    }
}

pub(crate) fn project(
    action: &pb::BackgroundTaskCompletionAction,
    mode: i32,
) -> Result<Projection> {
    if action.completions.is_empty() {
        return Err(reject(
            action,
            "background task completion action contains no completion",
        ));
    }

    let mut completions = BTreeMap::new();
    let mut has_shell = false;
    let mut has_subagent = false;
    for completion in &action.completions {
        let kind = pb::BackgroundTaskKind::try_from(completion.kind).map_err(|_| {
            reject(
                action,
                format!("unknown background task kind: {}", completion.kind),
            )
        })?;
        if kind == pb::BackgroundTaskKind::Unspecified {
            return Err(reject(
                action,
                format!(
                    "background task completion has invalid kind: {}",
                    kind.as_str_name()
                ),
            ));
        }
        let reason =
            pb::BackgroundTaskCompletionReason::try_from(completion.reason).map_err(|_| {
                reject(
                    action,
                    format!(
                        "unknown background task completion reason: {}",
                        completion.reason
                    ),
                )
            })?;
        if reason != pb::BackgroundTaskCompletionReason::TaskFinished {
            // Progress and reparenting notifications are informational; the
            // client batches them together with the real finish notification.
            continue;
        }
        if completion.task_id.is_empty() || completion.title.is_empty() {
            return Err(reject(
                action,
                "background task completion requires task_id and title",
            ));
        }
        let agent_id = match kind {
            pb::BackgroundTaskKind::Shell => {
                has_shell = true;
                None
            }
            pb::BackgroundTaskKind::Subagent => {
                has_subagent = true;
                Some(
                    completion
                        .subagent_id
                        .as_deref()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            reject(action, "background subagent completion has no subagent_id")
                        })?,
                )
            }
            pb::BackgroundTaskKind::Unspecified => unreachable!(),
        };
        let tool_call_id = completion
            .tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| reject(action, "background task completion has no tool_call_id"))?;
        let task_identity = agent_id.unwrap_or(&completion.task_id);
        let identity = format!("{}:{task_identity}:{tool_call_id}", kind.as_str_name());
        let context = completion_context(completion, kind, agent_id).map_err(|error| {
            tracing::warn!(
                completions = action.completions.len(),
                detail = %describe(action),
                error = %error,
                "rejecting a background task completion"
            );
            error
        })?;
        if completions
            .insert(identity.clone(), (completion, context))
            .is_some()
        {
            return Err(reject(
                action,
                format!("duplicate background task completion: {identity}"),
            ));
        }
    }

    let (first, _) = completions.values().next().ok_or_else(|| {
        reject(
            action,
            "background task notification contains no finished task",
        )
    })?;
    tracing::info!(
        completions = completions.len(),
        tasks = %completions.keys().cloned().collect::<Vec<_>>().join(" "),
        has_shell,
        has_subagent,
        "accepted a background task completion"
    );
    let text = match (has_shell, has_subagent) {
        (true, false) => SHELL_FOLLOW_UP.into(),
        (false, true) => FOLLOW_UP.into(),
        (true, true) => format!("{SHELL_FOLLOW_UP}\n\n{FOLLOW_UP}"),
        (false, false) => unreachable!(),
    };
    Ok(Projection {
        context: completions
            .values()
            .map(|(_, context)| context.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        turn_user: pb::UserMessage {
            text,
            message_id: format!(
                "background-completed:{}",
                completions.keys().cloned().collect::<Vec<_>>().join(":")
            ),
            mode,
            is_simulated_msg: Some(true),
            simulated_msg_reason: Some(pb::SimulatedMsgReason::BackgroundTaskCompletion as i32),
            simulated_message_metadata: Some(pb::user_message::SimulatedMessageMetadata {
                title: Some(first.title.clone()),
                task_id: Some(first.task_id.clone()),
                ..Default::default()
            }),
            ..Default::default()
        },
    })
}

/// 拒绝一条后台完成通知时，把"这条通知里到底有什么"写进日志：每条 completion 的
/// kind / reason / status 原值，以及四个必需字段是否存在。只记存在性，不记内容。
fn describe(action: &pb::BackgroundTaskCompletionAction) -> String {
    action
        .completions
        .iter()
        .take(4)
        .map(|completion| {
            format!(
                "[kind={} reason={} status={} task_id={} title={} subagent_id={} tool_call_id={}]",
                completion.kind,
                completion.reason,
                completion.status,
                present(&completion.task_id),
                present(&completion.title),
                present(completion.subagent_id.as_deref().unwrap_or_default()),
                present(completion.tool_call_id.as_deref().unwrap_or_default()),
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn present(value: &str) -> &'static str {
    if value.trim().is_empty() {
        "no"
    } else {
        "yes"
    }
}

fn reject(action: &pb::BackgroundTaskCompletionAction, reason: impl Into<String>) -> Error {
    let reason = reason.into();
    tracing::warn!(
        completions = action.completions.len(),
        detail = %describe(action),
        reason = %reason,
        "rejecting a background task completion"
    );
    Error::Protocol(reason)
}

fn status(completion: &pb::BackgroundTaskCompletion) -> Result<pb::BackgroundTaskStatus> {
    let status = pb::BackgroundTaskStatus::try_from(completion.status).map_err(|_| {
        Error::Protocol(format!(
            "unknown background task status: {}",
            completion.status
        ))
    })?;
    if status == pb::BackgroundTaskStatus::Unspecified {
        return Err(Error::Protocol(
            "background task completion has unspecified status".into(),
        ));
    }
    Ok(status)
}

fn completion_context(
    completion: &pb::BackgroundTaskCompletion,
    kind: pb::BackgroundTaskKind,
    agent_id: Option<&str>,
) -> Result<String> {
    let status = status(completion)?;
    let mut fields = vec![
        format!(
            "kind: {}",
            match kind {
                pb::BackgroundTaskKind::Shell => "shell",
                pb::BackgroundTaskKind::Subagent => "subagent",
                pb::BackgroundTaskKind::Unspecified => unreachable!(),
            }
        ),
        format!("status: {}", status_name(status)),
        format!("task_id: {}", completion.task_id),
        format!("title: {}", completion.title),
    ];
    optional_field(
        &mut fields,
        "tool_call_id",
        completion.tool_call_id.as_deref(),
    );
    optional_field(&mut fields, "agent_id", agent_id);
    optional_field(&mut fields, "detail", completion.detail.as_deref());
    optional_field(
        &mut fields,
        "output_path",
        completion.output_path.as_deref(),
    );
    optional_field(&mut fields, "thread_id", completion.thread_id.as_deref());
    Ok(format!(
        "<system_notification>\nThe following task has finished. If you were already aware, ignore this notification and do not restate prior responses.\n\n<task>\n{}\n</task>\n</system_notification>",
        fields.join("\n")
    ))
}

fn optional_field(fields: &mut Vec<String>, name: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        fields.push(format!("{name}: {value}"));
    }
}

fn status_name(status: pb::BackgroundTaskStatus) -> &'static str {
    match status {
        pb::BackgroundTaskStatus::Success => "success",
        pb::BackgroundTaskStatus::Error => "error",
        pb::BackgroundTaskStatus::Aborted => "aborted",
        pb::BackgroundTaskStatus::Unspecified => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 服务端自己发起的唤醒要与客户端发来的通知同形状：同样的完成上下文、
    /// 同样的模拟轮标记、同样的事件身份——这样两条来源天然去重。
    #[test]
    fn a_server_side_wakeup_matches_the_client_notification_shape() {
        let action = subagent_completion(
            "child-id",
            "task-call",
            "研究多智能体模式",
            Some("child result"),
            pb::BackgroundTaskStatus::Success,
        );
        let projection = project(&action, pb::AgentMode::Agent as i32).unwrap();
        assert!(projection.context.contains("kind: subagent"));
        assert!(projection.context.contains("agent_id: child-id"));
        assert!(projection.context.contains("child result"));
        assert_eq!(
            projection.turn_user.simulated_msg_reason,
            Some(pb::SimulatedMsgReason::BackgroundTaskCompletion as i32)
        );

        let event_id = format!(
            "background-completed:{}:child-id:task-call",
            pb::BackgroundTaskKind::Subagent.as_str_name()
        );
        let message = crate::cursor::compile::wakeup_message(&event_id, &projection.context)
            .expect("wakeup message");
        assert_eq!(message.runtime_event_id.as_deref(), Some(event_id.as_str()));
        let crate::model::MessageContent::Parts { parts } = &message.content else {
            panic!("wakeup message must be parts");
        };
        let [crate::model::ContentPart::Text { text }] = parts.as_slice() else {
            panic!("wakeup message must be one text part");
        };
        assert!(text.contains("kind: subagent"));
        assert!(text.contains("child result"));
        assert!(text.contains(FOLLOW_UP));
    }
}
