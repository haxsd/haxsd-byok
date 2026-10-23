import type { ModelDefinition, ModelSupport } from "cursor-byok:model";
import { accountData } from "./resources.ts";

const LANGUAGE_MODELS_URL = "https://api.x.ai/v1/language-models";
const MODELS_URL = "https://api.x.ai/v1/models";

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function modalities(value: unknown): string[] {
  return Array.isArray(value)
    ? value.flatMap((item) => (typeof item === "string" ? [item.toLowerCase()] : []))
    : [];
}

/** 把模型 ID 变成可读名称,如 grok-4-fast → Grok 4 Fast。 */
function displayName(id: string): string {
  return id
    .split("-")
    .map((part) => (/^\d/.test(part) ? part : part.charAt(0).toUpperCase() + part.slice(1)))
    .join(" ");
}

/** 兼容 /v1/language-models 的 models 数组与 /v1/models 的 data 数组。 */
export function parseGrokModels(body: unknown): ModelDefinition[] {
  const root = object(body);
  const source = root?.models ?? root?.data ?? body;
  if (!Array.isArray(source)) {
    throw new Error("Grok model discovery response does not contain a model list");
  }
  const seen = new Set<string>();
  const models: ModelDefinition[] = [];
  for (const raw of source) {
    const model = object(raw);
    const id = model ? text(model.id ?? model.name) : null;
    if (!id || seen.has(id)) continue;
    seen.add(id);
    const inputs = modalities(model?.input_modalities ?? model?.inputModalities);
    models.push({
      id,
      displayName: displayName(id),
      capabilities: {
        images: inputs.length === 0 || inputs.includes("image"),
      },
    });
  }
  return models;
}

type Attempt = { url: string; status: number; detail: string };

function failure(attempts: Attempt[]): Error {
  const unauthorized = attempts.find((attempt) => attempt.status === 401);
  if (unauthorized) {
    return new Error(
      `Grok model discovery was rejected (HTTP 401); sign in again to refresh the Grok credential: ${unauthorized.detail}`,
    );
  }
  const summary = attempts
    .map((attempt) => `${attempt.url} → HTTP ${attempt.status}: ${attempt.detail}`)
    .join("; ");
  return new Error(
    `Grok model discovery failed; the previous catalog is kept: ${summary}`,
  );
}

export const grokModels: ModelSupport = {
  list: async ({ resource }, context): Promise<ModelDefinition[]> => {
    if (!resource) throw new Error("add a Grok account before syncing models");
    const data = accountData(resource);
    const headers = {
      accept: "application/json",
      authorization: `Bearer ${data.accessToken}`,
    };
    // language-models 带模态与上下文元数据;不可用时回退到标准列表。
    // 两个端点都失败时抛出可诊断错误,由宿主保留上一次的模型目录;
    // 绝不用静态清单覆盖已发现的目录,401 也不降级成"部分可用"。
    const attempts: Attempt[] = [];
    for (const url of [LANGUAGE_MODELS_URL, MODELS_URL]) {
      const response = await context.network.fetch(url, { method: "GET", headers });
      if (response.status < 200 || response.status >= 300) {
        attempts.push({
          url,
          status: response.status,
          detail: response.body.trim().slice(0, 160),
        });
        continue;
      }
      let body: unknown;
      try {
        body = JSON.parse(response.body);
      } catch {
        attempts.push({ url, status: response.status, detail: "invalid JSON" });
        continue;
      }
      const models = parseGrokModels(body);
      if (models.length === 0) {
        attempts.push({ url, status: response.status, detail: "no models in response" });
        continue;
      }
      return models;
    }
    throw failure(attempts);
  },
};
