//! 记录“客户端运行时/协议和网关能力不匹配”的现场，供状态接口与日志诊断使用。
//!
//! 典型事件：Cursor 3.29+ 的 Agent Host 在 `agent_host_local_loop` 运行时下用
//! `InferenceService/RunInference` 调本地模型，网关没有实现本地推理桥；或者客户端
//! 升级后 BidiAppend 换成网关还不认识的消息。两者都属于“协议回退”信号：
//! 状态接口把它暴露给界面，用户重启 Cursor 后门禁会重新拉到 legacy 运行时。
use std::{
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, serde::Serialize)]
pub struct RuntimeMismatch {
    /// 事件类型：`inference_local_model` / `unknown_append`。
    pub kind: &'static str,
    pub model_id: Option<String>,
    pub conversation_id: Option<String>,
    pub detected_at_ms: u64,
    pub count: u64,
}

#[derive(Default)]
struct State {
    mismatch: Option<RuntimeMismatch>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// 记录一次不匹配。同一 `kind` 重复出现只累加计数并刷新时间；日志只在前两次与每 10 次
/// 打一条，避免一个卡住的会话把日志刷满，同时保留首次与持续的现场。
pub fn record_runtime_mismatch(
    kind: &'static str,
    model_id: Option<&str>,
    conversation_id: Option<&str>,
) -> u64 {
    let now = now_ms();
    let count = {
        let mut guard = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.mismatch.as_mut() {
            Some(existing) if existing.kind == kind => {
                existing.count += 1;
                existing.detected_at_ms = now;
                if model_id.is_some() {
                    existing.model_id = model_id.map(str::to_owned);
                }
                if conversation_id.is_some() {
                    existing.conversation_id = conversation_id.map(str::to_owned);
                }
                existing.count
            }
            _ => {
                guard.mismatch = Some(RuntimeMismatch {
                    kind,
                    model_id: model_id.map(str::to_owned),
                    conversation_id: conversation_id.map(str::to_owned),
                    detected_at_ms: now,
                    count: 1,
                });
                1
            }
        }
    };
    if count <= 2 || count % 10 == 0 {
        tracing::warn!(
            kind,
            count,
            model_id = model_id.unwrap_or(""),
            conversation_id = conversation_id.unwrap_or(""),
            "client runtime is not supported by this gateway; legacy runtime must be re-selected"
        );
    }
    count
}

pub fn runtime_mismatch() -> Option<RuntimeMismatch> {
    state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .mismatch
        .clone()
}

/// 只给测试与“用户重启 Cursor 后重置”用。
pub fn clear() {
    state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .mismatch = None;
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全局状态被多个用例共享，测试串行执行（单测二进制里只留一个 guard 用例）。
    #[test]
    fn repeated_mismatches_accumulate_and_keep_the_latest_context() {
        clear();
        assert!(runtime_mismatch().is_none());

        assert_eq!(
            record_runtime_mismatch("inference_local_model", Some("model-a"), Some("conv-a")),
            1
        );
        let first = runtime_mismatch().expect("first mismatch is recorded");
        assert_eq!(first.kind, "inference_local_model");
        assert_eq!(first.model_id.as_deref(), Some("model-a"));
        assert_eq!(first.conversation_id.as_deref(), Some("conv-a"));

        assert_eq!(
            record_runtime_mismatch("inference_local_model", Some("model-b"), None),
            2
        );
        let second = runtime_mismatch().expect("second mismatch updates the snapshot");
        assert_eq!(second.count, 2);
        assert_eq!(second.model_id.as_deref(), Some("model-b"));
        assert_eq!(
            second.conversation_id.as_deref(),
            Some("conv-a"),
            "缺省字段保留上一次的现场"
        );

        // 换一种 kind 表示新的事件类型，计数从 1 重新开始。
        assert_eq!(record_runtime_mismatch("unknown_append", None, None), 1);
        let third = runtime_mismatch().unwrap();
        assert_eq!(third.kind, "unknown_append");
        assert_eq!(third.count, 1);
        clear();
    }
}
