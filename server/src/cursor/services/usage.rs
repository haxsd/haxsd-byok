//! Builds Cursor usage and context breakdown data.
use std::collections::{HashMap, HashSet};

use crate::{
    cursor::protocol::proto::agent::v1 as pb,
    model::{CanonicalMessage, ContentPart, MessageContent, Origin, ToolDefinition},
    Result,
};

const CATEGORIES: [(&str, &str); 8] = [
    ("system_prompt", "System prompt"),
    ("tools", "Tool definitions"),
    ("rules", "Rules"),
    ("skills", "Skills"),
    ("mcp", "MCP & dynamic tools"),
    ("subagents", "Subagent definitions"),
    ("summarized_conversation", "Summarized conversation"),
    ("conversation", "Conversation"),
];
const SYSTEM: usize = 0;
const TOOLS: usize = 1;
const RULES: usize = 2;
const SKILLS: usize = 3;
const MCP: usize = 4;
const SUBAGENTS: usize = 5;
const SUMMARY: usize = 6;
const CONVERSATION: usize = 7;

#[derive(Clone, Copy, Default)]
struct Measure {
    characters: u64,
    token_units: u64,
}

impl Measure {
    fn add(&mut self, text: &str) {
        self.characters += text.encode_utf16().count() as u64;
        let mut units = 0_u64;
        for character in text.chars() {
            let width = character.len_utf16() as u64;
            units += if character.is_ascii() {
                width * 273
            } else {
                width * 550
            };
        }
        self.token_units += units;
    }

    fn estimated_tokens(self) -> u64 {
        self.token_units.div_ceil(1_000)
    }
}

/// 与历史无关的那部分量测:系统提示 + 工具定义(按动态工具分到 tools / mcp)。
///
/// 它们在一次请求里是固定的 —— 只随 `CheckpointBuilder::configure` 变 —— 而 `breakdown`
/// 每个 checkpoint 都要跑一次:留在里面就等于每次提交都把全部工具重新序列化一遍、把几十 KB
/// 的系统提示重新量一遍。这里改成"配置时算一次、之后直接消费";与按消息 id 缓存的
/// `MessageMeasureCache` 合起来,每次 checkpoint 只剩 O(消息数) 的查表加法。
#[derive(Clone, Copy, Default)]
pub(crate) struct StaticMeasures {
    measures: [Measure; 8],
}

impl StaticMeasures {
    pub(crate) fn new(
        instructions: &str,
        tools: &[ToolDefinition],
        dynamic_tools: &HashSet<String>,
    ) -> Result<Self> {
        let mut measures = [Measure::default(); 8];
        measures[SYSTEM].add(instructions);
        for tool in tools {
            let encoded = serde_json::to_string(tool)?;
            if dynamic_tools.contains(&tool.name) {
                measures[MCP].add(&encoded);
            } else {
                measures[TOOLS].add(&encoded);
            }
        }
        Ok(Self { measures })
    }
}

/// 按消息 id 缓存"这条消息给每个类别贡献了多少"。
///
/// 每个 checkpoint 都重建整份 breakdown，而写进历史的消息不会再变：没有缓存时每次
/// 提交都要把整条历史重新序列化一遍（工具结果正文常常是几十 KB），长会话里每个提交
/// 的成本随历史长度线性增长。累加是纯加法，按消息拆开再相加与整段累加结果相同；
/// 消息身份在库里是"同 id 必须同内容"（`put_message_tx`），所以按 id 缓存是安全的。
#[derive(Clone, Default)]
pub(crate) struct MessageMeasureCache {
    entries: HashMap<String, Box<[Measure; 8]>>,
}

impl MessageMeasureCache {
    fn measures(&mut self, message: &CanonicalMessage) -> Result<[Measure; 8]> {
        if let Some(cached) = self.entries.get(&message.message_id) {
            return Ok(**cached);
        }
        let mut measures = [Measure::default(); 8];
        measure_message(message, &mut measures)?;
        self.entries
            .insert(message.message_id.clone(), Box::new(measures));
        Ok(measures)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

pub(crate) fn breakdown(
    used_tokens: u32,
    max_tokens: u32,
    baseline: Option<&pb::PromptTokenBreakdownSnapshot>,
    statics: StaticMeasures,
    messages: &[CanonicalMessage],
    cache: &mut MessageMeasureCache,
) -> Result<pb::PromptTokenBreakdownSnapshot> {
    let mut measures = statics.measures;
    for message in messages {
        let contribution = cache.measures(message)?;
        for (index, measure) in contribution.iter().enumerate() {
            measures[index].characters += measure.characters;
            measures[index].token_units += measure.token_units;
        }
    }

    let mut estimates = [0_u64; 8];
    for index in 0..CONVERSATION {
        estimates[index] = measures[index].estimated_tokens();
    }
    if measures[SUMMARY].characters != 0 {
        estimates[SUMMARY] = measures[SUMMARY].estimated_tokens();
    } else if let Some(summary) = baseline.and_then(|snapshot| {
        snapshot
            .categories
            .iter()
            .find(|category| category.id == CATEGORIES[SUMMARY].0)
    }) {
        measures[SUMMARY].characters = summary.character_count.unwrap_or(0) as u64;
        estimates[SUMMARY] = summary.estimated_tokens as u64;
    }
    let categorized_tokens = used_tokens as u64;
    fit_special_estimates(&mut estimates, categorized_tokens);
    estimates[CONVERSATION] =
        categorized_tokens.saturating_sub(estimates[..CONVERSATION].iter().sum::<u64>());

    let categories = CATEGORIES
        .iter()
        .enumerate()
        .map(|(index, (id, label))| pb::PromptTokenBreakdownCategory {
            id: (*id).into(),
            label: (*label).into(),
            estimated_tokens: estimates[index].min(u32::MAX as u64) as u32,
            character_count: (measures[index].characters != 0)
                .then_some(measures[index].characters.min(u32::MAX as u64) as u32),
        })
        .collect::<Vec<_>>();
    Ok(pb::PromptTokenBreakdownSnapshot {
        total_used_tokens: used_tokens,
        max_tokens,
        categories,
    })
}

fn measure_message(message: &CanonicalMessage, measures: &mut [Measure; 8]) -> Result<()> {
    match &message.content {
        MessageContent::Parts { parts } => {
            for part in parts {
                if let ContentPart::Text { text } = part {
                    if message.origin == Origin::Runtime {
                        measure_runtime(text, measures);
                    } else {
                        measures[CONVERSATION].add(text);
                    }
                }
            }
        }
        MessageContent::Assistant {
            text,
            thinking,
            tool_calls,
            ..
        } => {
            measures[CONVERSATION].add(text);
            measures[CONVERSATION].add(thinking);
            measures[CONVERSATION].add(&serde_json::to_string(tool_calls)?);
        }
        MessageContent::ToolResult(result) => {
            measures[CONVERSATION].add(&serde_json::to_string(result)?);
        }
    }
    Ok(())
}

fn measure_runtime(text: &str, measures: &mut [Measure; 8]) {
    let mut ranges = Vec::new();
    collect_ranges(text, "rules", RULES, &mut ranges);
    collect_ranges(text, "rule", RULES, &mut ranges);
    collect_ranges(text, "agent_skills", SKILLS, &mut ranges);
    collect_ranges(text, "skill", SKILLS, &mut ranges);
    collect_ranges(text, "subagents", SUBAGENTS, &mut ranges);
    collect_ranges(text, "mcp_meta_tools", MCP, &mut ranges);
    collect_ranges(text, "conversation_summary", SUMMARY, &mut ranges);
    ranges.sort_by_key(|range| range.0);

    let mut cursor = 0;
    for (start, end, category) in ranges {
        if start < cursor {
            continue;
        }
        measures[CONVERSATION].add(&text[cursor..start]);
        measures[category].add(&text[start..end]);
        cursor = end;
    }
    measures[CONVERSATION].add(&text[cursor..]);
}

fn collect_ranges(text: &str, tag: &str, category: usize, output: &mut Vec<(usize, usize, usize)>) {
    let opening = format!("<{tag}");
    let closing = format!("</{tag}>");
    let mut cursor = 0;
    while let Some(relative_start) = text[cursor..].find(&opening) {
        let start = cursor + relative_start;
        let Some(open_end) = text[start..].find('>').map(|offset| start + offset + 1) else {
            break;
        };
        let Some(relative_end) = text[open_end..].find(&closing) else {
            break;
        };
        let end = open_end + relative_end + closing.len();
        output.push((start, end, category));
        cursor = end;
    }
}

fn fit_special_estimates(estimates: &mut [u64; 8], total: u64) {
    let special_total = estimates[..CONVERSATION].iter().sum::<u64>();
    if special_total <= total || special_total == 0 {
        return;
    }
    let original = *estimates;
    let mut assigned = 0;
    for index in 0..CONVERSATION {
        estimates[index] = original[index].saturating_mul(total) / special_total;
        assigned += estimates[index];
    }
    let mut remainder = total - assigned;
    let mut order = (0..CONVERSATION).collect::<Vec<_>>();
    order.sort_by_key(|index| {
        std::cmp::Reverse(original[*index].saturating_mul(total) % special_total)
    });
    for index in order {
        if remainder == 0 {
            break;
        }
        estimates[index] += 1;
        remainder -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    fn runtime_message(id: &str, text: &str) -> CanonicalMessage {
        CanonicalMessage {
            message_id: id.into(),
            role: Role::User,
            origin: Origin::Runtime,
            runtime_event_id: None,
            content: MessageContent::Parts {
                parts: vec![ContentPart::Text { text: text.into() }],
            },
        }
    }

    /// 按消息缓存量测：第二次重建不再重算已知消息，但结果必须与第一次逐字节一致。
    #[test]
    fn a_warm_measure_cache_reproduces_the_same_breakdown() {
        let messages = vec![
            runtime_message("m1", "<rules>\nrule body\n</rules>\nhello"),
            runtime_message("m2", "plain conversation text"),
        ];
        let statics = StaticMeasures::new("system prompt", &[], &HashSet::new()).expect("statics");
        let mut cache = MessageMeasureCache::default();
        let first =
            breakdown(1_000, 8_000, None, statics, &messages, &mut cache).expect("breakdown");
        assert_eq!(cache.len(), messages.len(), "每条消息各留一份量测");
        let second =
            breakdown(1_000, 8_000, None, statics, &messages, &mut cache).expect("breakdown");
        assert_eq!(first, second);
        assert_eq!(cache.len(), messages.len(), "已知消息不再重量测");
    }

    /// 与历史无关的那半(系统提示 + 工具定义)由调用方算一次:`StaticMeasures` 的结果必须与
    /// 逐个工具现场量测一致,动态工具要落到 mcp 那一类。
    #[test]
    fn the_static_measures_match_measuring_the_prompt_and_tools_directly() {
        let tools = vec![tool("Read"), tool("Edit"), tool("semble_search")];
        let dynamic = HashSet::from(["semble_search".to_string()]);
        let statics = StaticMeasures::new("system prompt", &tools, &dynamic).expect("statics");

        let mut expected = [Measure::default(); 8];
        expected[SYSTEM].add("system prompt");
        for tool in &tools {
            let encoded = serde_json::to_string(tool).unwrap();
            if dynamic.contains(&tool.name) {
                expected[MCP].add(&encoded);
            } else {
                expected[TOOLS].add(&encoded);
            }
        }

        let measured = breakdown(
            1_000,
            8_000,
            None,
            statics,
            &[],
            &mut MessageMeasureCache::default(),
        )
        .expect("breakdown");
        let category = |id: &str| {
            measured
                .categories
                .iter()
                .find(|category| category.id == id)
                .cloned()
                .expect("category")
        };
        assert_eq!(
            category("system_prompt").character_count,
            Some(expected[SYSTEM].characters as u32)
        );
        assert_eq!(
            category("tools").character_count,
            Some(expected[TOOLS].characters as u32),
            "工具定义算在 tools 里"
        );
        assert_eq!(
            category("mcp").character_count,
            Some(expected[MCP].characters as u32),
            "动态工具算在 mcp 里"
        );
    }

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("{name} the thing"),
            parameters: serde_json::json!({ "type": "object" }),
        }
    }

    /// 缓存必须真的在起作用：同一 id 第二次直接读缓存（库里同 id 必须同内容，这条断言
    /// 把该前提写进测试——真出现"同 id 换了正文"，这里会先红）。
    #[test]
    fn a_known_message_is_measured_only_once() {
        let mut cache = MessageMeasureCache::default();
        let original = runtime_message("m1", "<rules>\nrule body\n</rules>");
        let cached = cache.measures(&original).expect("measure");
        let changed = runtime_message("m1", "<rules>\na much longer rule body\n</rules>");
        let served = cache.measures(&changed).expect("measure");
        assert_eq!(
            cached.iter().map(|measure| measure.characters).sum::<u64>(),
            served.iter().map(|measure| measure.characters).sum::<u64>()
        );
        assert_eq!(cache.len(), 1, "同 id 只留一份量测");
    }

    #[test]
    fn breakdown_contains_only_real_context_categories() {
        let snapshot = breakdown(
            12_000,
            200_000,
            None,
            StaticMeasures::default(),
            &[],
            &mut MessageMeasureCache::default(),
        )
        .expect("breakdown");
        assert_eq!(
            snapshot
                .categories
                .iter()
                .map(|category| category.id.as_str())
                .collect::<Vec<_>>(),
            CATEGORIES.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "上下文构成只应显示真实类别，不应夹带来源项目标记"
        );
        assert_eq!(
            snapshot
                .categories
                .iter()
                .map(|category| u64::from(category.estimated_tokens))
                .sum::<u64>(),
            12_000,
            "所有分类加起来必须等于实际上下文用量"
        );
    }
}
