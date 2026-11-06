import type { DevinModelBinding, DevinModelChoice, Model } from "../../shared/api";
import type { ModelSelectOption } from "../../shared/ui/ModelSelect";
import { modelProviderName } from "../../shared/utils/modelProvider";
import { effectiveBindingModelHash, isNativeModelUid } from "./devinBindingEdits";

export function devinModelOptionLabel(model: Pick<Model, "display_name" | "tooltip_data" | "model_id" | "model_hash">): string {
  const displayName = model.display_name.trim();
  const remark = model.tooltip_data.trim();
  const normalizedRemark = remark.toLocaleLowerCase();
  const normalizedName = displayName.toLocaleLowerCase();
  if (remark && displayName && !normalizedName.includes(normalizedRemark) && !normalizedRemark.includes(normalizedName)) {
    return `${remark} · ${displayName}`;
  }
  return displayName || remark || model.model_id.trim() || model.model_hash;
}

export function devinModelOptions(models: Model[]): ModelSelectOption[] {
  return models.map((model) => ({
    value: model.model_hash,
    label: devinModelOptionLabel(model),
    group: modelProviderName(model),
  }));
}

/**
 * The explicit Devin default is a binding choice, not another model-library
 * choice: the label keeps the Devin-side name and the model that actually
 * answers. Disabled mappings are not selectable.
 */
export function devinDefaultModelOptions(
  bindings: DevinModelBinding[],
  models: Model[],
  choices: DevinModelChoice[],
): ModelSelectOption[] {
  const seen = new Set<string>();
  return bindings.flatMap((binding) => {
    if (!binding.enabled) return [];
    const uid = binding.model_uid.trim();
    if (!uid || seen.has(uid)) return [];
    seen.add(uid);

    const model = models.find(
      (candidate) => candidate.model_hash === effectiveBindingModelHash(binding),
    );
    const modelName = model ? devinModelOptionLabel(model) : t("模型已删除或不可用");
    const choiceName = isNativeModelUid(uid)
      ? model ? modelName : binding.display_name.trim() || modelName
      : choices.find((choice) => choice.uid.trim() === uid)?.name.trim() || uid;
    return [{
      value: uid,
      label: choiceName === modelName ? modelName : `${choiceName} → ${modelName}`,
      group: model ? modelProviderName(model) : t("模型库"),
    }];
  });
}
