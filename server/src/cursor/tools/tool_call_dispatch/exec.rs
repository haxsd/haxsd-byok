//! Dispatches command execution Tool calls.
//! Direct Exec and dynamic MCP dispatch.

use crate::{cursor::protocol::proto::agent::v1 as pb, model::ToolCall, Error, Result};

use super::{normalized, ToolStart};
use crate::cursor::tools::{
    codec,
    runtime::{CursorToolRuntime, ExecContext},
    tool_call_result as result,
};

pub(super) async fn start(
    runtime: &CursorToolRuntime,
    call: &ToolCall,
    context: &ExecContext,
) -> Result<ToolStart> {
    let message = match normalized(&call.name).as_str() {
        "getmcptools" => {
            let id = runtime.reserve_exec(call, context).await?;
            encode(runtime, id, Ok(codec::mcp_state_request(id, call))).await?
        }
        "callmcptool" => {
            let server = required(call, "server")?;
            let tool = required(call, "toolName")?;
            let Some(route) = context
                .mcp_routes
                .get(&(server.to_string(), tool.to_string()))
            else {
                return Ok(ToolStart {
                    messages: Vec::new(),
                    completion: Some(result::mcp_failure(
                        call,
                        format!("MCP descriptor not found for {server}/{tool}"),
                    )?),
                });
            };
            let id = runtime.reserve_exec(call, context).await?;
            encode(
                runtime,
                id,
                codec::mcp_meta_request(id, call, server, route),
            )
            .await?
        }
        _ => {
            let id = runtime.reserve_exec(call, context).await?;
            encode(runtime, id, codec::request(id, call, context)).await?
        }
    };
    Ok(ToolStart {
        messages: vec![message],
        completion: None,
    })
}

/// `reserve_exec` 之后再到编码这一步,任何 `?` 都会把登记项永久留在 `execs` 里:客户端从没
/// 收到这个 id,而 `running_exec_ids` 会一直把它算成在跑、停止时还会为它发 abort。所以编码
/// 失败必须把登记摘掉(与 `discard_exec`/`discard_interaction` 成对)。
async fn encode(
    runtime: &CursorToolRuntime,
    id: u32,
    encoded: Result<pb::AgentServerMessage>,
) -> Result<pb::AgentServerMessage> {
    match encoded {
        Ok(message) => Ok(message),
        Err(error) => {
            runtime.discard_exec(id).await;
            Err(error)
        }
    }
}

fn required<'a>(call: &'a ToolCall, name: &str) -> Result<&'a str> {
    call.arguments
        .get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Protocol(format!("{} is missing {name}", call.name)))
}

pub(super) async fn start_dynamic(
    runtime: &CursorToolRuntime,
    call: &ToolCall,
    definition: &pb::McpToolDefinition,
    context: &ExecContext,
) -> Result<ToolStart> {
    let id = runtime
        .reserve_dynamic_mcp(call, context, definition)
        .await?;
    let message = encode(runtime, id, codec::mcp_request(id, call, definition)).await?;
    Ok(ToolStart {
        messages: vec![message],
        completion: None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// 编码失败(`Read` 缺 path)不能让登记项留在 execs 里:客户端从没收到这个 id,
    /// 而 drain_running 会为残留项发 abort。
    #[tokio::test]
    async fn a_failed_encode_leaves_no_reserved_exec_behind() {
        let runtime = CursorToolRuntime::default();
        let call = ToolCall {
            index: 0,
            call_id: "read-call".into(),
            model_call_id: "model-read-call".into(),
            name: "Read".into(),
            arguments_text: "{}".into(),
            arguments: json!({}),
            argument_error: None,
        };

        assert!(start(&runtime, &call, &ExecContext::default())
            .await
            .is_err());

        assert!(
            runtime.running_exec_ids().await.is_empty(),
            "编码失败后 execs 里不该留下登记项"
        );
    }
}
