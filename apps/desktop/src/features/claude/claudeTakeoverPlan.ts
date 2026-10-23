import type { ClaudeSettings, ClaudeStatus, Model } from "../../shared/api";
import type { ModelSelectOption } from "../../shared/ui/ModelSelect";
import { modelProviderName } from "../../shared/utils/modelProvider";

/**
 * 接管时会写入什么、选择器里能选什么。
 *
 * 状态与按钮可用性在 `claudeConnectionView.ts`；这里只回答"应用接管会写哪些键"、
 * "Desktop 的模型列表从哪来、怎么排序"，以及引导视图要展示的值。
 */

/**
 * CLI 的三个模型家族：模型哈希决定请求，显示名只影响客户端的模型选择器。
 * 键名与服务端 `MANAGED_ENV_KEYS` 一一对应。
 */
const cliFamilies = [
  { hash: "sonnet_model_hash", model: "ANTHROPIC_DEFAULT_SONNET_MODEL", name: "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME" },
  { hash: "opus_model_hash", model: "ANTHROPIC_DEFAULT_OPUS_MODEL", name: "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME" },
  { hash: "haiku_model_hash", model: "ANTHROPIC_DEFAULT_HAIKU_MODEL", name: "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME" },
] as const;

/**
 * Claude Code 接管时写入 `settings.json` 的 env。
 *
 * 键名与 Claude Code 自己读取的环境变量一致；空值不写，避免把别名清成空串。显示名
 * 取自模型库，与服务端写入的值一致；模型不在库里时只写哈希（应用前 gate 会拦住）。
 * 默认模型不在 env 里：客户端自带的版本化 ID 由网关按家族名回落到它。
 */
export function claudeCliEnv(
  status: ClaudeStatus | null,
  settings: ClaudeSettings | null,
  models: Model[] = [],
): Record<string, string> {
  if (!status) return {};
  const env: Record<string, string> = {};
  if (status.base_url) env.ANTHROPIC_BASE_URL = status.base_url;
  if (status.token) env.ANTHROPIC_AUTH_TOKEN = status.token;
  env.CLAUDE_CODE_ATTRIBUTION_HEADER = "0";
  for (const family of cliFamilies) {
    const hash = settings?.[family.hash];
    if (!hash) continue;
    env[family.model] = hash;
    const name = models.find((model) => model.model_hash === hash)?.display_name?.trim();
    if (name) env[family.name] = name;
  }
  return env;
}

/** 应用前展示的只读预览：服务端会把这些键写进 `settings.json` 的 `env`。 */
export function claudeCliPreview(
  status: ClaudeStatus | null,
  settings: ClaudeSettings | null,
  models: Model[] = [],
): string {
  return JSON.stringify({ env: claudeCliEnv(status, settings, models) }, null, 2);
}

/** 复制给终端用的环境变量文本（`KEY=value` 每行一个）。 */
export function claudeEnvText(
  status: ClaudeStatus | null,
  settings: ClaudeSettings | null,
  models: Model[] = [],
): string {
  return Object.entries(claudeCliEnv(status, settings, models))
    .map(([key, value]) => `${key}=${value}`)
    .join("\n");
}

/** 接管写入的 env 键：撤销按这份清单逐键还原，顺序与服务端一致。 */
export const claudeCliEnvKeys = [
  "ANTHROPIC_BASE_URL",
  "ANTHROPIC_AUTH_TOKEN",
  "CLAUDE_CODE_ATTRIBUTION_HEADER",
  "ANTHROPIC_DEFAULT_SONNET_MODEL",
  "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
  "ANTHROPIC_DEFAULT_OPUS_MODEL",
  "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
  "ANTHROPIC_DEFAULT_HAIKU_MODEL",
  "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
] as const;

/**
 * Desktop 模型列表的初值：默认模型在前，随后是三个 CLI 别名，按哈希去重。
 *
 * Desktop 的列表没有单独持久化字段，设置里的四个哈希是它唯一可用的起点；用户调整
 * 顺序后由 apply 的 `models` 决定客户端里的顺序。
 */
export function claudeDesktopModelDraft(settings: ClaudeSettings | null): string[] {
  if (!settings) return [];
  const hashes = [
    settings.default_model_hash,
    settings.sonnet_model_hash,
    settings.opus_model_hash,
    settings.haiku_model_hash,
  ];
  return hashes
    .filter((hash): hash is string => Boolean(hash))
    .filter((hash, index, all) => all.indexOf(hash) === index);
}

/**
 * 调整 Desktop 模型顺序。第一项就是客户端里的默认模型，所以只做相邻交换；
 * 越界时原样返回，按钮 disabled 只是视觉提示，逻辑仍然安全。
 */
export function moveClaudeModel(hashes: string[], index: number, offset: -1 | 1): string[] {
  const target = index + offset;
  if (index < 0 || index >= hashes.length || target < 0 || target >= hashes.length) return hashes;
  const next = [...hashes];
  [next[index], next[target]] = [next[target], next[index]];
  return next;
}

/** 模型选择器的选项：模型库在前，已失效的历史选择排在后面，便于取消。 */
export function claudeModelOptions(models: Model[], selected: string[]): ModelSelectOption[] {
  const options: ModelSelectOption[] = models.map((model) => ({
    value: model.model_hash,
    label: model.display_name || model.model_id,
    group: modelProviderName(model),
  }));
  const known = new Set(options.map((option) => option.value));
  for (const hash of selected) {
    if (hash && !known.has(hash)) options.push({ value: hash, label: hash, group: t("模型库里已不存在") });
  }
  return options;
}

/** 单个别名可以显式留空：网关会回落到默认模型。 */
export function claudeAliasOptions(models: Model[], selected: string[]): ModelSelectOption[] {
  return [
    { value: "", label: t("使用默认模型"), group: t("回落") },
    ...claudeModelOptions(models, selected),
  ];
}

export function claudeModelLabel(models: Model[], hash: string): string {
  const model = models.find((candidate) => candidate.model_hash === hash);
  return model?.display_name || model?.model_id || hash;
}

/** 网关发现 ID：Claude 客户端只保留 id 里含 `claude` 的条目，裸哈希会被过滤掉。 */
export function claudeDiscoveryId(hash: string): string {
  return `claude-${hash.toLowerCase()}`;
}

export interface ClaudeGuideItem {
  key: string;
  /** 项目名（保留客户端里的英文原词，便于对照）。 */
  label: string;
  /** 要填进客户端的值。 */
  value: string;
  hint: string;
}

/**
 * Claude Desktop 的引导项：Developer → Configure Third-Party Inference 需要逐项填写。
 *
 * 这条路径只能手工完成：我们不写 profile，也不假装按钮能代劳。
 */
export function claudeDesktopGuideItems(input: {
  baseUrl: string;
  token: string;
  models: string[];
  modelNames: (hash: string) => string;
}): ClaudeGuideItem[] {
  return [
    { key: "base_url", label: t("网关地址（Gateway base URL）"), value: input.baseUrl, hint: t("本机网关的地址；端口换过时以这里为准。") },
    { key: "api_key", label: t("静态 API 密钥（Static API key）"), value: input.token, hint: t("本机网关令牌，只在本机使用。") },
    { key: "auth", label: t("鉴权方式（Auth）"), value: "Bearer", hint: t("选 Bearer，不要选其它鉴权方式。") },
    {
      key: "models",
      label: t("模型列表（Models）"),
      value: input.models.map((hash) => `${claudeDiscoveryId(hash)}（${input.modelNames(hash)}）`).join("\n"),
      hint: t("第一项是默认模型；顺序与上面的列表一致。"),
    },
    { key: "discovery", label: t("模型发现（Model discovery）"), value: "Off", hint: t("关闭自动发现，只使用上面的模型列表。") },
    { key: "allow_code", label: t("允许 Code（Allow Code）"), value: "On", hint: t("打开后 Claude Code 才能通过这个网关。") },
  ];
}
