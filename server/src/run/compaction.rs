//! Decides when to compact provider-visible context and assembles the replacement history.
//!
//! A compaction keeps three things: the latest request context, one summary
//! slot, and the newest whole turns replayed verbatim. Everything older is
//! dropped, so the compacted conversation is a valid provider history with a
//! deterministic prefix for the turns that follow it.

use std::collections::HashSet;

use crate::{
    model::{
        estimate_context_tokens, estimate_projected_messages_tokens, project_messages,
        CanonicalMessage, ContentPart, MessageContent, Origin, PreparedRun, ProjectedContent,
        ProjectedMessage, Role,
    },
    store::ContextUsageAnchor,
};

// The window reserve (default 10%) and the post-compaction retention share
// (default 20%) are configurable through `CompactionSettings`; the defaults
// preserve the shipped behaviour. The reserve absorbs the estimate running
// behind the provider: a conversation measured at 948K estimated against
// 1,017,628 actual landed over a 1M window while the check said there was
// room. A proportional reserve scales with the model instead of using a fixed
// 10K buffer.

/// Safety factor applied to the summarizer input budget.
///
/// The trim budget is already the provider's usable window minus the summary
/// output cap; keeping a further slice free absorbs the estimator's remaining
/// error (especially CJK) so the summarize call itself does not overflow.
const SUMMARY_INPUT_SAFETY_PERCENT: u64 = 90;

/// Tighter input budget for the single retry after a failed summarize call.
const SUMMARY_RETRY_PERCENT: u64 = 55;

/// Clamp for the provider-anchored estimate correction. A lower ratio would
/// shrink every downstream budget without bound; an upper one would turn a
/// noisy anchor into an absurd reserve.
const MIN_CALIBRATION: f64 = 0.5;
const MAX_CALIBRATION: f64 = 4.0;

/// How many of the newest tool results the summarizer keeps verbatim when it
/// replays an over-budget history. Everything older is head/tail pruned in the
/// summarize request only; the checkpoint keeps the original content.
const SUMMARY_RECENT_TOOL_RESULTS: usize = 2;

/// Byte budget for one pruned tool result in the summarize replay.
const SUMMARY_TOOL_RESULT_LIMIT: usize = 8 * 1024;

/// Upper bound on the deterministic file anchors attached to a summary slot.
const MAX_ANCHOR_FILES: usize = 12;

/// Images kept verbatim in the retained tail after a compaction. Older images
/// become text placeholders: they rarely matter for the next step, cost
/// provider-specific visual tokens, and the summary/archive keeps their paths.
const MAX_RETAINED_IMAGES: usize = 2;

/// Upper bound on one summary, in output tokens of the summarizer call.
///
/// The summary stays provider-visible until the next compaction, so it competes
/// with the retained turns for the budget above. This bound only stops a
/// rambling model from being cut mid-sentence; `INSTRUCTIONS` asks for a short
/// handover state. Call sites clamp it to the configured model limit, because
/// providers reject a `max_tokens` above the model's own output limit.
pub(super) const OUTPUT_TOKENS: u64 = 8_192;
pub(super) const INSTRUCTIONS: &str = "Summarize this conversation as a short handover state for the next model turn.\n\nReturn plain text with exactly these sections, in order, and keep every heading even when empty (write \"(none)\"):\n## Goal\n- [the user's original and evolving goal; quote exact wording where it matters]\n## Done and Verified\n- [what is already implemented, tested, or confirmed]\n## In Flight\n- [work currently in progress]\n## Next Action\n- [the single next concrete step]\n## Constraints and Decisions\n- [decisions that must not be reversed, user corrections, constraints]\n## Key Files, Commands, Values\n- [exact paths, commands, identifiers, numbers]\n## Open Errors / Unfinished Work\n- [errors, failing checks, explicitly requested work not done]\n## Earlier Summary\n- [merge still-true facts from a prior conversation_summary; do not copy it verbatim]\n\nRules:\n- Prefer durable state over narration of recent turns.\n- Do not address the user, do not mention compaction, summarization, or token limits, and do not call tools.\n- Preserve exact paths, commands, identifiers, numbers, and error strings.\n- Write in the language the user used in the conversation.\n- Return plain text only, in short bullet-like lines.";

/// Summary slot content when the summarizer produced nothing usable.
///
/// It states what happened instead of pretending to summarize content that was
/// dropped, and keeps the Cursor summary state well formed for the client.
pub(super) const FALLBACK_SUMMARY: &str = "This conversation was compacted because the context window was full, and the summary call failed. Older history was dropped; the turns below are kept verbatim. Continue the current task from this state.";

/// Usable prompt budget with the shipped reserve: the window minus the
/// proportional reserve. Kept for tests that do not carry a policy.
#[cfg(test)]
pub(super) fn context_budget(context_window: u64) -> u64 {
    context_budget_with(context_window, 10)
}

/// Usable prompt budget for one reserve percentage.
fn context_budget_with(context_window: u64, reserve_percent: u64) -> u64 {
    let reserve = (u128::from(context_window) * u128::from(reserve_percent) / 100) as u64;
    context_window.saturating_sub(reserve)
}

/// Converts a provider-token budget into the estimator's unit space.
///
/// `calibration` is actual tokens per estimated token for the anchored prefix:
/// a value of 1.5 means the estimator reports two thirds of what the provider
/// charges, so a budget expressed in estimated tokens must shrink to
/// `provider_budget / 1.5` for the real request to fit.
fn calibrate_budget(tokens: u64, calibration: f64) -> u64 {
    (tokens as f64 / calibration).floor().max(0.0) as u64
}

/// Output tokens the next routed request reserves inside the same window.
///
/// Providers that charge `max_tokens` against the context window need this
/// subtracted before deciding whether the prompt still fits. Without a
/// configured output cap the adapter's own default applies; treating it as zero
/// here keeps the old proportional-reserve behaviour for models that do not
/// declare one.
pub(super) fn output_reservation(prepared: &PreparedRun) -> u64 {
    prepared.model.max_output_tokens.unwrap_or(0)
}

/// Usable prompt budget: the window minus the configured proportional reserve
/// and the next request's output reservation.
pub(super) fn input_budget(
    prepared: &PreparedRun,
    settings: &crate::store::CompactionSettings,
) -> Option<u64> {
    let window = prepared
        .model
        .context_window_tokens
        .filter(|window| *window > 0)?;
    Some(
        context_budget_with(window, settings.reserve_percent())
            .saturating_sub(output_reservation(prepared)),
    )
}

/// Extracts the provider's own context limit from an over-limit error.
///
/// Providers phrase the refusal as prose, but the number is usually present:
/// `"prompt is too long: 210000 tokens > 200000 maximum"`, `"maximum context
/// length is 128000 tokens"`, or `"context window is 200000"`. Learning the
/// real limit for the rest of the run prevents the same estimator from
/// re-triggering the same refusal when the configured window is larger than the
/// provider's. Only numbers that look like token windows are accepted.
pub(super) fn provider_context_limit(message: &str) -> Option<u64> {
    let lowered = message.to_ascii_lowercase();
    // "N tokens > M maximum" (Anthropic and several gateways).
    if let Some(index) = lowered.find("> ") {
        if let Some(limit) = first_number(&lowered[index + 2..]) {
            if limit >= 1_000 {
                return Some(limit);
            }
        }
    }
    for marker in [
        "maximum context length is",
        "maximum context window is",
        "maximum context length of",
        "context window is",
        "context length is",
        "token limit is",
    ] {
        if let Some(limit) = first_number_after(&lowered, marker) {
            if limit >= 1_000 {
                return Some(limit);
            }
        }
    }
    None
}

fn first_number_after(text: &str, marker: &str) -> Option<u64> {
    let start = text.find(marker)? + marker.len();
    first_number(&text[start..])
}

fn first_number(text: &str) -> Option<u64> {
    let mut digits = String::new();
    let mut started = false;
    for character in text.chars() {
        if character.is_ascii_digit() || (started && character == ',') {
            started = true;
            if character != ',' {
                digits.push(character);
            }
        } else if started {
            break;
        }
    }
    digits.parse().ok()
}
///
/// Providers report this as a plain 400 with prose, so there is nothing
/// structured to match on. Anthropic says "prompt is too long"; OpenAI-style
/// gateways use `context_length_exceeded` or "maximum context length". The
/// remaining phrases cover the common gateway translations seen in practice;
/// the run engine additionally treats provider errors whose reported input
/// usage already reached the budget as overflow even when no phrase matches.
pub(super) fn is_context_overflow(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("prompt is too long")
        || lowered.contains("context window exceeded")
        || lowered.contains("context_length_exceeded")
        || lowered.contains("context length exceeded")
        || lowered.contains("model_context_window_exceeded")
        || lowered.contains("maximum context length")
        || lowered.contains("exceeds the maximum context")
        || lowered.contains("exceeds the maximum number of tokens")
        || lowered.contains("too many tokens")
        || lowered.contains("token limit exceeded")
        || lowered.contains("reduce the length of the")
        || lowered.contains("input length")
        || lowered.contains("request is too large")
        || lowered.contains("request too large")
}

/// Builds the history for a compaction call.
///
/// Two properties matter, and replaying the raw history guarantees neither:
///
/// 1. The history must end with a user message. Otherwise providers read the
///    request as an assistant prefill and refuse it outright: Anthropic answers
///    "This model does not support assistant message prefill. The conversation
///    must end with a user message."
/// 2. The history must fit the context window. Compaction runs precisely
///    because the conversation is too large, so replaying all of it asks the
///    summarizer to accept a prompt that is already over the limit and the
///    call fails with "prompt is too long".
///
/// Trimming keeps the most recent turns and only ever cuts at a user-message
/// boundary, so an assistant tool call is never separated from its results.
pub(super) fn compaction_history(
    history: Vec<ProjectedMessage>,
    context_window: Option<u64>,
    calibration: f64,
    settings: &crate::store::CompactionSettings,
) -> Vec<ProjectedMessage> {
    compact_history_at(
        history,
        context_window,
        calibration,
        settings,
        SUMMARY_INPUT_SAFETY_PERCENT,
    )
}

/// A tighter summarize history for the single retry after a failed call.
pub(super) fn compaction_history_tight(
    history: Vec<ProjectedMessage>,
    context_window: Option<u64>,
    calibration: f64,
    settings: &crate::store::CompactionSettings,
) -> Vec<ProjectedMessage> {
    compact_history_at(
        history,
        context_window,
        calibration,
        settings,
        SUMMARY_RETRY_PERCENT,
    )
}

fn compact_history_at(
    history: Vec<ProjectedMessage>,
    context_window: Option<u64>,
    calibration: f64,
    settings: &crate::store::CompactionSettings,
    percent: u64,
) -> Vec<ProjectedMessage> {
    super::history::user_terminated(
        trim_to_context(
            history,
            summary_input_budget(context_window, calibration, settings, percent),
        ),
        "compaction:instruction",
        INSTRUCTIONS,
    )
}

/// Estimate-unit budget for the summarize call's replayed history.
fn summary_input_budget(
    context_window: Option<u64>,
    calibration: f64,
    settings: &crate::store::CompactionSettings,
    percent: u64,
) -> Option<u64> {
    let window = context_window.filter(|window| *window > 0)?;
    let usable = context_budget_with(window, settings.reserve_percent())
        .saturating_sub(OUTPUT_TOKENS)
        .saturating_mul(percent)
        / 100;
    let scaled = calibrate_budget(usable, calibration);
    (scaled > 0).then_some(scaled)
}

fn trim_to_context(
    mut history: Vec<ProjectedMessage>,
    budget: Option<u64>,
) -> Vec<ProjectedMessage> {
    let Some(budget) = budget else {
        return history;
    };
    if estimate_projected_messages_tokens(&history) <= budget {
        return history;
    }
    // Level B before level A: clear/prune old tool outputs without a model
    // call. The pruned copy is provider-visible only - the checkpoint keeps the
    // original tool result - so this cannot corrupt Cursor state, and when it
    // makes the replay fit, the summarize prompt keeps whole turns instead of
    // losing them to the turn-boundary trim below.
    history = prune_replayed_tool_results(history);
    if estimate_projected_messages_tokens(&history) <= budget {
        return history;
    }
    // Walk back from the newest turn, keeping whole user-delimited turns.
    let mut kept = 0;
    let mut newest_turn = None;
    for (index, message) in history.iter().enumerate().rev() {
        if !is_turn_boundary(message) {
            continue;
        }
        newest_turn.get_or_insert(index);
        if estimate_projected_messages_tokens(&history[index..]) > budget {
            break;
        }
        kept = history.len() - index;
    }
    if kept == 0 {
        // Not even the newest turn fits. Keep it anyway rather than sending an
        // empty history: an empty summarize call returns a summary of nothing
        // that would then replace the whole conversation.
        let start = newest_turn.unwrap_or(0);
        return history.split_off(start);
    }
    history.split_off(history.len() - kept)
}

/// Head/tail prunes older tool results in the summarize replay.
///
/// The newest [`SUMMARY_RECENT_TOOL_RESULTS`] results stay verbatim: they carry
/// the work in flight. Older over-budget results keep their head and tail so
/// the summarizer still sees commands, paths, and error lines at both ends.
fn prune_replayed_tool_results(mut history: Vec<ProjectedMessage>) -> Vec<ProjectedMessage> {
    let mut seen = 0_usize;
    for message in history.iter_mut().rev() {
        let ProjectedContent::ToolResult(result) = &mut message.content else {
            continue;
        };
        if seen < SUMMARY_RECENT_TOOL_RESULTS {
            seen += 1;
            continue;
        }
        if result.content.len() > SUMMARY_TOOL_RESULT_LIMIT {
            result.content =
                truncate_middle(&result.content, SUMMARY_TOOL_RESULT_LIMIT, &result.name);
        }
        seen += 1;
    }
    history
}

/// Keeps both ends of an over-long text within `limit` bytes.
fn truncate_middle(content: &str, limit: usize, tool: &str) -> String {
    if content.len() <= limit {
        return content.to_string();
    }
    let original = content.len();
    let mut shown = limit;
    loop {
        let notice = format!(
            "\n\n[old {tool} result pruned for the summary: showing {shown} of {original} bytes]\n\n"
        );
        let available = limit.saturating_sub(notice.len());
        let head = utf8_prefix(content, available / 2);
        let tail = utf8_suffix(content, available.saturating_sub(head.len()));
        let next_shown = head.len().saturating_add(tail.len());
        if next_shown == shown {
            return format!("{head}{notice}{tail}");
        }
        shown = next_shown;
    }
}

fn utf8_prefix(value: &str, limit: usize) -> &str {
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn utf8_suffix(value: &str, limit: usize) -> &str {
    let mut start = value.len().saturating_sub(limit);
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

fn is_turn_boundary(message: &ProjectedMessage) -> bool {
    message.role == Role::User && matches!(message.content, ProjectedContent::Parts(_))
}

/// Actual provider tokens per estimated token for the anchored prefix.
///
/// The provider reports what the previous request really cost; comparing that
/// against the estimate for the same persisted prefix gives a per-conversation
/// correction factor. It is applied to the messages added after the anchor and
/// to every budget expressed in estimated tokens. Without an anchor (first
/// request, model switch, just-compacted history) the uncalibrated estimate is
/// the only information available and the factor stays 1.
pub(super) fn estimate_calibration(
    prepared: &PreparedRun,
    projected_messages: &[ProjectedMessage],
    anchor: Option<ContextUsageAnchor>,
) -> f64 {
    let Some(anchor) = anchor.filter(|anchor| {
        anchor.message_count > 0 && anchor.message_count <= projected_messages.len()
    }) else {
        return 1.0;
    };
    if anchor.context_input_tokens == 0 {
        return 1.0;
    }
    let estimated_prefix = estimate_context_tokens(
        &prepared.prompt,
        &projected_messages[..anchor.message_count],
    )
    .max(1);
    (anchor.context_input_tokens as f64 / estimated_prefix as f64)
        .clamp(MIN_CALIBRATION, MAX_CALIBRATION)
}

pub(super) fn estimated_tokens(
    prepared: &PreparedRun,
    projected_messages: &[ProjectedMessage],
    anchor: Option<ContextUsageAnchor>,
) -> u64 {
    anchor
        .filter(|anchor| anchor.message_count <= projected_messages.len())
        .map(|anchor| {
            let unanchored =
                estimate_projected_messages_tokens(&projected_messages[anchor.message_count..]);
            let calibration = estimate_calibration(prepared, projected_messages, Some(anchor));
            let calibrated = (unanchored as f64 * calibration).ceil() as u64;
            anchor.context_input_tokens.saturating_add(calibrated)
        })
        .unwrap_or_else(|| estimate_context_tokens(&prepared.prompt, projected_messages))
}

/// One pressure decision plus the calibration it was priced with.
pub(super) struct CompactionEstimate {
    pub tokens: u64,
    pub calibration: f64,
}

pub(super) fn compaction_estimate(
    prepared: &PreparedRun,
    projected_messages: &[ProjectedMessage],
    anchor: Option<ContextUsageAnchor>,
    settings: &crate::store::CompactionSettings,
) -> Option<CompactionEstimate> {
    let budget = input_budget(prepared, settings)?;
    let calibration = estimate_calibration(prepared, projected_messages, anchor);
    let tokens = estimated_tokens(prepared, projected_messages, anchor);
    (tokens > budget).then_some(CompactionEstimate {
        tokens,
        calibration,
    })
}

#[cfg(test)]
pub(super) fn should_compact(
    prepared: &PreparedRun,
    projected_messages: &[ProjectedMessage],
    anchor: Option<ContextUsageAnchor>,
) -> bool {
    compaction_estimate(
        prepared,
        projected_messages,
        anchor,
        &crate::store::CompactionSettings::default(),
    )
    .is_some()
}

pub(super) fn validate_compacted(
    prepared: &PreparedRun,
    projected_messages: &[ProjectedMessage],
    calibration: f64,
    settings: &crate::store::CompactionSettings,
) -> std::result::Result<u64, String> {
    let estimated = estimate_context_tokens(&prepared.prompt, projected_messages);
    let Some(budget) = input_budget(prepared, settings) else {
        return Ok(estimated);
    };
    let budget = calibrate_budget(budget, calibration);
    if estimated <= budget {
        return Ok(estimated);
    }
    Err(format!(
        "context overflow after compaction: estimated input {estimated} tokens exceeds budget {budget} tokens"
    ))
}

pub(super) fn partition(
    messages: &[CanonicalMessage],
    current_ids: &HashSet<&str>,
) -> (Vec<CanonicalMessage>, Option<CanonicalMessage>) {
    let latest_request_context = messages
        .iter()
        .rposition(|message| message.message_id.starts_with("request-context:"));
    let compactable = messages
        .iter()
        .enumerate()
        .filter(|(index, message)| {
            Some(*index) != latest_request_context
                && !current_ids.contains(message.message_id.as_str())
        })
        .map(|(_, message)| message.clone())
        .collect();
    let retained = latest_request_context
        .and_then(|index| messages.get(index))
        .filter(|message| !current_ids.contains(message.message_id.as_str()))
        .cloned();
    (compactable, retained)
}

/// Budgets for the verbatim recent turns kept below the summary slot.
pub(super) struct RetentionBudget {
    /// Target share of the context window: the summary slot plus the retained
    /// turns stay within this, so a compacted conversation keeps room to grow
    /// before the next compaction.
    soft: u64,
    /// Largest tail that still passes `validate_compacted`.
    hard: u64,
}

/// Budgets for [`retained_tail`].
///
/// `head` is placed above the retained turns (the retained request context and
/// the summary slot); `after` is placed below them (the run's own initial
/// messages). Both are fixed costs of the compacted input.
///
/// `calibration` is the provider-anchored actual/estimated ratio: budgets are
/// converted into the estimator's unit space so the retained tail's real token
/// count stays inside the same share of the window, not just its estimate.
pub(super) fn retention_budget(
    prepared: &PreparedRun,
    head: &[CanonicalMessage],
    after: &[CanonicalMessage],
    calibration: f64,
    settings: &crate::store::CompactionSettings,
) -> crate::Result<Option<RetentionBudget>> {
    let Some(window) = prepared
        .model
        .context_window_tokens
        .filter(|window| *window > 0)
    else {
        // Without a window there is nothing to size a retention budget with:
        // the summary slot stands alone, as it did before.
        return Ok(None);
    };
    let Some(input_budget) = input_budget(prepared, settings) else {
        return Ok(None);
    };
    let head_tokens = projected_tokens(head)?;
    let window = calibrate_budget(window, calibration);
    let input_budget = calibrate_budget(input_budget, calibration);
    let retention = (window as u128 * u128::from(settings.retention_percent()) / 100) as u64;
    let mut soft = retention.saturating_sub(head_tokens);
    if let Some(cap) = settings.max_retained_tokens {
        soft = soft.min(calibrate_budget(cap, calibration));
    }
    let hard = input_budget
        .saturating_sub(estimate_context_tokens(&prepared.prompt, &[]))
        .saturating_sub(head_tokens)
        .saturating_sub(projected_tokens(after)?);
    let hard = match settings.max_retained_tokens {
        Some(cap) => hard.min(calibrate_budget(cap, calibration)),
        None => hard,
    };
    Ok(Some(RetentionBudget { soft, hard }))
}

fn projected_tokens(messages: &[CanonicalMessage]) -> crate::Result<u64> {
    Ok(estimate_projected_messages_tokens(&project_messages(
        messages,
    )?))
}

/// The newest whole turns kept verbatim after a compaction.
///
/// The tail is always a valid provider history: it starts at a user turn and
/// never splits a tool round, so every retained assistant tool call keeps its
/// results. Prompt context messages and superseded summaries are dropped from
/// it: the latest request context is retained above the summary slot, and an
/// older summary would only restate it.
pub(super) fn retained_tail(
    compactable: &[CanonicalMessage],
    budget: RetentionBudget,
) -> crate::Result<Vec<CanonicalMessage>> {
    if projected_tokens(compactable)? <= budget.soft {
        // Nothing is dropped for size, so compacting here is a deliberate reset
        // (a manual summarize on a short conversation): the summary slot stands
        // alone instead of being stacked on top of the full history.
        return Ok(Vec::new());
    }
    let starts = turn_starts(compactable);
    // 首轮 + 单轮内累计超预算时,compactable 里只有本轮的助手/工具消息:本轮那条 user 在
    // current_ids 里(partition 按 initial_messages 过滤),工具轮的助手/工具消息都算不上
    // 轮起点。这时直接返回空会让模型丢掉刚刚读到的全部内容,只能回头重读——和 retained_tail
    // 存在的理由相反。用第一条可保留的消息当锚点整段按预算保留:它正是本轮第一个工具轮的
    // 助手半条,从它开始不会把工具轮切成两半。
    let starts = if starts.is_empty() {
        compactable
            .iter()
            .position(|message| !dropped_from_tail(message))
            .into_iter()
            .collect()
    } else {
        starts
    };
    let Some(&newest) = starts.last() else {
        return Ok(Vec::new());
    };
    // `hard` is the largest tail that still passes `validate_compacted`. It is
    // normally far above `soft`, but a window small enough that the static
    // prompt prefix alone claims most of the budget can invert the two.
    let target = budget.soft.min(budget.hard);
    let mut keep_from = None;
    let mut total = 0_u64;
    for (position, &start) in starts.iter().enumerate().rev() {
        let end = starts
            .get(position + 1)
            .copied()
            .unwrap_or(compactable.len());
        let tokens = projected_tokens(&compactable[start..end])?;
        if total.saturating_add(tokens) > target {
            break;
        }
        total = total.saturating_add(tokens);
        keep_from = Some(start);
    }
    let start = match keep_from {
        Some(start) => start,
        // The newest turn alone exceeds the target. Keep it while the compacted
        // input still fits: it usually carries the tool output the model needs
        // to continue, and dropping it sends the model back to re-read.
        None if projected_tokens(&compactable[newest..])? <= budget.hard => newest,
        None => return Ok(Vec::new()),
    };
    Ok(compactable[start..]
        .iter()
        .filter(|message| !dropped_from_tail(message))
        .cloned()
        .collect())
}

/// Indexes of the user turns that may start a retained tail, oldest first.
///
/// Only a user message that started a turn can begin one: prompt context is
/// conversation-level, and a summary slot carries no turn of its own, so
/// neither may start a retained tail (see `dropped_from_tail`).
fn turn_starts(messages: &[CanonicalMessage]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| is_turn_start(message))
        .map(|(index, _)| index)
        .collect()
}

fn is_turn_start(message: &CanonicalMessage) -> bool {
    message.role == Role::User
        && message.origin != Origin::Prompt
        && !is_superseded_summary(message)
}

fn dropped_from_tail(message: &CanonicalMessage) -> bool {
    message.role == Role::User && message.origin == Origin::Prompt || is_superseded_summary(message)
}

fn is_superseded_summary(message: &CanonicalMessage) -> bool {
    message
        .runtime_event_id
        .as_deref()
        .is_some_and(|event_id| event_id.starts_with("summary:"))
}

/// The provider-visible message that carries a summary.
pub(super) fn summary_message(event_id: &str, summary: &str) -> CanonicalMessage {
    CanonicalMessage {
        message_id: format!("runtime:{event_id}"),
        role: Role::User,
        origin: Origin::Runtime,
        content: MessageContent::Parts {
            parts: vec![ContentPart::Text {
                text: format!("<conversation_summary>\n{summary}\n</conversation_summary>"),
            }],
        },
        runtime_event_id: Some(event_id.into()),
    }
}

/// Whether a compaction replays recent turns verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Retention {
    /// Keep the newest whole turns within the retention budget.
    Window,
    /// Keep nothing but the summary slot, its request context, and the run's
    /// own initial messages — the replacement 1.0.22 sent. Used when the
    /// provider refused a retained compaction, so an over-limit conversation
    /// falls back to the old shape instead of failing the run.
    None,
}

/// Builds the history a compaction leaves behind.
///
/// The order is fixed: the latest request context, the summary slot, the
/// retained recent turns, then this run's own initial messages. Both compaction
/// paths (Cursor `/summarize` and the BYOK preflight) assemble their
/// replacement here so they cannot drift apart.
pub(super) fn replacement(
    prepared: &PreparedRun,
    messages: &[CanonicalMessage],
    summary: CanonicalMessage,
    retention: Retention,
    calibration: f64,
    settings: &crate::store::CompactionSettings,
) -> crate::Result<Vec<CanonicalMessage>> {
    let current_ids = prepared
        .initial_messages
        .iter()
        .map(|message| message.message_id.as_str())
        .collect::<HashSet<_>>();
    let (compactable, retained_request_context) = partition(messages, &current_ids);
    let mut replacement = retained_request_context.into_iter().collect::<Vec<_>>();
    replacement.push(summary);
    if retention == Retention::Window {
        let tail = match retention_budget(
            prepared,
            &replacement,
            &prepared.initial_messages,
            calibration,
            settings,
        )? {
            Some(budget) => retained_tail(&compactable, budget)?,
            None => Vec::new(),
        };
        replacement.extend(offload_old_tail_images(tail));
    }
    replacement.extend(prepared.initial_messages.iter().cloned());
    Ok(replacement)
}

/// Keeps only the newest images in the retained tail visible to the provider.
fn offload_old_tail_images(tail: Vec<CanonicalMessage>) -> Vec<CanonicalMessage> {
    let mut remaining = MAX_RETAINED_IMAGES;
    let mut offloaded: Vec<CanonicalMessage> = tail
        .into_iter()
        .rev()
        .map(|mut message| {
            match &mut message.content {
                MessageContent::Parts { parts } => offload_parts(parts, &mut remaining),
                MessageContent::ToolResult(result) => {
                    if let Some(image) = result.image.take() {
                        if remaining > 0 {
                            remaining -= 1;
                            result.image = Some(image);
                        } else {
                            result.content =
                                format!("[image omitted during compaction: {}]", image.path);
                        }
                    }
                    offload_parts(&mut result.provider_parts, &mut remaining);
                }
                MessageContent::Assistant { .. } => {}
            }
            message
        })
        .collect();
    offloaded.reverse();
    offloaded
}

fn offload_parts(parts: &mut [ContentPart], remaining: &mut usize) {
    for part in parts.iter_mut().rev() {
        if let ContentPart::Image { .. } = part {
            if *remaining > 0 {
                *remaining -= 1;
            } else {
                *part = ContentPart::Text {
                    text: "[image omitted during compaction]".into(),
                };
            }
        }
    }
}

/// The text carried by a summary slot, with the checkpoint tags removed.
pub(super) fn summary_text(message: &CanonicalMessage) -> Option<String> {
    let MessageContent::Parts { parts } = &message.content else {
        return None;
    };
    let mut text = String::new();
    for part in parts {
        if let ContentPart::Text { text: part } = part {
            text.push_str(part);
        }
    }
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let text = text
        .strip_prefix("<conversation_summary>")
        .unwrap_or(text)
        .strip_suffix("</conversation_summary>")
        .unwrap_or(text)
        .trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The newest summary slot in a replacement history, when one exists.
pub(super) fn latest_summary_message(messages: &[CanonicalMessage]) -> Option<&CanonicalMessage> {
    messages
        .iter()
        .rev()
        .find(|message| is_superseded_summary(message))
}

/// A deterministic digest of the history a failed summarize call would have
/// covered, appended to the fallback slot.
///
/// The summary model is the only lossy step in compaction; when it fails, the
/// fallback used to be a single sentence that dropped every concrete detail.
/// This digest keeps the last user requests and assistant outputs (bounded, one
/// line each) so the model can still resume instead of re-reading everything.
pub(super) fn fallback_summary(compactable: &[CanonicalMessage]) -> String {
    match digest_of(compactable) {
        Some(digest) => {
            format!("{FALLBACK_SUMMARY}\n\nDeterministic digest of the dropped history:\n{digest}")
        }
        None => FALLBACK_SUMMARY.to_string(),
    }
}

/// The bounded, deterministic last-entries digest shared by the fallback slot
/// and the summary-reuse path of the overflow ladder.
pub(super) fn digest_of(compactable: &[CanonicalMessage]) -> Option<String> {
    const MAX_ENTRIES: usize = 3;
    const MAX_CHARS: usize = 400;
    let mut digest = Vec::new();
    for message in compactable.iter().rev() {
        if is_superseded_summary(message) {
            // A prior summary is not dropped history; quoting it back into its
            // own digest only duplicates tokens.
            continue;
        }
        let label = match message.role {
            Role::User if message.origin != Origin::Prompt => "User",
            Role::Assistant => "Assistant",
            _ => continue,
        };
        if digest.len() >= MAX_ENTRIES {
            break;
        }
        let text = message_text(message);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        let text = truncate_chars_middle(&text, MAX_CHARS);
        digest.push(format!("- {label}: {text}"));
    }
    if digest.is_empty() {
        return None;
    }
    digest.reverse();
    Some(digest.join("\n"))
}

/// Appends deterministic file anchors to a summary slot.
///
/// The compacted model loses the tool calls that read or edited files. The
/// summary template asks the summarizer to list key files, but a small model or
/// a failed summary drops them; the paths recovered from the shadowed tool
/// calls are exact, ordered, and cost only a line, so the model can re-read
/// them instead of rediscovering the repository.
pub(super) fn with_context_anchors(summary: &str, messages: &[CanonicalMessage]) -> String {
    if summary.contains("## Context Anchors") {
        return summary.to_string();
    }
    match context_anchors(messages) {
        Some(anchors) => format!("{summary}\n\n{anchors}"),
        None => summary.to_string(),
    }
}

/// Appends the searchable archive path to a summary slot.
///
/// The archive keeps every shadowed canonical message, so the model can recover
/// a detail the summary omitted with a `Shell` grep instead of re-reading files.
pub(super) fn with_archive_anchor(summary: &str, archive: Option<&std::path::Path>) -> String {
    let Some(archive) = archive else {
        return summary.to_string();
    };
    if summary.contains("## Archived History") {
        return summary.to_string();
    }
    format!(
        "{summary}\n\n## Archived History\n- Full pre-compaction messages: {} (use Shell grep/read to recover details the summary omitted)",
        archive.display()
    )
}

fn context_anchors(messages: &[CanonicalMessage]) -> Option<String> {
    let mut files: Vec<String> = Vec::new();
    for message in messages.iter().rev() {
        let MessageContent::Assistant { tool_calls, .. } = &message.content else {
            continue;
        };
        for call in tool_calls.iter().rev() {
            let Some(path) = anchor_path(&call.name, &call.arguments) else {
                continue;
            };
            if files.iter().any(|existing| existing == &path) {
                continue;
            }
            files.push(path);
            if files.len() >= MAX_ANCHOR_FILES {
                break;
            }
        }
        if files.len() >= MAX_ANCHOR_FILES {
            break;
        }
    }
    if files.is_empty() {
        return None;
    }
    files.reverse();
    Some(format!(
        "## Context Anchors\n- Recently referenced files (re-read them if the summary is not enough): {}",
        files.join(", ")
    ))
}

fn anchor_path(name: &str, arguments: &serde_json::Value) -> Option<String> {
    if !is_file_tool(name) {
        return None;
    }
    for key in [
        "path",
        "file_path",
        "target_file",
        "absolute_path",
        "filename",
        "file",
    ] {
        if let Some(value) = arguments.get(key).and_then(|value| value.as_str()) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn is_file_tool(name: &str) -> bool {
    let normalized: String = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "read"
            | "readfile"
            | "edit"
            | "write"
            | "strreplace"
            | "patchedit"
            | "patcheditlines"
            | "patcheditspan"
            | "editnotebook"
            | "applypatch"
            | "multiedit"
            | "createfile"
    )
}

fn message_text(message: &CanonicalMessage) -> String {
    match &message.content {
        MessageContent::Parts { parts } => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.clone()),
                ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        MessageContent::Assistant { text, .. } => text.clone(),
        MessageContent::ToolResult(result) => result.content.clone(),
    }
}

/// Keeps both ends of an over-long line: assistant output usually carries the
/// conclusion (markers, file names, errors) at the end, while the beginning
/// holds the task framing.
fn truncate_chars_middle(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit * 2 / 3).collect();
    let tail_count = limit.saturating_sub(head.chars().count() + 1);
    let tail: String = text
        .chars()
        .rev()
        .take(tail_count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        project_messages, CheckpointId, ContentPart, ConversationId, ModelSpec, Origin, PromptSpec,
        Role, RunAction, RunId, RunKind, ToolCallContent, ToolImageReference, ToolResultContent,
    };

    fn prepared(context_window_tokens: u64) -> PreparedRun {
        let mut model = ModelSpec::new("model");
        model.context_window_tokens = Some(context_window_tokens);
        PreparedRun {
            run_id: RunId::new("run"),
            cursor_request_id: None,
            conversation_id: ConversationId::new("conversation"),
            kind: RunKind::Root,
            model,
            prompt: PromptSpec {
                instructions: String::new(),
                tools: Vec::new(),
            },
            initial_messages: Vec::new(),
            action: RunAction::Start,
            base_checkpoint_id: CheckpointId(1),
        }
    }

    // Unit-test conveniences for the default policy/calibration path. Tests
    // that exercise a non-default policy call `super::` with explicit settings.
    fn default_settings() -> crate::store::CompactionSettings {
        crate::store::CompactionSettings::default()
    }

    fn compaction_history(
        history: Vec<ProjectedMessage>,
        context_window: Option<u64>,
    ) -> Vec<ProjectedMessage> {
        super::compaction_history(history, context_window, 1.0, &default_settings())
    }

    fn validate_compacted(
        prepared: &PreparedRun,
        projected_messages: &[ProjectedMessage],
    ) -> std::result::Result<u64, String> {
        super::validate_compacted(
            prepared,
            projected_messages,
            1.0,
            &crate::store::CompactionSettings::default(),
        )
    }

    fn retention_budget(
        prepared: &PreparedRun,
        head: &[CanonicalMessage],
        after: &[CanonicalMessage],
    ) -> crate::Result<Option<RetentionBudget>> {
        super::retention_budget(
            prepared,
            head,
            after,
            1.0,
            &crate::store::CompactionSettings::default(),
        )
    }

    fn replacement(
        prepared: &PreparedRun,
        messages: &[CanonicalMessage],
        summary: CanonicalMessage,
        retention: Retention,
    ) -> crate::Result<Vec<CanonicalMessage>> {
        super::replacement(
            prepared,
            messages,
            summary,
            retention,
            1.0,
            &crate::store::CompactionSettings::default(),
        )
    }

    #[test]
    fn automatic_compaction_uses_proportional_reserve_for_every_action() {
        let messages = vec![CanonicalMessage::text(
            "user",
            Role::User,
            Origin::Runtime,
            "x".repeat(40_000),
        )];
        let projected = project_messages(&messages).unwrap();
        let estimated = estimate_context_tokens(&prepared(1).prompt, &projected);
        // Smallest multiple-of-ten window whose 90% budget covers the estimate.
        let window = estimated.div_ceil(9) * 10;
        let mut prepared = prepared(window);
        assert!(context_budget(window) >= estimated);
        assert!(context_budget(window - 10) < estimated);

        assert!(!should_compact(&prepared, &projected, None));
        prepared.model.context_window_tokens = Some(window - 10);
        assert!(should_compact(&prepared, &projected, None));

        prepared.action = RunAction::Resume {
            pending_tool_round: None,
        };
        assert!(should_compact(&prepared, &projected, None));
    }

    #[test]
    fn the_reserve_leaves_room_for_the_estimate_to_run_behind() {
        // Reproduces a conversation that wedged itself against a 1M window.
        // The anchor said 947,797 tokens, so a fixed 10K reserve found room
        // and let the request through. Anthropic counted 1,017,628 and
        // refused it, and every retry repeated the same arithmetic.
        let messages = vec![CanonicalMessage::text(
            "user",
            Role::User,
            Origin::Runtime,
            "hello",
        )];
        let projected = project_messages(&messages).unwrap();
        let anchor = ContextUsageAnchor {
            context_input_tokens: 947_797,
            message_count: 1,
        };
        assert!(should_compact(
            &prepared(1_000_000),
            &projected,
            Some(anchor)
        ));
    }

    #[test]
    fn the_reserve_scales_with_the_window() {
        assert_eq!(context_budget(200_000), 180_000);
        assert_eq!(context_budget(1_000_000), 900_000);
        assert_eq!(context_budget(0), 0);
        assert_eq!(context_budget(1), 1);
    }

    #[test]
    fn provider_refusals_that_mean_the_prompt_did_not_fit_are_recognized() {
        assert!(is_context_overflow(
            "provider error: Anthropic 400 Bad Request: {\"type\":\"error\",\"error\":\
             {\"type\":\"invalid_request_error\",\"message\":\"prompt is too long: \
             1017628 tokens > 1000000 maximum\"}}"
        ));
        assert!(is_context_overflow("model_context_window_exceeded"));
        assert!(is_context_overflow("context_length_exceeded"));
        assert!(is_context_overflow(
            "This model's maximum context length is 128000 tokens"
        ));

        // Unrelated failures must not trigger a compaction, which would
        // destroy history to fix something compaction cannot fix.
        assert!(!is_context_overflow("401 Unauthorized: invalid api key"));
        assert!(!is_context_overflow("429 Too Many Requests"));
        assert!(!is_context_overflow(
            "This model does not support assistant message prefill"
        ));
    }

    fn user(id: &str, text: &str) -> ProjectedMessage {
        ProjectedMessage {
            message_id: id.into(),
            role: Role::User,
            content: ProjectedContent::Parts(vec![ContentPart::Text { text: text.into() }]),
        }
    }

    fn assistant(id: &str, text: &str) -> ProjectedMessage {
        ProjectedMessage {
            message_id: id.into(),
            role: Role::Assistant,
            content: ProjectedContent::Assistant {
                text: text.into(),
                thinking: String::new(),
                replay_state: None,
                calls: Vec::new(),
            },
        }
    }

    #[test]
    fn compaction_history_always_ends_with_a_user_message() {
        // Providers reject an assistant-terminated history as a prefill, which
        // made every automatic compaction fall back to the truncated summary.
        let history = vec![user("u1", "question"), assistant("a1", "answer")];
        let prepared = compaction_history(history, Some(200_000));
        assert_eq!(prepared.last().unwrap().role, Role::User);
        assert_eq!(
            prepared.last().unwrap().message_id,
            "compaction:instruction"
        );

        // An already user-terminated history is left alone.
        let history = vec![assistant("a1", "answer"), user("u2", "next")];
        let prepared = compaction_history(history.clone(), Some(200_000));
        assert_eq!(prepared, history);
    }

    #[test]
    fn compaction_history_is_trimmed_to_fit_the_context_window() {
        // Compaction runs because the conversation is too large, so the
        // summarize call must not replay a prompt that is over the window.
        let big = "x".repeat(400_000);
        let history = vec![
            user("u1", &big),
            assistant("a1", &big),
            user("u2", &big),
            assistant("a2", "recent answer"),
        ];
        let window = 200_000;
        let prepared = compaction_history(history, Some(window));

        let budget = context_budget(window) - OUTPUT_TOKENS;
        assert!(estimate_projected_messages_tokens(&prepared) <= budget);
        assert_eq!(prepared.last().unwrap().role, Role::User);
        // The newest turn survives the trim and is never split.
        assert!(prepared.iter().any(|message| message.message_id == "u2"));
        assert!(prepared.iter().any(|message| message.message_id == "a2"));
        assert!(!prepared.iter().any(|message| message.message_id == "u1"));
    }

    #[test]
    fn compaction_history_keeps_the_newest_turn_even_when_it_is_over_budget() {
        // A history whose newest turn alone exceeds the budget is still sent
        // rather than trimmed to nothing: a summary of nothing would replace
        // the whole conversation.
        let history = vec![
            user("u1", "old"),
            assistant("a1", "old answer"),
            user("u2", &"x".repeat(400_000)),
            assistant("a2", "answer"),
        ];
        let prepared = compaction_history(history, Some(50_000));
        assert_eq!(prepared[0].message_id, "u2");
        assert_eq!(prepared.last().unwrap().role, Role::User);
    }

    #[test]
    fn compaction_history_without_a_context_window_is_untouched_apart_from_termination() {
        let history = vec![user("u1", "question"), assistant("a1", "answer")];
        let prepared = compaction_history(history.clone(), None);
        assert_eq!(prepared[..2], history[..]);
        assert_eq!(prepared.last().unwrap().role, Role::User);
    }

    #[test]
    fn provider_usage_anchor_only_estimates_messages_added_after_last_request() {
        let messages = vec![
            CanonicalMessage::text("old", Role::User, Origin::Runtime, "x".repeat(400_000)),
            CanonicalMessage::text("new", Role::User, Origin::Runtime, "short follow-up"),
        ];
        let projected = project_messages(&messages).unwrap();
        let anchor = ContextUsageAnchor {
            context_input_tokens: 103_904,
            message_count: 1,
        };
        // The anchor proves the estimator under-counted the ASCII prefix by
        // 3.9%, so the delta after it is priced with the same correction.
        let calibration = estimate_calibration(&prepared(200_000), &projected, Some(anchor));
        assert!(
            (calibration - 1.039).abs() < 0.001,
            "calibration {calibration}"
        );
        let delta = estimate_projected_messages_tokens(&projected[1..]);
        let expected = 103_904 + (delta as f64 * calibration).ceil() as u64;

        assert_eq!(
            estimated_tokens(&prepared(200_000), &projected, Some(anchor)),
            expected
        );
        assert!(!should_compact(
            &prepared(200_000),
            &projected,
            Some(anchor)
        ));
    }

    #[test]
    fn cjk_anchor_calibration_prices_the_remaining_history_at_provider_density() {
        // A provider-charged prefix far above the estimate (CJK-heavy content)
        // must raise the estimate for messages added after the anchor too.
        let messages = vec![
            CanonicalMessage::text(
                "old",
                Role::User,
                Origin::Runtime,
                "中文内容".repeat(10_000),
            ),
            CanonicalMessage::text("new", Role::User, Origin::Runtime, "继续处理这个问题"),
        ];
        let projected = project_messages(&messages).unwrap();
        let estimated_prefix = estimate_context_tokens(&prepared(200_000).prompt, &projected[..1]);
        // Provider reports 1.8x the estimate for the Chinese prefix.
        let anchor = ContextUsageAnchor {
            context_input_tokens: estimated_prefix * 2 - estimated_prefix / 5,
            message_count: 1,
        };
        let calibration = estimate_calibration(&prepared(200_000), &projected, Some(anchor));
        assert!(calibration > 1.5, "calibration {calibration}");
        assert!(
            estimated_tokens(&prepared(200_000), &projected, Some(anchor))
                > estimate_projected_messages_tokens(&projected)
        );
    }

    #[test]
    fn anchor_calibration_is_clamped_to_the_supported_range() {
        let messages = vec![CanonicalMessage::text(
            "old",
            Role::User,
            Origin::Runtime,
            "tiny",
        )];
        let projected = project_messages(&messages).unwrap();
        let out_of_range = ContextUsageAnchor {
            context_input_tokens: 10_000_000,
            message_count: 1,
        };
        assert_eq!(
            estimate_calibration(&prepared(200_000), &projected, Some(out_of_range)),
            MAX_CALIBRATION
        );
        let tiny = ContextUsageAnchor {
            context_input_tokens: 1,
            message_count: 1,
        };
        assert_eq!(
            estimate_calibration(&prepared(200_000), &projected, Some(tiny)),
            MIN_CALIBRATION
        );
    }

    #[test]
    fn calibrated_summary_history_trims_more_than_the_uncalibrated_estimate() {
        let mut history = Vec::new();
        for index in 1..=8 {
            history.push(user(&format!("u{index}"), &"x".repeat(80_000)));
            history.push(assistant(&format!("a{index}"), "ok"));
        }
        let uncalibrated =
            super::compaction_history(history.clone(), Some(200_000), 1.0, &default_settings());
        let calibrated =
            super::compaction_history(history, Some(200_000), 2.0, &default_settings());
        assert!(
            estimate_projected_messages_tokens(&calibrated)
                < estimate_projected_messages_tokens(&uncalibrated),
            "a provider that charges twice the estimate must get a smaller summarize history"
        );
    }

    #[test]
    fn provider_usage_anchor_triggers_after_new_messages_cross_budget() {
        let messages = vec![
            CanonicalMessage::text("old", Role::User, Origin::Runtime, "old"),
            CanonicalMessage::text("new", Role::User, Origin::Runtime, "x".repeat(80_000)),
        ];
        let projected = project_messages(&messages).unwrap();

        assert!(should_compact(
            &prepared(200_000),
            &projected,
            Some(ContextUsageAnchor {
                context_input_tokens: 180_000,
                message_count: 1,
            })
        ));
    }

    #[test]
    fn missing_anchor_uses_full_fallback() {
        let messages = vec![CanonicalMessage::text(
            "user",
            Role::User,
            Origin::Runtime,
            "x".repeat(40_000),
        )];
        let projected = project_messages(&messages).unwrap();
        let prepared = prepared(200_000);

        assert_eq!(
            estimated_tokens(&prepared, &projected, None),
            estimate_context_tokens(&prepared.prompt, &projected)
        );
    }

    #[test]
    fn invalid_anchor_message_count_uses_full_fallback() {
        let messages = vec![CanonicalMessage::text(
            "user",
            Role::User,
            Origin::Runtime,
            "x".repeat(40_000),
        )];
        let projected = project_messages(&messages).unwrap();
        let expected = estimate_context_tokens(&prepared(200_000).prompt, &projected);

        assert_eq!(
            estimated_tokens(
                &prepared(200_000),
                &projected,
                Some(ContextUsageAnchor {
                    context_input_tokens: 1,
                    message_count: 2,
                })
            ),
            expected
        );
    }

    #[test]
    fn compacted_history_is_validated_against_the_same_budget() {
        let messages = vec![CanonicalMessage::text(
            "user",
            Role::User,
            Origin::Runtime,
            "x".repeat(40_000),
        )];
        let projected = project_messages(&messages).unwrap();
        let estimated = estimate_context_tokens(&prepared(1).prompt, &projected);
        let window = estimated.div_ceil(9) * 10;
        assert!(context_budget(window) >= estimated);
        assert!(context_budget(window - 10) < estimated);

        assert_eq!(
            validate_compacted(&prepared(window), &projected),
            Ok(estimated)
        );
        assert!(validate_compacted(&prepared(window - 10), &projected)
            .unwrap_err()
            .contains("context overflow after compaction"));
    }

    fn canonical_user(id: &str, text: &str) -> CanonicalMessage {
        CanonicalMessage::text(id, Role::User, Origin::Runtime, text)
    }

    fn canonical_context(id: &str, text: &str) -> CanonicalMessage {
        CanonicalMessage::text(id, Role::User, Origin::Prompt, text)
    }

    fn canonical_assistant(id: &str, text: &str) -> CanonicalMessage {
        CanonicalMessage {
            message_id: id.into(),
            role: Role::Assistant,
            origin: Origin::Assistant,
            content: MessageContent::Assistant {
                text: text.into(),
                thinking: String::new(),
                tool_round_id: None,
                replay_state: None,
                tool_calls: Vec::new(),
            },
            runtime_event_id: None,
        }
    }

    /// A user turn whose prompt is `characters` long, so tests can size turns.
    fn big_turn(user_id: &str, assistant_id: &str, characters: usize) -> Vec<CanonicalMessage> {
        vec![
            canonical_user(user_id, &"x".repeat(characters)),
            canonical_assistant(assistant_id, "ok"),
        ]
    }

    fn budget(prepared: &PreparedRun) -> RetentionBudget {
        retention_budget(prepared, &[], &prepared.initial_messages)
            .unwrap()
            .expect("a windowed model has a retention budget")
    }

    /// 首轮单 run 内触发压缩时,compactable 里没有 user 轮起点,尾部不能因此为空。
    /// 空尾部会把这一轮读到的文件内容整段丢掉,模型只能回头重读(甚至再次触发压缩)。
    #[test]
    fn a_turn_without_a_user_start_still_retains_its_tool_history() {
        let mut prepared = prepared(200_000);
        prepared.initial_messages = vec![canonical_user("runtime:current", "do it")];
        let history: Vec<CanonicalMessage> = (0..200)
            .map(|index| canonical_assistant(&format!("a{index}"), &"y".repeat(2_000)))
            .collect();
        assert!(
            projected_tokens(&history).unwrap() > budget(&prepared).soft,
            "这条用例要越过 soft 预算,否则走的是短会话刻意重置那条早返回"
        );

        let tail = retained_tail(&history, budget(&prepared)).unwrap();

        assert_eq!(tail.len(), history.len(), "首轮的工具历史必须被保留");
        assert_eq!(tail[0].message_id, "a0");
        assert_eq!(tail.last().unwrap().message_id, "a199");
    }

    #[test]
    fn retention_budget_scales_with_the_window_and_shrinks_with_the_summary_slot() {
        let windowed = prepared(200_000);
        assert_eq!(budget(&windowed).soft, 40_000);
        assert_eq!(budget(&windowed).hard, 180_000);
        assert_eq!(budget(&prepared(100_000)).soft, 20_000);

        // A long summary is charged to the same budget as the retained turns.
        let summary = summary_message("summary:auto:run:1", &"s".repeat(4_000));
        let head = projected_tokens(std::slice::from_ref(&summary)).unwrap();
        assert!(head > 1_000);
        assert_eq!(
            retention_budget(&windowed, &[summary], &[])
                .unwrap()
                .unwrap()
                .soft,
            40_000 - head
        );

        // Without a context window there is nothing to size a budget with.
        let mut windowless = windowed.clone();
        windowless.model.context_window_tokens = None;
        assert!(retention_budget(&windowless, &[], &[]).unwrap().is_none());
        assert!(retention_budget(&prepared(0), &[], &[]).unwrap().is_none());
    }

    #[test]
    fn a_history_within_the_budget_is_not_retained() {
        // Compacting a short conversation is a deliberate reset: the summary
        // slot stands alone instead of being stacked on the full history.
        let history = vec![
            canonical_user("u1", "short question"),
            canonical_assistant("a1", "short answer"),
        ];
        assert!(retained_tail(&history, budget(&prepared(200_000)))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn retained_tail_keeps_the_newest_whole_turns_within_the_budget() {
        let mut history = Vec::new();
        for index in 1..=5 {
            history.extend(big_turn(&format!("u{index}"), &format!("a{index}"), 40_000));
        }
        let tail = retained_tail(&history, budget(&prepared(200_000))).unwrap();
        // Three ~10k-token turns fit the 40k budget, the fourth would exceed it.
        assert_eq!(
            tail.iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["u3", "a3", "u4", "a4", "u5", "a5"]
        );
    }

    #[test]
    fn retained_tail_never_starts_inside_a_tool_round() {
        let mut history = big_turn("u1", "a1", 400_000);
        history.push(canonical_user("u2", "read the file"));
        history.push(CanonicalMessage {
            message_id: "a2".into(),
            role: Role::Assistant,
            origin: Origin::Assistant,
            content: MessageContent::Assistant {
                text: String::new(),
                thinking: String::new(),
                tool_round_id: Some(crate::model::ToolRoundId::new("round")),
                replay_state: None,
                tool_calls: vec![crate::model::ToolCallContent {
                    index: 0,
                    call_id: "call-1".into(),
                    name: "Read".into(),
                    arguments: serde_json::json!({"path": "src/main.rs"}),
                }],
            },
            runtime_event_id: None,
        });
        history.push(CanonicalMessage {
            message_id: "t1".into(),
            role: Role::Tool,
            origin: Origin::Tool,
            content: MessageContent::ToolResult(crate::model::ToolResultContent {
                call_id: "call-1".into(),
                name: "Read".into(),
                content: "file body".into(),
                is_error: false,
                image: None,
                provider_parts: Vec::new(),
            }),
            runtime_event_id: None,
        });
        history.push(canonical_assistant("a3", "done"));

        let tail = retained_tail(&history, budget(&prepared(200_000))).unwrap();
        assert_eq!(
            tail.iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["u2", "a2", "t1", "a3"],
            "the retained tail is the newest whole turn: the big older turn is dropped"
        );
        // The retained slice is a valid provider history on its own.
        let projected = project_messages(&tail).unwrap();
        assert_eq!(projected[0].role, Role::User);
        assert!(matches!(
            &projected[1].content,
            ProjectedContent::Assistant { calls, .. } if calls.len() == 1
        ));
        assert_eq!(projected[2].role, Role::Tool);
    }

    #[test]
    fn retained_tail_drops_prompt_context_and_superseded_summaries() {
        let mut history = big_turn("u1", "a1", 400_000);
        history.extend(big_turn("u2", "a2", 40_000));
        history.push(canonical_context("request-context:old", "older rules"));
        history.push(summary_message("summary:old", "older summary"));
        history.extend(big_turn("u3", "a3", 40_000));
        history.push(canonical_user("u4", "next"));
        history.push(canonical_assistant("a4", "ok"));

        let tail = retained_tail(&history, budget(&prepared(200_000))).unwrap();
        assert_eq!(tail.first().unwrap().message_id, "u2");
        assert!(
            tail.iter().all(|message| message.origin != Origin::Prompt),
            "the latest request context is retained above the summary, older ones are dropped"
        );
        assert!(
            tail.iter().all(|message| !message
                .runtime_event_id
                .as_deref()
                .is_some_and(|event_id| event_id.starts_with("summary:"))),
            "a superseded summary would only restate the new one"
        );
        assert_eq!(
            tail.iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["u2", "a2", "u3", "a3", "u4", "a4"]
        );
    }

    #[test]
    fn retained_tail_keeps_a_newest_turn_that_exceeds_the_target() {
        // The turn that filled the window usually carries the tool output the
        // model needs to continue, so it is kept while the input still fits.
        let history = vec![
            canonical_user("u1", "old"),
            canonical_assistant("a1", "old answer"),
            canonical_user("u2", &"x".repeat(400_000)),
            canonical_assistant("a2", "read it"),
        ];
        let tail = retained_tail(&history, budget(&prepared(200_000))).unwrap();
        assert_eq!(
            tail.iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["u2", "a2"]
        );
    }

    #[test]
    fn retained_tail_is_empty_when_the_newest_turn_exceeds_the_hard_limit() {
        let history = vec![
            canonical_user("u1", "old"),
            canonical_assistant("a1", "old answer"),
            canonical_user("u2", &"x".repeat(800_000)),
            canonical_assistant("a2", "read it"),
        ];
        assert!(retained_tail(&history, budget(&prepared(200_000)))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_static_prefix_that_overshoots_the_target_still_bounds_the_tail() {
        // A window where the system prompt alone claims most of the budget
        // inverts the two budgets. The tail must then follow the hard limit,
        // or `validate_compacted` rejects the input the compaction just built.
        let mut prepared = prepared(100_000);
        prepared.prompt.instructions = "s".repeat(320_000);
        let history = vec![
            canonical_user("u1", "old"),
            canonical_assistant("a1", "old answer"),
            canonical_user("u2", &"x".repeat(48_000)),
            canonical_assistant("a2", "read it"),
        ];
        let budget = retention_budget(&prepared, &[], &[]).unwrap().unwrap();
        assert!(
            budget.soft > budget.hard,
            "the setup must invert the budgets: soft {} hard {}",
            budget.soft,
            budget.hard
        );

        let replacement = replacement(
            &prepared,
            &history,
            summary_message("summary:auto:run:1", "hard state"),
            Retention::Window,
        )
        .unwrap();
        let projected = project_messages(&replacement).unwrap();
        assert!(
            validate_compacted(&prepared, &projected).is_ok(),
            "a tail that only fits the soft budget must not be retained"
        );
    }

    #[test]
    fn a_superseded_summary_never_starts_a_retained_tail() {
        // The assistant turn that follows a summary has no user message of its
        // own, so a tail anchored on the summary would begin with an assistant
        // message and drop the tool round it belongs to.
        let mut history = big_turn("u1", "a1", 400_000);
        history.push(summary_message("summary:old", "older summary"));
        history.push(CanonicalMessage {
            message_id: "a2".into(),
            role: Role::Assistant,
            origin: Origin::Assistant,
            content: MessageContent::Assistant {
                text: String::new(),
                thinking: String::new(),
                tool_round_id: Some(crate::model::ToolRoundId::new("round")),
                replay_state: None,
                tool_calls: vec![crate::model::ToolCallContent {
                    index: 0,
                    call_id: "call-1".into(),
                    name: "Read".into(),
                    arguments: serde_json::json!({"path": "src/main.rs"}),
                }],
            },
            runtime_event_id: None,
        });
        history.push(CanonicalMessage {
            message_id: "t1".into(),
            role: Role::Tool,
            origin: Origin::Tool,
            content: MessageContent::ToolResult(crate::model::ToolResultContent {
                call_id: "call-1".into(),
                name: "Read".into(),
                content: "file body".into(),
                is_error: false,
                image: None,
                provider_parts: Vec::new(),
            }),
            runtime_event_id: None,
        });

        let tail = retained_tail(&history, budget(&prepared(200_000))).unwrap();
        assert_eq!(
            tail.iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["u1", "a1", "a2", "t1"],
            "the retained tail keeps the whole turn instead of starting mid-turn"
        );
        assert_eq!(project_messages(&tail).unwrap()[0].role, Role::User);
    }

    #[test]
    fn replacement_never_manufactures_an_assistant_message() {
        // The slot and the request context are both user messages, and every
        // retained turn is reused verbatim, so a compacted history cannot
        // introduce an assistant turn that carries no visible content — the
        // shape Chat Completions providers reject (`content or tool_calls must be set` 400).
        let mut history = big_turn("u1", "a1", 400_000);
        history.extend(big_turn("u2", "a2", 40_000));
        let mut prepared = prepared(200_000);
        prepared.initial_messages = vec![canonical_user("runtime:current", "do it")];

        let replacement = replacement(
            &prepared,
            &history,
            summary_message("summary:auto:run:1", "hard state"),
            Retention::Window,
        )
        .unwrap();
        assert!(!replacement.is_empty());
        for message in &replacement {
            if message.role == Role::Assistant {
                assert!(
                    history.contains(message),
                    "compaction manufactured assistant message {}",
                    message.message_id
                );
            }
            if message.origin == Origin::Runtime {
                assert_eq!(
                    message.role,
                    Role::User,
                    "compaction only adds runtime user messages"
                );
            }
        }
    }

    #[test]
    fn a_retention_free_replacement_keeps_only_the_slot_and_the_run() {
        // The fallback used when the provider refused a retained compaction:
        // the result must be no larger than the summary-only replacement of
        // 1.0.22, whatever the history looked like.
        let mut history = big_turn("u1", "a1", 400_000);
        history.extend(big_turn("u2", "a2", 40_000));
        history.push(canonical_context("request-context:new", "current rules"));
        let mut prepared = prepared(200_000);
        prepared.initial_messages = vec![canonical_user("runtime:current", "do it")];

        let replacement = replacement(
            &prepared,
            &history,
            summary_message("summary:auto:run:1", "hard state"),
            Retention::None,
        )
        .unwrap();
        assert_eq!(
            replacement
                .iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            [
                "request-context:new",
                "runtime:summary:auto:run:1",
                "runtime:current"
            ]
        );
    }

    #[test]
    fn replacement_orders_context_summary_tail_and_initial_messages() {
        let mut history = big_turn("u1", "a1", 400_000);
        history.push(canonical_context("request-context:new", "current rules"));
        history.extend(big_turn("u2", "a2", 40_000));
        let mut prepared = prepared(200_000);
        prepared.initial_messages = vec![canonical_user("runtime:current", "do it")];

        let replacement = replacement(
            &prepared,
            &history,
            summary_message("summary:auto:run:1", "hard state"),
            Retention::Window,
        )
        .unwrap();
        assert_eq!(
            replacement
                .iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            [
                "request-context:new",
                "runtime:summary:auto:run:1",
                "u2",
                "a2",
                "runtime:current",
            ]
        );
    }

    #[test]
    fn replacement_without_a_context_window_keeps_only_the_summary() {
        let history = big_turn("u1", "a1", 400_000);
        let mut prepared = prepared(200_000);
        prepared.model.context_window_tokens = None;
        let replacement = replacement(
            &prepared,
            &history,
            summary_message("summary:auto:run:1", "hard state"),
            Retention::Window,
        )
        .unwrap();
        assert_eq!(
            replacement
                .iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["runtime:summary:auto:run:1"]
        );
    }

    #[test]
    fn output_reservation_shrinks_the_pressure_budget() {
        let mut prepared = prepared(200_000);
        prepared.model.max_output_tokens = Some(32_000);
        assert_eq!(context_budget(200_000), 180_000);
        assert_eq!(input_budget(&prepared, &default_settings()), Some(148_000));

        let history = vec![canonical_user("u1", &"x".repeat(600_000))];
        let projected = project_messages(&history).unwrap();
        assert!(
            should_compact(&prepared, &projected, None),
            "150k estimated input is over 200k - 20k reserve - 32k output"
        );
        prepared.model.max_output_tokens = None;
        assert!(
            !should_compact(&prepared, &projected, None),
            "without an output reservation the same input fits the 90% budget"
        );
    }

    #[test]
    fn summary_replay_prunes_only_the_older_tool_results() {
        let body = "x".repeat(100_000);
        let tool_result = |id: &str| ProjectedMessage {
            message_id: id.into(),
            role: Role::Tool,
            content: ProjectedContent::ToolResult(ToolResultContent {
                call_id: format!("call-{id}"),
                name: "Read".into(),
                content: body.clone(),
                is_error: false,
                image: None,
                provider_parts: Vec::new(),
            }),
        };
        let history = vec![
            tool_result("t1"),
            tool_result("t2"),
            tool_result("t3"),
            tool_result("t4"),
        ];
        let pruned = super::prune_replayed_tool_results(history);

        assert_eq!(pruned.len(), 4, "pruning never drops a message");
        let content_len = |message: &ProjectedMessage| match &message.content {
            ProjectedContent::ToolResult(result) => result.content.len(),
            _ => 0,
        };
        assert_eq!(
            content_len(&pruned[3]),
            body.len(),
            "the newest tool result stays verbatim"
        );
        assert_eq!(
            content_len(&pruned[2]),
            body.len(),
            "the second-newest tool result stays verbatim"
        );
        for message in &pruned[..2] {
            let ProjectedContent::ToolResult(result) = &message.content else {
                panic!("expected a tool result");
            };
            assert!(
                result.content.contains("pruned for the summary"),
                "older results are pruned with a notice"
            );
            assert!(result.content.len() <= SUMMARY_TOOL_RESULT_LIMIT);
        }
    }

    #[test]
    fn context_anchors_come_from_the_shadowed_file_tool_calls() {
        let assistant = |calls: Vec<ToolCallContent>| CanonicalMessage {
            message_id: "a1".into(),
            role: Role::Assistant,
            origin: Origin::Assistant,
            content: MessageContent::Assistant {
                text: String::new(),
                thinking: String::new(),
                tool_round_id: None,
                replay_state: None,
                tool_calls: calls,
            },
            runtime_event_id: None,
        };
        let call = |id: &str, path: &str| ToolCallContent {
            index: 0,
            call_id: id.into(),
            name: "Read".into(),
            arguments: serde_json::json!({"path": path}),
        };
        let messages = vec![assistant(vec![
            call("c1", "src/main.rs"),
            call("c2", "src/main.rs"),
            call("c3", "src/lib.rs"),
        ])];

        let summary = with_context_anchors("handover", &messages);
        assert!(summary.starts_with("handover\n\n## Context Anchors"));
        assert!(summary.contains("src/main.rs, src/lib.rs"));
        assert_eq!(summary.matches("src/main.rs").count(), 1);
        assert_eq!(
            with_context_anchors(&summary, &messages),
            summary,
            "anchors are not appended twice"
        );
    }

    #[test]
    fn fallback_digest_skips_prior_summary_slots() {
        let compactable = vec![
            summary_message("summary:old", "OLD-SUMMARY-MARKER"),
            canonical_user("u1", "continue the task"),
            canonical_assistant("a1", "working on it"),
        ];
        let digest = super::digest_of(&compactable).expect("a digest for real messages");
        assert!(!digest.contains("OLD-SUMMARY-MARKER"));
        assert!(digest.contains("continue the task"));
    }

    #[test]
    fn compaction_policy_changes_reserve_retention_and_absolute_cap() {
        let prepared = prepared(1_000_000);
        assert_eq!(input_budget(&prepared, &default_settings()), Some(900_000));

        let strict = crate::store::CompactionSettings {
            reserve_percent: Some(25),
            ..Default::default()
        };
        assert_eq!(
            input_budget(&prepared, &strict),
            Some(750_000),
            "a 25% reserve leaves three quarters of the window for the prompt"
        );

        let capped = crate::store::CompactionSettings {
            retention_percent: Some(5),
            max_retained_tokens: Some(20_000),
            ..Default::default()
        };
        let budget = super::retention_budget(&prepared, &[], &[], 1.0, &capped)
            .unwrap()
            .unwrap();
        assert_eq!(
            budget.soft, 20_000,
            "5% of 1M is 50k but the absolute cap wins"
        );
        assert_eq!(budget.hard, 20_000, "the cap bounds the hard limit too");
    }

    #[test]
    fn provider_context_limits_are_learned_from_error_prose() {
        assert_eq!(
            provider_context_limit("prompt is too long: 210000 tokens > 200000 maximum"),
            Some(200_000)
        );
        assert_eq!(
            provider_context_limit("maximum context length is 128000 tokens"),
            Some(128_000)
        );
        assert_eq!(
            provider_context_limit("context window is 1,000,000 tokens"),
            Some(1_000_000)
        );
        assert_eq!(
            provider_context_limit("401 Unauthorized: invalid api key"),
            None
        );
        assert_eq!(provider_context_limit("prompt is too long"), None);
    }

    #[test]
    fn archive_anchor_points_at_the_searchable_history() {
        let summary = with_archive_anchor(
            "handover",
            Some(std::path::Path::new("/tmp/compaction/run-1.jsonl")),
        );
        assert!(summary.starts_with("handover"));
        assert!(summary.contains("## Archived History"));
        assert!(summary.contains("/tmp/compaction/run-1.jsonl"));
        assert_eq!(
            with_archive_anchor(&summary, Some(std::path::Path::new("/tmp/other"))),
            summary,
            "the archive anchor is not appended twice"
        );
        assert_eq!(with_archive_anchor("handover", None), "handover");
    }

    #[test]
    fn tail_image_offload_keeps_only_the_newest_images() {
        let image_message = |id: &str| CanonicalMessage {
            message_id: id.into(),
            role: Role::Tool,
            origin: Origin::Tool,
            content: MessageContent::ToolResult(ToolResultContent {
                call_id: id.into(),
                name: "Read".into(),
                content: "Read image file".into(),
                is_error: false,
                image: Some(ToolImageReference {
                    blob_id: "blob".into(),
                    mime_type: "image/png".into(),
                    path: format!("/tmp/{id}.png"),
                }),
                provider_parts: Vec::new(),
            }),
            runtime_event_id: None,
        };
        let tail = vec![image_message("a"), image_message("b"), image_message("c")];
        let offloaded = offload_old_tail_images(tail);
        fn result(message: &CanonicalMessage) -> &crate::model::ToolResultContent {
            match &message.content {
                MessageContent::ToolResult(result) => result,
                _ => panic!("expected tool result"),
            }
        }
        assert!(result(&offloaded[0]).image.is_none());
        assert!(result(&offloaded[0]).content.contains("omitted"));
        assert!(result(&offloaded[1]).image.is_some());
        assert!(result(&offloaded[2]).image.is_some());
    }
}
