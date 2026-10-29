import type { Model, ModelInput } from "../../shared/api";

const normalizedUrl = (value: string) => value.trim().replace(/\/+$/, "");

/** Provider discovery can repeat ids; preserve the first occurrence and drop blanks. */
export function discoveryCandidates(values: string[]) {
  const seen = new Set<string>();
  const result: string[] = [];
  for (const value of values) {
    const modelId = value.trim();
    if (!modelId || seen.has(modelId)) continue;
    seen.add(modelId);
    result.push(modelId);
  }
  return result;
}

/**
 * Models already configured for the same provider endpoint. The server rejects
 * duplicate fingerprints for the whole batch, so the picker disables these rows
 * instead of letting one duplicate fail every selected model.
 */
export function existingDiscoveryIds(
  library: Model[],
  base: Pick<ModelInput, "type" | "base_url" | "use_full_url" | "openai_endpoint">,
) {
  const baseUrl = normalizedUrl(base.base_url);
  const ids = new Set<string>();
  for (const model of library) {
    if (model.type !== base.type) continue;
    if (normalizedUrl(model.base_url) !== baseUrl) continue;
    if (model.use_full_url !== base.use_full_url) continue;
    if (model.type === "openai" && model.openai_endpoint !== base.openai_endpoint) continue;
    ids.add(model.model_id);
  }
  return ids;
}

/**
 * Copy the provider-wide settings to each selected model, changing only the
 * identity and display metadata. Display names and notes are intentionally
 * deterministic: users can edit them from the library afterwards.
 */
export function bulkModelInputs(
  base: ModelInput,
  modelIds: string[],
  startOrder: number,
  providerLabel: string,
): ModelInput[] {
  const label = providerLabel.trim();
  return modelIds.map((modelId, index) => ({
    ...base,
    sort_order: startOrder + index,
    model_id: modelId,
    display_name: modelId,
    tooltip_data: label ? `${label} · ${modelId}` : modelId,
  }));
}
