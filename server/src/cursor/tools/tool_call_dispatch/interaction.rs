//! Dispatches Tool calls that require Cursor user interaction.
//! Interaction query dispatch and approval continuation.

use crate::{
    cursor::{protocol::proto::agent::v1 as pb, tools::codec as interaction},
    model::ToolCall,
    search::{WebFetch, WebSearch},
    Error, Result,
};

use super::{normalized, InteractionContinuation, ToolStart};
use crate::cursor::tools::{
    runtime::{CursorToolRuntime, PendingInteraction},
    tool_call_result::{self as result, ToolResultSender},
};

const INTERRUPTED_FETCH: &str = "WebFetch was interrupted before it returned a result";
const INTERRUPTED_SEARCH: &str = "WebSearch was interrupted before it returned a result";

pub(super) async fn start(runtime: &CursorToolRuntime, call: &ToolCall) -> Result<ToolStart> {
    let id = runtime.reserve_interaction(call).await?;
    // 与 exec 侧同一个道理:登记之后编码失败要摘掉,否则这条交互会一直挂在 interactions 里,
    // 停止时被当成"还有待审批的工具"。
    let query = match interaction::tool_query(id, call) {
        Ok(query) => query,
        Err(error) => {
            runtime.discard_interaction(id).await;
            return Err(error);
        }
    };
    Ok(ToolStart {
        messages: vec![query],
        completion: None,
    })
}

pub(super) async fn resume(
    runtime: &CursorToolRuntime,
    results: &ToolResultSender,
    search: &WebSearch,
    fetch: &WebFetch,
    pending: PendingInteraction,
    response: &pb::InteractionResponse,
) -> Result<InteractionContinuation> {
    if normalized(&pending.call.name) == "websearch"
        && matches!(
            response.result.as_ref(),
            Some(pb::interaction_response::Result::WebSearchRequestResponse(
                pb::WebSearchRequestResponse {
                    result: Some(pb::web_search_request_response::Result::Approved(_)),
                }
            ))
        )
    {
        start_web_search(runtime, results.clone(), search.clone(), pending)?;
        return Ok(InteractionContinuation::Pending);
    }
    if normalized(&pending.call.name) == "webfetch"
        && matches!(
            response.result.as_ref(),
            Some(pb::interaction_response::Result::WebFetchRequestResponse(
                pb::WebFetchRequestResponse {
                    result: Some(pb::web_fetch_request_response::Result::Approved(_)),
                }
            ))
        )
    {
        start_web_fetch(runtime, results.clone(), fetch.clone(), pending)?;
        return Ok(InteractionContinuation::Pending);
    }
    Ok(InteractionContinuation::Completed(Box::new(
        result::from_interaction(pending, response)?,
    )))
}

/// 审批之后的抓取/搜索是脱离运行时的裸 `tokio::spawn`:交互条目已经被
/// `take_interaction` 摘掉,所以 `drain_running`/`interrupt_*` 都看不到它们,用户点"停止"
/// 或断开连接之后它们照样跑到底(最长 30s 抓取、Semble 更是没有内建超时)。
///
/// 这里把任务的取消令牌登记进运行时(停止/断连/换轮次都会取消),并用 `select!` 让取消
/// 立刻生效;取消时仍然发一条工具结果 —— 工具轮不会因为少一个结果而一直等下去,而真正
/// 被打断的轮次会按 call_id 丢弃这条迟到结果。
fn start_web_fetch(
    runtime: &CursorToolRuntime,
    results: ToolResultSender,
    fetch: WebFetch,
    pending: PendingInteraction,
) -> Result<()> {
    let url = pending
        .call
        .arguments
        .get("url")
        .and_then(serde_json::Value::as_str)
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| Error::Protocol("WebFetch is missing url".into()))?
        .to_string();
    let detached = runtime.track_detached(&pending.call.call_id);
    let cancelled = detached.token();
    tokio::spawn(async move {
        let _detached = detached;
        let outcome = tokio::select! {
            outcome = fetch.fetch(&url) => outcome.map_err(|error| error.to_string()),
            () = cancelled.cancelled() => Err(INTERRUPTED_FETCH.to_string()),
        };
        match result::complete_web_fetch(pending, outcome) {
            Ok(completion) => results.send(completion),
            Err(error) => results.send_error(error),
        }
    });
    Ok(())
}

fn start_web_search(
    runtime: &CursorToolRuntime,
    results: ToolResultSender,
    search: WebSearch,
    pending: PendingInteraction,
) -> Result<()> {
    // Claude Code 习惯的 query 作为 search_term 的别名兼容。
    let query = ["search_term", "query"]
        .iter()
        .find_map(|name| {
            pending
                .call
                .arguments
                .get(name)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
        })
        .ok_or_else(|| Error::Protocol("WebSearch is missing search_term".into()))?
        .to_string();
    let detached = runtime.track_detached(&pending.call.call_id);
    let cancelled = detached.token();
    tokio::spawn(async move {
        let _detached = detached;
        let outcome = tokio::select! {
            outcome = search.search(&query) => outcome.map_err(|error| error.to_string()),
            () = cancelled.cancelled() => Err(INTERRUPTED_SEARCH.to_string()),
        };
        match result::complete_web_search(pending, outcome) {
            Ok(completion) => results.send(completion),
            Err(error) => results.send_error(error),
        }
    });
    Ok(())
}
