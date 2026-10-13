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

/// Fraction of the context window kept free, as a divisor: 10 = 10%.
///
/// The estimate runs behind the provider: the anchor is what the provider
/// charged for the *previous* call, and the next request re-sends request
/// context and carries provider-side overhead the message-tail estimate does
/// not model. A real conversation measured 948K estimated against 1,017,628
/// actual, a 7% shortfall that landed it over a 1M window while the check said
/// there was room. A proportional reserve absorbs that drift and scales with
/// the model: a 200K window keeps 20K free and a 1M window keeps 100K.
const CONTEXT_RESERVE_DIVISOR: u64 = 10;

/// Post-compaction message budget as a divisor of the context window: 5 = 20%.
///
/// The summary slot and the verbatim turns kept below it share this budget, so
/// a long summary automatically leaves less room for recent turns. The static
/// prompt prefix (mode instructions plus tool definitions) is not charged to
/// it: that prefix is sent on every turn regardless, and charging it here would
/// leave small windows with almost no room for the turns the model needs.
const RETAIN_WINDOW_DIVISOR: u64 = 5;

/// Upper bound on one summary, in output tokens of the summarizer call.
///
/// The summary stays provider-visible until the next compaction, so it competes
/// with the retained turns for the budget above. This bound only stops a
/// rambling model from being cut mid-sentence; `INSTRUCTIONS` asks for a short
/// handover state. Call sites clamp it to the configured model limit, because
/// providers reject a `max_tokens` above the model's own output limit.
pub(super) const OUTPUT_TOKENS: u64 = 8_192;
pub(super) const INSTRUCTIONS: &str = "Summarize this conversation as a short handover state for the next model turn. Keep only durable state: the user's goal, what is already done or verified, the work in flight with the next concrete action, constraints and decisions that must not be reversed, key files, commands and values, and open errors or unfinished work. Prefer durable state over narration of recent turns; do not address the user, do not mention compaction or token limits, and do not call tools. Return plain text only, in short bullet-like lines.";

/// Summary slot content when the summarizer produced nothing usable.
///
/// It states what happened instead of pretending to summarize content that was
/// dropped, and keeps the Cursor summary state well formed for the client.
pub(super) const FALLBACK_SUMMARY: &str = "This conversation was compacted because the context window was full, and the summary call failed. Older history was dropped; the turns below are kept verbatim. Continue the current task from this state.";

/// Usable prompt budget: the window minus the proportional reserve.
pub(super) fn context_budget(context_window: u64) -> u64 {
    context_window.saturating_sub(context_window / CONTEXT_RESERVE_DIVISOR)
}

pub(super) fn input_budget(prepared: &PreparedRun) -> Option<u64> {
    prepared.model.context_window_tokens.map(context_budget)
}

/// Whether a provider failure means the prompt did not fit.
///
/// Providers report this as a plain 400 with prose, so there is nothing
/// structured to match on. Anthropic says "prompt is too long"; OpenAI-style
/// gateways use `context_length_exceeded` or "maximum context length".
pub(super) fn is_context_overflow(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("prompt is too long")
        || lowered.contains("context window exceeded")
        || lowered.contains("model_context_window_exceeded")
        || lowered.contains("context_length_exceeded")
        || (lowered.contains("maximum context length") && lowered.contains("token"))
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
/// Either failure falls back to the truncated summary, which is usually still
/// too large, so the conversation stays over its window and cannot recover.
///
/// Trimming keeps the most recent turns and only ever cuts at a user-message
/// boundary, so an assistant tool call is never separated from its results.
pub(super) fn compaction_history(
    history: Vec<ProjectedMessage>,
    context_window: Option<u64>,
) -> Vec<ProjectedMessage> {
    super::history::user_terminated(
        trim_to_context(history, context_window),
        "compaction:instruction",
        INSTRUCTIONS,
    )
}

fn trim_to_context(
    mut history: Vec<ProjectedMessage>,
    context_window: Option<u64>,
) -> Vec<ProjectedMessage> {
    let Some(budget) = context_window
        .filter(|window| *window > 0)
        .map(context_budget)
        .map(|budget| budget.saturating_sub(OUTPUT_TOKENS))
        .filter(|budget| *budget > 0)
    else {
        return history;
    };
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

fn is_turn_boundary(message: &ProjectedMessage) -> bool {
    message.role == Role::User && matches!(message.content, ProjectedContent::Parts(_))
}

pub(super) fn estimated_tokens(
    prepared: &PreparedRun,
    projected_messages: &[ProjectedMessage],
    anchor: Option<ContextUsageAnchor>,
) -> u64 {
    anchor
        .filter(|anchor| anchor.message_count <= projected_messages.len())
        .map(|anchor| {
            anchor
                .context_input_tokens
                .saturating_add(estimate_projected_messages_tokens(
                    &projected_messages[anchor.message_count..],
                ))
        })
        .unwrap_or_else(|| estimate_context_tokens(&prepared.prompt, projected_messages))
}

pub(super) fn compaction_estimate(
    prepared: &PreparedRun,
    projected_messages: &[ProjectedMessage],
    anchor: Option<ContextUsageAnchor>,
) -> Option<u64> {
    let budget = input_budget(prepared)?;
    let estimated = estimated_tokens(prepared, projected_messages, anchor);
    (estimated > budget).then_some(estimated)
}

#[cfg(test)]
pub(super) fn should_compact(
    prepared: &PreparedRun,
    projected_messages: &[ProjectedMessage],
    anchor: Option<ContextUsageAnchor>,
) -> bool {
    compaction_estimate(prepared, projected_messages, anchor).is_some()
}

pub(super) fn validate_compacted(
    prepared: &PreparedRun,
    projected_messages: &[ProjectedMessage],
) -> std::result::Result<u64, String> {
    let estimated = estimate_context_tokens(&prepared.prompt, projected_messages);
    let Some(budget) = input_budget(prepared) else {
        return Ok(estimated);
    };
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
pub(super) fn retention_budget(
    prepared: &PreparedRun,
    head: &[CanonicalMessage],
    after: &[CanonicalMessage],
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
    let Some(input_budget) = input_budget(prepared) else {
        return Ok(None);
    };
    let head_tokens = projected_tokens(head)?;
    let soft = (window / RETAIN_WINDOW_DIVISOR).saturating_sub(head_tokens);
    let hard = input_budget
        .saturating_sub(estimate_context_tokens(&prepared.prompt, &[]))
        .saturating_sub(head_tokens)
        .saturating_sub(projected_tokens(after)?);
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
        let tail = match retention_budget(prepared, &replacement, &prepared.initial_messages)? {
            Some(budget) => retained_tail(&compactable, budget)?,
            None => Vec::new(),
        };
        replacement.extend(tail);
    }
    replacement.extend(prepared.initial_messages.iter().cloned());
    Ok(replacement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        project_messages, CheckpointId, ContentPart, ConversationId, ModelSpec, Origin, PromptSpec,
        Role, RunAction, RunId, RunKind,
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
        let expected = 103_904 + estimate_projected_messages_tokens(&projected[1..]);

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
}
