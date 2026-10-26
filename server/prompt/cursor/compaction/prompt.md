Summarize this conversation as a complete engineering handover for the next model turn.

You are compacting conversation history for future model turns. Return plain text with exactly these sections, in order, and keep every heading even when empty (write "(none)"):
## Goal
- [the user's original and evolving goal, including explicit non-goals; quote exact wording whenever a requirement or correction matters]
## Done and Verified
- [what is already implemented, tested, or confirmed; include the exact checks that passed]
## In Flight
- [work currently in progress, including the exact file or command being handled]
## Next Action
- [the single next concrete step]
## Constraints and Decisions
- [decisions that must not be reversed, their rationale, rejected alternatives, user corrections, and all constraints]
## Key Files, Commands, Values
- [exact paths, commands, identifiers, numbers, API names, flags, versions, and configuration values]
## Open Errors / Unfinished Work
- [complete error strings, failing checks, unverified assumptions, and explicitly requested work that is not done]

Rules:
- Prefer completeness over brevity. Fill every section with durable state, not narration of recent turns.
- For a long coding session, expect roughly 2,000–6,000 tokens of dense handover. For a short session, stay proportionate.
- Preserve every still-true user requirement, correction, constraint, accepted decision, and rejected alternative. Do not collapse distinct facts into a vague combined bullet.
- State what was verified separately from what was only attempted or assumed.
- Keep exact paths, commands, identifiers, numbers, API names, flags, versions, and error strings verbatim.
- Do not omit an older fact merely because it is old; omit it only when it is no longer true or no longer relevant.
- If a prior `<conversation_summary>` appears in the history, it is already retained verbatim outside this output. Use it only as context and do not repeat or rewrite it.
- Do not address the user, do not mention compaction, summarization, or token limits, and do not call tools.
- Write in the language the user used in the conversation.
- Return plain text only, using dense bullet-like lines.
