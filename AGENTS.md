# AGENTS.md
- **The directory structure is the architecture.** Simple, clear directory and module naming >= module dependency relationships > concrete implementation details; communicate with the user using directory trees.
- Do not preserve backward compatibility. Remove obsolete paths instead of adding compatibility layers, fallbacks, or migrations.
- Choose the simplest implementation that fully meets the current requirements. Avoid speculative abstractions, configuration, and indirection.
- Grow the system in layers. Start from the smallest version that works end to end, and add each new capability on top of a product that already works. Never trade a working product for unfinished complexity.
- Keep components modular and concerns clearly separated.
- Prefer established, well-maintained libraries when they reduce overall complexity or improve reliability. Do not reimplement common functionality without a clear reason.
- Lean on the dependencies already in the project before writing your own implementation or adding packages. Do not assume a library lacks a capability without checking its documentation and types.
- Make architectural decisions for the long term. Do not accept a stopgap that only works for now and is meant to be replaced later.

## Communication

- Lead with the conclusion. Keep responses direct and omit filler, repeated context, generic explanations, and narration of obvious steps.
- Match the level requested by the user: discuss architecture as architecture, behavior as behavior, and code details only when they materially support the answer.
- For architecture, refactoring, and module-boundary discussions, communicate primarily with annotated directory trees and ASCII architecture/data-flow diagrams instead of long prose.
- In directory trees, annotate every relevant directory and file with its single responsibility. When discussing code size, include line counts in the comments:
  - use measured line counts for existing code;
  - use clearly marked approximate targets such as `≈300 lines` for proposed code;
  - state whether tests, generated code, and blank lines are excluded.
- Show the complete main execution path, including inputs, ownership boundaries, runtime loops, persistence, outputs, and extension points. Make the direction of data and control flow explicit.
- Clearly separate the current structure from the proposed structure. Explicitly list modules that move, merge, split, or are deleted.
- Use the user's vocabulary consistently. Do not introduce new domain terms when existing plain-language terms are sufficient; when an implementation name must be mentioned, distinguish it from the architecture concept.
- For stateful or concurrent behavior, show timing, ownership, state boundaries, and before/during/after behavior explicitly. Distinguish continuation within the same lifecycle from creation of a new lifecycle.
- Prefer compact trees, flow diagrams, state matrices, and mappings when they communicate the structure more clearly than paragraphs.
- Tie recommendations to the actual repository structure and code. Do not present a speculative target architecture as if it already exists.

## 仓库边界：内部资料不进公开仓库

本仓库对陌生人可见。**内部资料**——交接说明、内部方案与审查/审计记录、含机器路径或真实用量数据的笔记、
逆向工具——一律写进私有记录仓库 `haxsd/haxsd-dev-records`（本仓库对应分区 `haxsd-byok/` 或 `cursor-byok/`），
公开仓库只放产品与贡献者需要的内容。

公开仓库不得出现这些路径：`HANDOFF.md`、`BRANCH_ISOLATION.md`、`docs/plans/**`、`docs/superpowers/**`、
`docs/quality-review-*`、`docs/devin-router-vendor-evidence.md`、`support/cursor-capture/**`、
`support/cursor-protocol-extractor/**`。CI 的 `records-guard` 任务会拦下它们。

提交信息里也别写内部细节（账号、机器路径、真实 token 数），这些写进记录仓库的提交信息。
收尾检查：在记录仓库里跑 `sync-records.ps1`（扫描五个公开仓库的当前树与历史，发现内部资料即报错）。
