import type { ClaudeSettings, Model } from "../../shared/api";
import type { ModelSelectOption } from "../../shared/ui/ModelSelect";
import { modelProviderName } from "../../shared/utils/modelProvider";

/**
 * 接管时会写入什么、选择器里能选什么。
 *
 * 状态与按钮可用性在 `claudeConnectionView.ts`；这里只回答"Desktop 的模型列表从哪来、
 * 怎么排序"，以及引导视图要展示的值。
 */

/**
 * Desktop 模型列表的初值：默认模型在前，随后是三个可选别名，按哈希去重。
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
    { key: "allow_code", label: t("允许 Code（Allow Code）"), value: "On", hint: t("打开后 Claude Desktop 才能执行代码。") },
  ];
}
