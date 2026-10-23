import type { ModelDefinition, ModelSupport } from "cursor-byok:model";

/**
 * 模型目录（2026-10-23 核实）：
 * - 当前官方阵容来自 https://platform.claude.com/docs/en/about-claude/models/overview
 *   （Fable 5.1 / Opus 5.5 / Sonnet 5.5 / Haiku 5.5）。
 * - ID、上下文窗口与输出上限与 LiteLLM 的
 *   https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json
 *   交叉核对；官方模型页要求 1M 上下文，LiteLLM 对 5.x 标 1,000,000 / 128,000。
 * - 4.5 系列仍是 200,000 上下文 / 64,000 输出；带日期后缀的是固定快照。
 *
 * SDK 的 ModelDefinition 没有 contextWindow 字段，因此这里只声明 host 能消费的
 * maxOutputTokens；上下文窗口写在文档与本节注释中，避免塞进 privateData 形成假字段。
 */
export const CLAUDE_CODE_MODELS: ModelDefinition[] = [
  {
    id: "claude-fable-5-1",
    displayName: "Claude Fable 5.1",
    description: "Anthropic's most demanding reasoning and long-horizon agent model.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-opus-5-5",
    displayName: "Claude Opus 5.5",
    description: "Long-running agentic coding and knowledge work.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-sonnet-5-5",
    displayName: "Claude Sonnet 5.5",
    description: "Balanced speed and intelligence for everyday coding.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-haiku-5-5",
    displayName: "Claude Haiku 5.5",
    description: "High-volume, latency-sensitive tasks.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-opus-5",
    displayName: "Claude Opus 5",
    description: "Previous Opus generation; 1M context.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-sonnet-5",
    displayName: "Claude Sonnet 5",
    description: "Previous Sonnet generation; 1M context.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-fable-5",
    displayName: "Claude Fable 5",
    description: "First Fable generation; 1M context.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-opus-4-8",
    displayName: "Claude Opus 4.8",
    description: "Opus 4.8; 1M context.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-opus-4-7",
    displayName: "Claude Opus 4.7",
    description: "Opus 4.7; 1M context.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-opus-4-6",
    displayName: "Claude Opus 4.6",
    description: "Opus 4.6; 1M context.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-sonnet-4-6",
    displayName: "Claude Sonnet 4.6",
    description: "Sonnet 4.6; 1M context.",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-opus-4-5",
    displayName: "Claude Opus 4.5",
    description: "Opus 4.5; 200K context.",
    maxOutputTokens: 64_000,
    capabilities: { images: true },
  },
  {
    id: "claude-sonnet-4-5",
    displayName: "Claude Sonnet 4.5",
    description: "Sonnet 4.5; 200K context.",
    maxOutputTokens: 64_000,
    capabilities: { images: true },
  },
  {
    id: "claude-haiku-4-5",
    displayName: "Claude Haiku 4.5",
    description: "Haiku 4.5; 200K context.",
    maxOutputTokens: 64_000,
    capabilities: { images: true },
  },
  // 日期后缀是固定快照；需要可复现行为时选它们，日常使用上面的稳定别名。
  {
    id: "claude-opus-4-7-20260416",
    displayName: "Claude Opus 4.7 (2026-04-16)",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-opus-4-6-20260205",
    displayName: "Claude Opus 4.6 (2026-02-05)",
    maxOutputTokens: 128_000,
    capabilities: { images: true },
  },
  {
    id: "claude-opus-4-5-20251101",
    displayName: "Claude Opus 4.5 (2025-11-01)",
    maxOutputTokens: 64_000,
    capabilities: { images: true },
  },
  {
    id: "claude-sonnet-4-5-20250929",
    displayName: "Claude Sonnet 4.5 (2025-09-29)",
    maxOutputTokens: 64_000,
    capabilities: { images: true },
  },
  {
    id: "claude-haiku-4-5-20251001",
    displayName: "Claude Haiku 4.5 (2025-10-01)",
    maxOutputTokens: 64_000,
    capabilities: { images: true },
  },
];

export const claudeCodeModels: ModelSupport = {
  list: () => Promise.resolve(CLAUDE_CODE_MODELS),
};
