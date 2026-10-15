import type {
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import type { PluginContext } from "cursor-byok:plugin";
import { HttpError, streamOpenAiChat } from "cursor-byok:protocol/openai-chat";
import { grokModels } from "./models.ts";
import { type AccountData, accountData, quotaExhaustedPatch, RESOURCE_TYPE } from "./resources.ts";

const CHAT_URL = "https://api.x.ai/v1/chat/completions";

/** 流内错误只有文本可用,按积分/额度关键词分类。 */
export function isQuotaError(error: string): boolean {
  const message = error.toLowerCase();
  return message.includes("insufficient_quota") ||
    message.includes("credits exhausted") ||
    message.includes("out of credits") ||
    message.includes("quota_exceeded") ||
    (message.includes("429") &&
      (message.includes("quota") || message.includes("credit") ||
        message.includes("insufficient")));
}

/** 额度耗尽由响应体判断;普通 429 只表示短期限流。 */
function isQuotaHttpError(error: HttpError): boolean {
  const body = error.body.toLowerCase();
  return body.includes("insufficient_quota") ||
    body.includes("credits exhausted") ||
    body.includes("out of credits") ||
    // 免费账号触达消费上限时返回 403 spending-limit,属于额度而非授权问题。
    body.includes("spending-limit") ||
    body.includes("run out of credits") ||
    body.includes("quota_exceeded");
}

function rateLimitResult(error: HttpError): ProviderResult {
  const now = Date.now();
  const retryAfter = new Headers(error.headers).get("retry-after")?.trim();
  const seconds = retryAfter && /^\d+$/.test(retryAfter) ? Number(retryAfter) : NaN;
  const deadline = Number.isFinite(seconds)
    ? now + seconds * 1000
    : retryAfter ? Date.parse(retryAfter) : NaN;
  return {
    status: "resource-error",
    message: error.message,
    patch: {
      state: {
        status: "cooling",
        retryAtMs: Number.isFinite(deadline) && deadline > now ? deadline : now + 60_000,
        message: "Grok rate limit reached; wait before retrying",
      },
    },
  };
}

function invalidResult(message: string, stateMessage: string): ProviderResult {
  return {
    status: "resource-error",
    message,
    patch: { state: { status: "invalid", message: stateMessage } },
  };
}

async function invoke(
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<ProviderResult> {
  if (!input.resource) {
    return { status: "request-error", message: "add a Grok account before calling Grok" };
  }
  let data: AccountData;
  try {
    data = accountData(input.resource);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return invalidResult(message, message);
  }
  try {
    await streamOpenAiChat(
      {
        url: CHAT_URL,
        model: input.model.id,
        // xAI 不接受 reasoning_effort 与 service_tier;思考由模型自身决定。
        request: {
          ...input.request,
          reasoning: { enabled: false, effort: null },
          latency: "standard",
        },
        headers: { authorization: `Bearer ${data.accessToken}` },
      },
      output,
      context,
    );
    return { status: "completed" };
  } catch (error) {
    if (error instanceof HttpError) {
      // 403 可能只是模型权限或内容限制,不能据此停用整个账号。
      if (error.status === 401 && !isQuotaHttpError(error)) {
        return invalidResult(error.message, "Grok authorization expired; sign in again");
      }
      if (isQuotaHttpError(error)) {
        return {
          status: "resource-error",
          message: error.message,
          patch: quotaExhaustedPatch(data),
        };
      }
      if (error.status === 429) return rateLimitResult(error);
      return { status: "request-error", message: error.message };
    }
    const message = error instanceof Error ? error.message : String(error);
    if (isQuotaError(message)) {
      return { status: "resource-error", message, patch: quotaExhaustedPatch(data) };
    }
    return { status: "request-error", message };
  }
}

export const grokProvider: ProviderSupport = {
  id: "grok",
  displayName: "xAI Grok",
  description: {
    "en-US": "SuperGrok subscription access through the official Grok CLI endpoint.",
    "zh-CN": "通过官方 Grok CLI 接口使用 SuperGrok 订阅。",
  },
  providerType: "xai",
  resourceType: RESOURCE_TYPE,
  models: grokModels,
  invoke,
};
