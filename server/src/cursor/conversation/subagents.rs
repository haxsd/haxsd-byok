//! Wakes a parent conversation when one of its background subagents finishes.
//!
//! 官方把这一步放在宿主（也就是服务端）里：宿主自己发现后台任务结束、写进会话事件流，
//! 然后自己发起一个唤醒轮（`queueHostOwnedBackgroundWakeup`）。客户端只在"用户自己在
//! 子代理里打字"这类交互式子代理上才会发完成通知，代理派出的后台子代理不会——
//! 所以 BYOK 这边必须由我们补上这一步，否则父轮收尾之后没人再想起子代理。

use crate::{
    cursor::{
        compile::{project_background_completion, subagent_completion, wakeup_message},
        protocol::proto::agent::v1 as pb,
        transport::TransportHandle,
    },
    model::{CanonicalMessage, ConversationId, MessageContent},
};

use super::{CompiledMessages, ConversationRegistry, MessageDelivery, TransportFinish};

/// 子代理这次跑完的结局，翻译成协议里的后台任务状态。
fn completion_status(finish: &TransportFinish) -> pb::BackgroundTaskStatus {
    match finish {
        TransportFinish::Success => pb::BackgroundTaskStatus::Success,
        TransportFinish::Failed(_) => pb::BackgroundTaskStatus::Error,
        TransportFinish::Cancelled => pb::BackgroundTaskStatus::Aborted,
    }
}

/// 子代理的收尾文本：最后一条有正文的助手消息。
fn final_text(messages: &[CanonicalMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find_map(|message| match &message.content {
            MessageContent::Assistant { text, .. } if !text.trim().is_empty() => {
                Some(text.trim().to_owned())
            }
            _ => None,
        })
}

/// 子代理运行结束时通知父会话。
///
/// 父会话此刻可能在等它（这一轮挂在收尾前）、可能在跑别的轮次、也可能完全空闲；
/// `deliver` 三种情况都接得住：插进活跃轮次、或排进 pending 等下一轮开头吃进去。
/// 只有后台子代理值得通知：阻塞式 Task 的结果由父轮自己拿到（判据见
/// `Store::background_task`）。
pub(super) async fn subagent_finished(
    registry: &ConversationRegistry,
    handle: &TransportHandle,
    finish: &TransportFinish,
) {
    let Some(parent) = handle.parent() else {
        return;
    };
    let Some(tool_call_id) = parent.tool_call_id.as_deref() else {
        return;
    };
    let Some(child_conversation) = handle.conversation_id() else {
        return;
    };
    let store = &registry.dependencies().store;
    let (parent_conversation, title) = match store.background_task(tool_call_id).await {
        Ok(Some(task)) => task,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(
                %error,
                tool_call_id,
                "failed to look up the background task of a finished subagent"
            );
            return;
        }
    };
    let detail = match store
        .load_current_messages(&ConversationId::new(child_conversation))
        .await
    {
        Ok(messages) => final_text(&messages),
        Err(error) => {
            tracing::warn!(%error, "failed to read the subagent's final message");
            None
        }
    };
    let action = subagent_completion(
        child_conversation,
        tool_call_id,
        &title,
        detail.as_deref(),
        completion_status(finish),
    );
    // 这个 mode 只进模拟轮的 turn user，而服务端唤醒插进的是已经存在的轮次，
    // 用不上那份 turn user。
    let projection = match project_background_completion(&action, pb::AgentMode::Agent as i32) {
        Ok(projection) => projection,
        Err(error) => {
            tracing::warn!(%error, "failed to build the parent wakeup message");
            return;
        }
    };
    let event_id = format!(
        "background-completed:{}:{}:{}",
        pb::BackgroundTaskKind::Subagent.as_str_name(),
        child_conversation,
        tool_call_id
    );
    let message = match wakeup_message(&event_id, &projection.context) {
        Ok(message) => message,
        Err(error) => {
            tracing::warn!(%error, "failed to build the parent wakeup message");
            return;
        }
    };
    let result = registry
        .deliver(
            &parent_conversation,
            CompiledMessages {
                event_id,
                target_run_id: None,
                messages: vec![message],
                delivery: MessageDelivery::InsertMessages,
            },
        )
        .await;
    tracing::info!(
        conversation_id = parent_conversation.as_str(),
        subagent_id = child_conversation,
        tool_call_id,
        title,
        ?result,
        "subagent finished; notified the parent conversation"
    );
}
