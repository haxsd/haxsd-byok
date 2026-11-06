use serde::Deserialize;

use super::{anthropic, chat};

#[derive(Deserialize)]
struct Cases {
    merge_identifier: Vec<MergeCase>,
    merge_arguments: Vec<MergeCase>,
    chat_finish: Vec<ChatCase>,
    anthropic_stop: Vec<StopCase>,
    thinking_budget: Vec<BudgetCase>,
}

#[derive(Deserialize)]
struct MergeCase {
    target: String,
    fragment: String,
    expect: String,
}

#[derive(Deserialize)]
struct ChatCase {
    reason: String,
    expect: chat::ChatFinish,
}

#[derive(Deserialize)]
struct StopCase {
    reason: String,
    expect: anthropic::AnthropicStop,
}

#[derive(Deserialize)]
struct BudgetCase {
    configured: Option<u64>,
    max_tokens: u64,
    expect: Option<u64>,
}

/// 同一份表也被插件 SDK 的 Deno 用例读取；两边对同一行为的断言必须一致。
#[test]
fn the_shared_wire_cases_hold() {
    let cases: Cases =
        serde_json::from_str(include_str!("cases.json")).expect("wire cases must be valid JSON");

    for case in cases.merge_identifier {
        let mut target = case.target;
        chat::merge_identifier(&mut target, &case.fragment);
        assert_eq!(target, case.expect, "identifier {:?}", case.fragment);
    }
    for case in cases.merge_arguments {
        let mut target = case.target;
        chat::merge_arguments(&mut target, &case.fragment);
        assert_eq!(target, case.expect, "arguments {:?}", case.fragment);
    }
    for case in cases.chat_finish {
        assert_eq!(
            chat::chat_finish(&case.reason),
            case.expect,
            "chat finish {:?}",
            case.reason
        );
    }
    for case in cases.anthropic_stop {
        assert_eq!(
            anthropic::anthropic_stop(&case.reason),
            case.expect,
            "stop reason {:?}",
            case.reason
        );
    }
    for case in cases.thinking_budget {
        assert_eq!(
            anthropic::thinking_budget(case.configured, case.max_tokens),
            case.expect,
            "budget {:?} / {}",
            case.configured,
            case.max_tokens
        );
    }
}
