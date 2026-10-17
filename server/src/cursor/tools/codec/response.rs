//! Decodes Tool execution responses received from Cursor.
use crate::{
    cursor::{
        protocol::{events, proto::agent::v1 as pb},
        tools::{
            compat, edit,
            runtime::{CursorToolRuntime, ExecStage, ExecTake, PendingExec},
            tool_call_result::{self as result, ToolCompletion},
        },
    },
    model::ToolCall,
    Error, Result,
};

use super::request::edit_write_request;

pub enum ClientExecEvent {
    Delta(Box<pb::AgentServerMessage>),
    Message(Box<pb::AgentServerMessage>),
    Completed(Box<ToolCompletion>),
    Pending,
}

pub async fn client_event(
    message: &pb::ExecClientMessage,
    pending: &CursorToolRuntime,
) -> Result<ClientExecEvent> {
    if pending.is_interrupted(message.id).await {
        if message.message.as_ref().is_some_and(is_terminal) {
            pending.discard_exec(message.id).await;
        }
        return Ok(ClientExecEvent::Pending);
    }
    let call = match pending.exec_call(message.id).await {
        Some(call) => call,
        None if pending.completed_call(message.id).await.is_some() => {
            tracing::warn!(id = message.id, "ignoring duplicate terminal tool response");
            return Ok(ClientExecEvent::Pending);
        }
        None => {
            tracing::warn!(
                id = message.id,
                "ignoring response for unknown tool execution"
            );
            return Ok(ClientExecEvent::Pending);
        }
    };
    let Some(wire_result) = &message.message else {
        return Ok(ClientExecEvent::Pending);
    };
    let pb::exec_client_message::Message::ShellStream(stream) = wire_result else {
        let Some(entry) = take(message.id, pending).await else {
            return Ok(ClientExecEvent::Pending);
        };
        return match entry.stage {
            ExecStage::EditRead => advance_edit(entry, wire_result, pending).await,
            ExecStage::Direct | ExecStage::DynamicMcp(_) | ExecStage::EditWrite(_) => {
                completed(entry, wire_result.clone())
            }
        };
    };
    use pb::shell_stream::Event;
    let event = match &stream.event {
        Some(Event::Stdout(stdout)) => {
            if pending.append_stdout(message.id, &stdout.data).await {
                ClientExecEvent::Delta(Box::new(shell_delta(&call, true, &stdout.data)))
            } else {
                ClientExecEvent::Pending
            }
        }
        Some(Event::Stderr(stderr)) => {
            if pending.append_stderr(message.id, &stderr.data).await {
                ClientExecEvent::Delta(Box::new(shell_delta(&call, false, &stderr.data)))
            } else {
                ClientExecEvent::Pending
            }
        }
        Some(Event::Start(_)) | Some(Event::HookContext(_)) => ClientExecEvent::Pending,
        Some(Event::Exit(exit)) => {
            let Some(entry) = take(message.id, pending).await else {
                return Ok(ClientExecEvent::Pending);
            };
            let result = shell_exit_result(message, exit, &entry.stdout, &entry.stderr);
            completed(entry, pb::exec_client_message::Message::ShellResult(result))?
        }
        Some(Event::Backgrounded(backgrounded)) => {
            let Some(entry) = take(message.id, pending).await else {
                return Ok(ClientExecEvent::Pending);
            };
            let result = shell_backgrounded_result(
                backgrounded,
                &entry.stdout,
                &entry.stderr,
                &entry.context.terminals_folder,
            );
            completed(entry, pb::exec_client_message::Message::ShellResult(result))?
        }
        Some(Event::Rejected(value)) => {
            let result = pb::ShellResult {
                result: Some(pb::shell_result::Result::Rejected(value.clone())),
                ..Default::default()
            };
            complete(
                message.id,
                pending,
                pb::exec_client_message::Message::ShellResult(result),
            )
            .await?
        }
        Some(Event::PermissionDenied(value)) => {
            let result = pb::ShellResult {
                result: Some(pb::shell_result::Result::PermissionDenied(value.clone())),
                ..Default::default()
            };
            complete(
                message.id,
                pending,
                pb::exec_client_message::Message::ShellResult(result),
            )
            .await?
        }
        Some(Event::SandboxUnsupported(value)) => {
            let result = pb::ShellResult {
                result: Some(pb::shell_result::Result::SpawnError(pb::ShellSpawnError {
                    command: value.command.clone(),
                    working_directory: value.working_directory.clone(),
                    error: value.reason.clone(),
                })),
                ..Default::default()
            };
            complete(
                message.id,
                pending,
                pb::exec_client_message::Message::ShellResult(result),
            )
            .await?
        }
        None => ClientExecEvent::Pending,
    };
    Ok(event)
}

pub async fn stream_closed(id: u32, pending: &CursorToolRuntime) -> Result<Option<ToolCompletion>> {
    if pending.is_interrupted(id).await {
        pending.discard_exec(id).await;
        return Ok(None);
    }
    let Some(entry) = pending.take_exec(id).await else {
        return Ok(None);
    };
    let error = "Cursor Exec stream closed before returning a terminal result";
    if entry.call.name.eq_ignore_ascii_case("Shell") || entry.call.name.eq_ignore_ascii_case("Bash")
    {
        let command = entry
            .call
            .arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let working_directory = entry
            .call
            .arguments
            .get("working_directory")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Ok(Some(result::from_exec(
            entry,
            &pb::exec_client_message::Message::ShellResult(pb::ShellResult {
                result: Some(pb::shell_result::Result::SpawnError(pb::ShellSpawnError {
                    command,
                    working_directory,
                    error: error.into(),
                })),
                ..Default::default()
            }),
        )?));
    }
    let rendered = match &entry.stage {
        ExecStage::DynamicMcp(definition) => {
            Ok(super::render_dynamic_mcp(&entry.call, definition, false))
        }
        _ => super::render_tool_call(&entry.call, false),
    };
    let rendered = match rendered {
        Ok(rendered) => rendered,
        Err(Error::Protocol(message)) => {
            return Ok(Some(compat::failure_with_message(&entry.call, message)));
        }
        Err(Error::Json(error)) => {
            return Ok(Some(compat::failure_with_message(
                &entry.call,
                error.to_string(),
            )));
        }
        Err(error) => return Err(error),
    };
    Ok(Some(ToolCompletion::from_rendered(
        &entry.call,
        entry.started_at_ms,
        error.into(),
        true,
        rendered,
    )?))
}

fn is_terminal(message: &pb::exec_client_message::Message) -> bool {
    use pb::{exec_client_message::Message, shell_stream::Event};

    match message {
        Message::ShellStream(stream) => matches!(
            stream.event.as_ref(),
            Some(Event::Exit(_))
                | Some(Event::Backgrounded(_))
                | Some(Event::Rejected(_))
                | Some(Event::PermissionDenied(_))
                | Some(Event::SandboxUnsupported(_))
        ),
        _ => true,
    }
}

async fn advance_edit(
    entry: PendingExec,
    result: &pb::exec_client_message::Message,
    registry: &CursorToolRuntime,
) -> Result<ClientExecEvent> {
    let read = match result {
        pb::exec_client_message::Message::ReadResult(result)
        | pb::exec_client_message::Message::RedactedReadResult(result) => result,
        _ => {
            let message = format!("expected ReadResult for edit tool {}", entry.call.name);
            return Ok(ClientExecEvent::Completed(Box::new(
                compat::failure_with_message(&entry.call, message),
            )));
        }
    };
    let write = match edit::after_read(&entry.call, read) {
        Ok(write) => write,
        Err(error) => {
            return Ok(ClientExecEvent::Completed(Box::new(result::edit_failure(
                entry, error,
            )?)))
        }
    };
    let id = registry
        .reserve_edit_write(
            &entry.call,
            &entry.context,
            write.clone(),
            entry.started_at_ms,
        )
        .await?;
    let request = match edit_write_request(id, &entry.call, &write) {
        Ok(request) => request,
        Err(error) => {
            // 写请求编码失败时同样要摘掉登记(见 tool_call_dispatch/exec.rs 的 encode)。
            registry.discard_exec(id).await;
            return Err(error);
        }
    };
    Ok(ClientExecEvent::Message(Box::new(request)))
}

async fn complete(
    id: u32,
    pending: &CursorToolRuntime,
    result: pb::exec_client_message::Message,
) -> Result<ClientExecEvent> {
    let Some(entry) = take(id, pending).await else {
        return Ok(ClientExecEvent::Pending);
    };
    completed(entry, result)
}

/// 取走终态事件对应的 exec。"已打断"与"未知 id"都只记日志、不返回错误:
/// 客户端的终态事件可能与用户插话抢时序(`take_exec_unless_interrupted` 说明),把它当
/// 致命协议错误会让整轮失败,而这条事件本身没有任何非法之处。
async fn take(id: u32, pending: &CursorToolRuntime) -> Option<PendingExec> {
    match pending.take_exec_unless_interrupted(id).await {
        ExecTake::Taken(entry) => Some(*entry),
        ExecTake::Interrupted => None,
        ExecTake::Missing => {
            tracing::warn!(
                id,
                "ignoring terminal response for an unknown tool execution"
            );
            None
        }
    }
}

fn completed(
    pending: PendingExec,
    result: pb::exec_client_message::Message,
) -> Result<ClientExecEvent> {
    let call = pending.call.clone();
    let completion = match result::from_exec(pending, &result) {
        Ok(completion) => completion,
        Err(Error::Protocol(message)) => compat::failure_with_message(&call, message),
        Err(Error::Json(error)) => compat::failure_with_message(&call, error.to_string()),
        Err(error) => return Err(error),
    };
    Ok(ClientExecEvent::Completed(Box::new(completion)))
}

fn shell_exit_result(
    message: &pb::ExecClientMessage,
    exit: &pb::ShellStreamExit,
    stdout: &str,
    stderr: &str,
) -> pb::ShellResult {
    let result = if exit.code == 0 && !exit.aborted {
        pb::shell_result::Result::Success(pb::ShellSuccess {
            working_directory: exit.cwd.clone(),
            exit_code: exit.code as i32,
            stdout: stdout.into(),
            stderr: stderr.into(),
            interleaved_output: Some(format!("{stdout}{stderr}")),
            local_execution_time_ms: exit
                .local_execution_time_ms
                .or(message.local_execution_time_ms),
            ..Default::default()
        })
    } else {
        pb::shell_result::Result::Failure(pb::ShellFailure {
            working_directory: exit.cwd.clone(),
            exit_code: exit.code as i32,
            stdout: stdout.into(),
            stderr: stderr.into(),
            interleaved_output: Some(format!("{stdout}{stderr}")),
            abort_reason: exit.abort_reason,
            aborted: exit.aborted,
            local_execution_time_ms: exit
                .local_execution_time_ms
                .or(message.local_execution_time_ms),
            ..Default::default()
        })
    };
    pb::ShellResult {
        result: Some(result),
        is_background: Some(false),
        ..Default::default()
    }
}

fn shell_backgrounded_result(
    backgrounded: &pb::ShellStreamBackgrounded,
    stdout: &str,
    stderr: &str,
    terminals_folder: &str,
) -> pb::ShellResult {
    pb::ShellResult {
        result: Some(pb::shell_result::Result::Success(pb::ShellSuccess {
            command: backgrounded.command.clone(),
            working_directory: backgrounded.working_directory.clone(),
            stdout: stdout.into(),
            stderr: stderr.into(),
            shell_id: Some(backgrounded.shell_id),
            pid: backgrounded.pid,
            ms_to_wait: backgrounded.ms_to_wait,
            background_reason: backgrounded.reason,
            interleaved_output: Some(format!("{stdout}{stderr}")),
            ..Default::default()
        })),
        is_background: Some(true),
        terminals_folder: (!terminals_folder.is_empty()).then(|| terminals_folder.into()),
        pid: backgrounded.pid,
        ..Default::default()
    }
}

fn shell_delta(call: &ToolCall, stdout: bool, content: &str) -> pb::AgentServerMessage {
    let delta = if stdout {
        pb::shell_tool_call_delta::Delta::Stdout(pb::ShellToolCallStdoutDelta {
            content: content.into(),
        })
    } else {
        pb::shell_tool_call_delta::Delta::Stderr(pb::ShellToolCallStderrDelta {
            content: content.into(),
        })
    };
    events::server_interaction(pb::interaction_update::Message::ToolCallDelta(Box::new(
        pb::ToolCallDeltaUpdate {
            call_id: call.call_id.clone(),
            tool_call_delta: Some(Box::new(pb::ToolCallDelta {
                delta: Some(pb::tool_call_delta::Delta::ShellToolCallDelta(
                    pb::ShellToolCallDelta { delta: Some(delta) },
                )),
            })),
            model_call_id: call.model_call_id.clone(),
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::tools::runtime::ExecContext;

    fn terminal_message(id: u32) -> pb::ExecClientMessage {
        pb::ExecClientMessage {
            id,
            message: Some(pb::exec_client_message::Message::ShellResult(
                pb::ShellResult::default(),
            )),
            ..Default::default()
        }
    }

    /// 取不到 exec 曾经是 `Error::Protocol("unknown terminal Exec id")`,一路传到 run 结果里
    /// 变成整轮失败;现在只记日志并忽略这条终态事件。
    #[tokio::test]
    async fn taking_an_unknown_exec_is_not_an_error() {
        let runtime = CursorToolRuntime::default();
        assert!(take(9, &runtime).await.is_none());
    }

    /// 迟到的终态事件(客户端推来、服务端已经没有这条 exec)同样只是被忽略。
    #[tokio::test]
    async fn a_terminal_event_without_a_pending_exec_is_ignored() {
        let runtime = CursorToolRuntime::default();
        let event = client_event(&terminal_message(42), &runtime).await.unwrap();
        assert!(matches!(event, ClientExecEvent::Pending));
    }

    /// 用户插话摘掉 exec 之后到达的终态事件:保持"忽略",不冒泡成错误。
    #[tokio::test]
    async fn a_terminal_event_for_an_interrupted_exec_is_ignored() {
        let runtime = CursorToolRuntime::default();
        let call = ToolCall {
            index: 0,
            call_id: "shell-call".into(),
            model_call_id: "model-shell-call".into(),
            name: "Shell".into(),
            arguments_text: String::new(),
            arguments: serde_json::json!({"command": "echo hi"}),
            argument_error: None,
        };
        let id = runtime
            .reserve_exec(&call, &ExecContext::default())
            .await
            .unwrap();
        runtime.interrupt_for_message().await;

        let event = client_event(&terminal_message(id), &runtime).await.unwrap();

        assert!(matches!(event, ClientExecEvent::Pending));
    }
}
