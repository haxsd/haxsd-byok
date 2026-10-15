//! Codex Responses 与第三方上游之间的纯协议转换；不做网络、不执行工具、不落盘。
//!
//! - `request`：Responses 请求 → Chat Completions / Anthropic Messages 请求。保留完整历史、
//!   工具声明、tool_choice、parallel、图像与推理回放；无法表达的语义明确报错。
//! - `response`：上游 SSE 分片 → Responses 事件与终态 response。异常结束永远不会变成 completed。
//! - `tools`：function / custom / namespace → 上游工具名的可逆映射。
//! - `replay`：跨轮推理状态（Chat 的 reasoning_content、Anthropic 的签名 thinking）的归属封装。
mod replay;
mod request;
mod response;
mod tools;

pub use request::translate_request;
pub(crate) use request::translate_request_owned;
pub use response::BridgeStream;
pub use tools::ToolMap;

/// 桥接目标上游使用的协议。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamFormat {
    /// OpenAI 兼容的 Chat Completions。
    Chat,
    /// Anthropic Messages。
    Anthropic,
}

/// `translate_request` 的结果：上游请求正文，以及响应侧还原工具名所需的映射。
#[derive(Debug)]
pub struct BridgeRequest {
    pub body: serde_json::Value,
    pub tools: ToolMap,
}
