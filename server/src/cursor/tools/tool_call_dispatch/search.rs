//! Dispatches search Tool calls.
//! Cursor tool orchestration for application-owned Semble search.

use crate::{
    cursor::tools::{
        runtime::{now_ms, CursorToolRuntime, ExecContext},
        tool_call_result::{self as result, ToolResultSender},
    },
    model::ToolCall,
    search,
    store::Store,
    Result,
};

use super::ToolStart;

pub(super) fn start(
    runtime: &CursorToolRuntime,
    results: &ToolResultSender,
    call: &ToolCall,
    context: &ExecContext,
    store: Option<Store>,
) -> Result<ToolStart> {
    let tool_name = super::normalized(&call.name);
    let arguments = call.arguments.clone();
    let workspace_paths = context.workspace_paths.clone();
    let call = call.clone();
    let results = results.clone();
    let started_at_ms = now_ms();
    // Semble 不在 execs 里(它没有交互审批那一步),所以整段工作既不受 drain_running 覆盖,
    // 也没有内建超时。登记取消令牌让"停止/断连/换轮次"能中断它;取消时仍发一条工具结果,
    // 工具轮不会因为缺一个结果而一直等。执行侧另有硬超时(search::search_provider)。
    let detached = runtime.track_detached(&call.call_id);
    let cancelled = detached.token();
    tokio::spawn(async move {
        let _detached = detached;
        let output = tokio::select! {
            output = search::execute_semble(&tool_name, arguments, store, &workspace_paths) => output,
            () = cancelled.cancelled() => Err("Semble search was interrupted before it returned a result".into()),
        };
        match result::semble(&call, started_at_ms, output) {
            Ok(completion) => results.send(completion),
            Err(error) => results.send_error(error),
        }
    });
    Ok(ToolStart {
        messages: Vec::new(),
        completion: None,
    })
}
