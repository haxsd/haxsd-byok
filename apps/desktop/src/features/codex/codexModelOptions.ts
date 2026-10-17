import type { Model, PluginModelDescriptor } from "../../shared/api";
import type { ModelSelectOption } from "../../shared/ui/ModelSelect";
import { claudeIcon, openAiIcon } from "../../shared/ui/icons";
import { modelProviderName } from "../../shared/utils/modelProvider";

/**
 * Codex 页能提供哪些模型。
 *
 * Codex 网关把设置里的每个 model_hash 直接拿去读模型库（`Store::model`），并按模型配置
 * 生成 Codex 的能力目录：插件提供的模型（模型库里单独一节，例如 Grok）没有模型库记录，
 * 选进设置后只会在应用配置或第一次请求时失败。所以这里把选择范围收紧到网关真的能发布的
 * 那些模型，并把不能用的原因带回界面。
 *
 * 另外两种"选了却不可用"的状态也要能被看见、被取消，而不是在界面上留一个选不中的幽灵：
 * 模型库里已被删除的历史选择，以及插件模型的历史选择。
 */
export interface CodexModelAvailability {
  /** 可以选进 Codex 的模型；含已失效的历史选择，便于用户取消它们。 */
  options: ModelSelectOption[];
  /** 模型库里有、但 Codex 网关用不了的插件模型。 */
  blockedPluginModels: PluginModelDescriptor[];
  /** 已选、但模型库里已经没有的标识。 */
  missingSelections: string[];
  /** 已选、但因为是插件模型而不能用的标识。 */
  blockedPluginSelections: string[];
}

function builtInOption(model: Model): ModelSelectOption {
  return {
    value: model.model_hash,
    label: model.display_name || model.model_id,
    group: modelProviderName(model),
    icon: model.type === "anthropic" ? claudeIcon : openAiIcon,
  };
}

export function codexModelAvailability(
  models: Model[],
  pluginModels: PluginModelDescriptor[],
  selected: string[],
): CodexModelAvailability {
  const pluginIds = new Set(pluginModels.map((model) => model.id));
  const options = models
    .filter((model) => !pluginIds.has(model.model_hash))
    .map(builtInOption);
  const known = new Set(options.map((option) => option.value));

  const missingSelections = selected.filter((hash) => !known.has(hash) && !pluginIds.has(hash));
  const blockedPluginSelections = selected.filter((hash) => pluginIds.has(hash));
  for (const hash of missingSelections) {
    options.push({ value: hash, label: hash, group: t("模型库里已不存在") });
  }
  for (const hash of blockedPluginSelections) {
    const model = pluginModels.find((candidate) => candidate.id === hash);
    options.push({ value: hash, label: model?.displayName ?? hash, group: t("插件模型（Codex 不能用）") });
  }

  return {
    options,
    blockedPluginModels: pluginModels.filter((model) => !selected.includes(model.id)),
    missingSelections,
    blockedPluginSelections,
  };
}
