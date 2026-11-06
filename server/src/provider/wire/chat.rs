//! Chat Completions 的共享判定：分片归并、`finish_reason` 分类。

use serde::{Deserialize, Serialize};

/// `finish_reason` 的语义分类。
///
/// 两个调用方对同一取值的处理不同：provider 把未知/失败都折成 `Stop`，Codex 桥接把
/// `content_filter` 标成未完成、把失败值报错。分类只回答“它是什么”，怎么处理交给调用方。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ChatFinish {
    Stop,
    Length,
    ToolUse,
    ContentFilter,
    Failed,
}

pub(crate) fn chat_finish(reason: &str) -> ChatFinish {
    match reason.to_ascii_lowercase().as_str() {
        "tool_calls" | "function_call" => ChatFinish::ToolUse,
        "length" => ChatFinish::Length,
        "content_filter" => ChatFinish::ContentFilter,
        // 只认“确定是失败”的词；其余自造成功值按完成处理。真正的失败还有兜底：
        // 这一轮没有任何可用输出时，调用方仍会报错。
        failed
            if failed.contains("fail") || failed.contains("error") || failed.contains("cancel") =>
        {
            ChatFinish::Failed
        }
        _ => ChatFinish::Stop,
    }
}

/// 标识符（`call_id`、函数名）的分片归并。
///
/// 标识符整段下发，重复只可能是旧值或完整重发；当前值已经是分片前缀时保留当前值。
pub(crate) fn merge_identifier(target: &mut String, fragment: &str) {
    merge_fragment(target, fragment, true);
}

/// 工具参数的分片归并。
///
/// 参数是流式 JSON 文档，左花括号、双引号、左方括号这类短分片本身就是内容，不能当成
/// 旧整串丢掉；只有“整串相同”与“分片是当前值的完整超集”才按重发处理。
pub(crate) fn merge_arguments(target: &mut String, fragment: &str) {
    merge_fragment(target, fragment, false);
}

fn merge_fragment(target: &mut String, fragment: &str, ignore_older_snapshot: bool) {
    if target.as_str() == fragment {
        return;
    }
    if ignore_older_snapshot && target.starts_with(fragment) {
        return;
    }
    if fragment.starts_with(target.as_str()) {
        target.push_str(&fragment[target.len()..]);
        return;
    }
    target.push_str(fragment);
}
