//! 上游 wire 格式的共享判定。
//!
//! 同一个概念在两个适配器里各写一遍时，修复会漏在另一条路径上：工具结果的图片、
//! 分片归并、finish/stop 的未知取值、思考预算的下界都出过这种问题。这里只放**纯判定**
//! ——字符串怎么归并、上游标记对应哪种语义、预算能不能装下——不持有事件类型，
//! 由 provider 与 Codex 桥接各自映射到自己的输出。

pub(crate) mod anthropic;
pub(crate) mod chat;

#[cfg(test)]
mod tests;
