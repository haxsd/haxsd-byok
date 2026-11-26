import type { JsonValue } from "cursor-byok:plugin";
import type { ModelDefinition, ModelSnapshot, ModelSupport } from "cursor-byok:model";
import { accountData, accountHeaders } from "./resources.ts";

const CODEX_MODELS_URL = "https://chatgpt.com/backend-api/codex/models";
/**
 * Codex 模型目录的 API 客户端版本，不是 Codex CLI 的发布版本。
 * 端点把它当作兼容性参数；只有官方目录契约变化时才调整，避免跟着 CLI 版本乱跳。
 */
export const CODEX_MODEL_CLIENT_VERSION = "1.0.0";
export const CODEX_MODELS_URL_WITH_VERSION =
  `${CODEX_MODELS_URL}?client_version=${CODEX_MODEL_CLIENT_VERSION}`;

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

/**
 * 官方 Codex 在字段缺失时默认按“文本 + 图片”处理；只有上游显式给出
 * input_modalities 且不含 image 时，才关闭图片能力。
 */
function inputImages(model: Record<string, unknown>): boolean {
  const source = model.input_modalities ?? model.inputModalities;
  if (!Array.isArray(source)) return true;
  return source.some((item) => {
    if (typeof item === "string") return item.toLowerCase() === "image";
    const entry = object(item);
    return text(entry?.type ?? entry?.id ?? entry?.value)?.toLowerCase() === "image";
  });
}

function positiveInteger(value: unknown): number | null {
  const parsed = typeof value === "number"
    ? value
    : typeof value === "string"
    ? Number(value)
    : NaN;
  return Number.isFinite(parsed) && parsed > 0 ? Math.floor(parsed) : null;
}

function parseReasoningEfforts(model: Record<string, unknown>): string[] {
  const source = model.supported_reasoning_levels ??
    model.supportedReasoningLevels ??
    model.reasoning_levels ??
    model.reasoningLevels ??
    model.supported_reasoning_efforts ??
    model.supportedReasoningEfforts ??
    model.reasoning_efforts ??
    model.reasoningEfforts;
  if (!Array.isArray(source)) return [];
  const values = source.flatMap((item) => {
    if (typeof item === "string") return [item.trim()];
    const entry = object(item);
    const value = text(entry?.effort ?? entry?.id ?? entry?.value ?? entry?.name);
    return value ? [value] : [];
  }).filter(Boolean);
  return [...new Set(values)];
}

function modelId(value: unknown): string | null {
  if (typeof value === "string") return text(value);
  const model = object(value);
  return model ? text(model.slug ?? model.id ?? model.model ?? model.name) : null;
}

export function parseOfficialModels(body: unknown): ModelDefinition[] {
  const root = object(body);
  const source = root?.models ?? root?.data ?? body;
  if (!Array.isArray(source)) {
    throw new Error("Codex model discovery response does not contain a model list");
  }
  const seen = new Set<string>();
  const models: ModelDefinition[] = [];
  for (const raw of source) {
    const model = object(raw);
    if (!model || model.supported_in_api === false || model.supportedInApi === false) continue;
    if (text(model.visibility)?.toLowerCase() === "hidden") continue;
    const id = modelId(model);
    if (!id || seen.has(id)) continue;
    seen.add(id);
    const efforts = parseReasoningEfforts(model);
    const description = text(model.description);
    const maxOutputTokens = positiveInteger(
      model.max_output_tokens ?? model.maxOutputTokens ?? model.max_completion_tokens ??
        model.maxCompletionTokens,
    );
    models.push({
      id,
      displayName: text(model.display_name ?? model.displayName ?? model.title ?? model.name) ??
        id,
      ...(description ? { description } : {}),
      ...(maxOutputTokens !== null ? { maxOutputTokens } : {}),
      capabilities: { images: inputImages(model) },
      privateData: { reasoningEfforts: efforts },
    });
  }
  const defaultModel = modelId(
    root?.default_model ??
      root?.defaultModel ??
      root?.default_model_slug ??
      root?.defaultModelSlug ??
      root?.primary_model ??
      root?.primaryModel,
  );
  // 把上游默认模型排在最前,让宿主自然选中它。
  if (defaultModel) {
    models.sort((left, right) =>
      Number(right.id === defaultModel) - Number(left.id === defaultModel)
    );
  }
  return models;
}

export function reasoningEfforts(model: ModelSnapshot): string[] {
  const data = object(model.privateData);
  const efforts = data?.reasoningEfforts;
  return Array.isArray(efforts) ? efforts.filter((item) => typeof item === "string") : [];
}

export const codexModels: ModelSupport = {
  list: async ({ resource }, context): Promise<ModelDefinition[]> => {
    if (!resource) throw new Error("add a ChatGPT account before syncing Codex models");
    const data = accountData(resource);
    const response = await context.network.fetch(CODEX_MODELS_URL_WITH_VERSION, {
      method: "GET",
      headers: accountHeaders(data),
    });
    if (response.status < 200 || response.status >= 300) {
      // 宿主在 list 抛错时保留上一次目录;这里必须带够诊断信息,而不是返回空数组
      // (返回空数组会被宿主当成"成功但没有模型")。
      throw new Error(discoveryFailure(response.status, response.body));
    }
    let body: unknown;
    try {
      body = JSON.parse(response.body) as JsonValue;
    } catch {
      throw new Error("Codex model discovery returned invalid JSON; the previous catalog is kept");
    }
    const models = parseOfficialModels(body);
    if (models.length === 0) {
      throw new Error(
        "Codex model discovery returned no supported models; the previous catalog is kept",
      );
    }
    return models;
  },
};

function discoveryFailure(status: number, body: string): string {
  const detail = body.trim().slice(0, 200);
  if (status === 401) {
    return `Codex model discovery was rejected (HTTP 401); sign in again to refresh the ChatGPT session: ${detail}`;
  }
  return `Codex model discovery failed (HTTP ${status}); the previous catalog is kept: ${detail}`;
}
