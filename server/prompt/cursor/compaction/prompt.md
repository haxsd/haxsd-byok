You are compacting conversation history for future model turns.
Summarize this conversation as a durable handover state for the next model turn, not a retelling of the conversation.

Return plain text with exactly these sections, in order, and keep every heading even when empty (write "(none)"):
## Goal
- [the user's original and evolving goal; quote exact wording where it matters]
## Done and Verified
- [what is already implemented, tested, or confirmed]
## In Flight
- [work currently in progress]
## Next Action
- [the single next concrete step]
## Constraints and Decisions
- [decisions that must not be reversed, user corrections, constraints]
## Key Files, Commands, Values
- [exact paths, commands, identifiers, numbers]
## Open Errors / Unfinished Work
- [errors, failing checks, explicitly requested work not done]
## Earlier Summary
- [merge still-true facts from a prior conversation_summary; do not copy it verbatim]

Rules:
- Prefer durable state over narration of recent turns.
- Do not address the user. Do not mention compaction, summarization, or token limits. Do not call tools.
- Preserve exact paths, commands, identifiers, numbers, and error strings.
- Write in the language the user used in the conversation.
- Return plain text only, in short bullet-like lines.
