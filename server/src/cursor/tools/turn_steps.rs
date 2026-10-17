//! Tracks how many steps the current turn already occupies in the projected conversation.
use crate::model::{CanonicalMessage, MessageContent, Role};

/// 一个工具轮的 assistant 内容在投影里占几个 step:思考、正文、每个工具调用各一个。
///
/// 口径必须与 [`turn_step_count`] 里 `MessageContent::Assistant` 那一支一字不差 ——
/// 一个工具轮在库里会被拆成"每个调用一条 assistant 消息"(首条带 text/thinking,见
/// `commit_tool_result`),它们的贡献之和正好是这里算出来的数。
pub(crate) fn round_steps(thinking: &str, text: &str, tool_calls: usize) -> usize {
    usize::from(!thinking.is_empty()) + usize::from(!text.is_empty()) + tool_calls
}

/// 本轮第一个工具调用在投影里的下标(从 1 开始):已提交的 step 数 + 本轮在它前面的
/// 思考/正文 + 1。
pub(crate) fn first_tool_index(turn_steps: usize, thinking: &str, text: &str) -> usize {
    turn_steps + round_steps(thinking, text, 0) + 1
}

/// 按"库里已提交的历史"数出当前 turn 的 step 数:最后一个 user 消息之后的 assistant 步骤。
///
/// 这是 `TurnSteps::Unknown` 时的回落算法(也是改动前每个工具轮都在跑的那套)。
pub(crate) fn turn_step_count(messages: &[CanonicalMessage]) -> usize {
    let turn_start = messages
        .iter()
        .rposition(|message| message.role == Role::User)
        .map_or(0, |position| position + 1);
    messages[turn_start..]
        .iter()
        .map(|message| match &message.content {
            MessageContent::Assistant {
                text,
                thinking,
                tool_calls,
                ..
            } => round_steps(thinking, text, tool_calls.len()),
            _ => 0,
        })
        .sum()
}

/// 当前 turn 已提交的 step 数 —— 下一个工具轮算 `message_index` 的基础值。
///
/// 旧实现每个工具轮都 `store.load_current_messages()` 把整段历史拉回来重数一遍(P3-5)。
/// 这里把它变成"形状可能变过就装载一次、之后每个工具轮只加自己那轮的贡献":
/// - [`TurnSteps::Unknown`]:历史形状变过(新 turn / 压缩 / 插话),下次工具轮装一次历史;
/// - [`TurnSteps::Known`]:已知值,每个已派发的工具轮用 [`TurnSteps::advance`] 累加。
///
/// 不变式:`Known(n)` 必须等于 `turn_step_count(load_current_messages())`。所有会改变历史
/// 形状的提交都把状态打回 `Unknown` 而不是去猜,所以真出错也只会多装载一次,不会偏移。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TurnSteps {
    #[default]
    Unknown,
    Known(usize),
}

impl TurnSteps {
    pub(crate) fn known(self) -> Option<usize> {
        match self {
            Self::Unknown => None,
            Self::Known(steps) => Some(steps),
        }
    }

    /// 记下"按历史数出来"的结果。
    pub(crate) fn remember(&mut self, steps: usize) {
        *self = Self::Known(steps);
    }

    /// 历史形状变过:下一次工具轮重新按历史数。
    pub(crate) fn invalidate(&mut self) {
        *self = Self::Unknown;
    }

    /// 本轮派发之后,这一轮的 assistant 消息会在工具结果提交时进入历史(一个调用一条,
    /// 首条带 text/thinking),所以下一轮的基础值 = 本轮基础 + 本轮贡献。
    ///
    /// 前提是调用点在一轮**结束后**才推进(引擎在 `interrupt_remaining` 里为每个未回报的
    /// 调用补 interrupted 结果,下一轮开始前这一轮必然已经全部落库)。
    pub(crate) fn advance(&mut self, thinking: &str, text: &str, tool_calls: usize) {
        if let Self::Known(steps) = *self {
            *self = Self::Known(steps + round_steps(thinking, text, tool_calls));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Origin, ToolCallContent, ToolResultContent};

    fn user(id: &str) -> CanonicalMessage {
        CanonicalMessage {
            message_id: id.into(),
            role: Role::User,
            origin: Origin::User,
            runtime_event_id: None,
            content: MessageContent::Parts { parts: Vec::new() },
        }
    }

    /// 库里一个工具轮的样子:每个调用一条 assistant 消息(首条带 text/thinking,各带一个
    /// 调用),后面跟一条 tool 结果。
    fn committed_round(
        round: &str,
        thinking: &str,
        text: &str,
        calls: usize,
    ) -> Vec<CanonicalMessage> {
        let mut messages = Vec::new();
        for index in 0..calls {
            let first = index == 0;
            messages.push(CanonicalMessage {
                message_id: format!("{round}:{index}:assistant"),
                role: Role::Assistant,
                origin: Origin::Assistant,
                runtime_event_id: None,
                content: MessageContent::Assistant {
                    text: if first { text.into() } else { String::new() },
                    thinking: if first {
                        thinking.into()
                    } else {
                        String::new()
                    },
                    tool_round_id: Some(round.into()),
                    replay_state: None,
                    tool_calls: vec![ToolCallContent {
                        index,
                        call_id: format!("{round}-call-{index}"),
                        name: "Read".into(),
                        arguments: serde_json::json!({}),
                    }],
                },
            });
            messages.push(CanonicalMessage {
                message_id: format!("{round}:{index}:result"),
                role: Role::Tool,
                origin: Origin::Tool,
                runtime_event_id: None,
                content: MessageContent::ToolResult(ToolResultContent {
                    call_id: format!("{round}-call-{index}"),
                    name: "Read".into(),
                    content: "ok".into(),
                    is_error: false,
                    image: None,
                    provider_parts: Vec::new(),
                }),
            });
        }
        messages
    }

    /// 对拍:同一轮 call 序列下,增量算出的 `message_index` 与"每轮重新装载历史再数"的
    /// 旧算法逐值相同 —— 这正是 P3-5 唯一不能变的东西。
    #[test]
    fn the_incremental_step_count_matches_recounting_the_history() {
        let rounds = [
            ("r1", "thinking one", "answer one", 2),
            ("r2", "", "answer two", 1),
            ("r3", "thinking three", "", 3),
            ("r4", "", "", 1),
        ];
        let mut history = vec![user("u1")];
        let mut state = TurnSteps::Unknown;
        // 第一次工具轮:按历史数一次,之后只累加。
        state.remember(turn_step_count(&history));

        for (round, thinking, text, calls) in rounds {
            let base = state
                .known()
                .expect("known after the first turn step count");

            // 旧算法:装载此刻的历史重新数。
            let recounted = turn_step_count(&history);
            assert_eq!(base, recounted, "{round} 开始前的基础值必须一致");
            assert_eq!(
                first_tool_index(base, thinking, text),
                first_tool_index(recounted, thinking, text),
                "{round} 的 message_index 必须逐值一致"
            );

            // 本轮结束后历史里多出这一轮,下一轮的基础值往前推进。
            state.advance(thinking, text, calls);
            history.extend(committed_round(round, thinking, text, calls));
            assert_eq!(
                state.known(),
                Some(turn_step_count(&history)),
                "{round} 落库之后基础值必须追平历史"
            );
        }
    }

    /// 逆变:累加时漏掉 thinking 或 text 之一,下一轮的索引就会偏 —— 这条用例把"漏掉就红"
    /// 钉死(不是在实现里写死差值,而是用同一套函数故意少加一项再断言不等)。
    #[test]
    fn dropping_thinking_or_text_from_the_accumulation_shifts_the_next_index() {
        let history = committed_round("r1", "thinking one", "answer one", 2);
        let mut correct = TurnSteps::Unknown;
        correct.remember(turn_step_count(&[]));
        correct.advance("thinking one", "answer one", 2);

        let mut without_thinking = TurnSteps::Unknown;
        without_thinking.remember(turn_step_count(&[]));
        without_thinking.advance("", "answer one", 2);

        let mut without_text = TurnSteps::Unknown;
        without_text.remember(turn_step_count(&[]));
        without_text.advance("thinking one", "", 2);

        let expected = turn_step_count(&history);
        assert_eq!(correct.known(), Some(expected));
        assert_eq!(without_thinking.known(), Some(expected - 1));
        assert_eq!(without_text.known(), Some(expected - 1));
        assert_ne!(
            first_tool_index(without_thinking.known().unwrap(), "", "next"),
            first_tool_index(expected, "", "next"),
            "漏掉 thinking 之后下一轮的 message_index 必须不同"
        );
    }

    /// 形状变过之后回到"重新数":`invalidate` 之后 `advance` 不能再凭旧值累加。
    #[test]
    fn invalidating_a_known_count_drops_back_to_recounting() {
        let mut state = TurnSteps::Known(7);
        state.advance("thinking", "text", 2);
        assert_eq!(state.known(), Some(11), "思考 + 正文 + 两个调用");

        state.invalidate();
        assert_eq!(state.known(), None, "压缩/新 turn 之后必须重新数");
        state.advance("thinking", "text", 2);
        assert_eq!(state.known(), None, "未知时不许凭猜累加");
    }
}
