import type {
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import { HttpError, streamOpenAiChat } from "cursor-byok:protocol/openai-chat";
import { grokModels } from "./models.ts";
import {
  accessTokenNeedsRefresh,
  type AccountData,
  accountData,
  AUTH_EXPIRED_MESSAGE,
  quotaExhaustedPatch,
  refreshCredentialForAccount,
  RESOURCE_TYPE,
} from "./resources.ts";

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
    : retryAfter
    ? Date.parse(retryAfter)
    : NaN;
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

function invalidResult(
  message: string,
  stateMessage: string,
  data: AccountData | null = null,
): ProviderResult {
  return {
    status: "resource-error",
    message,
    patch: {
      ...(data ? { privateData: data as unknown as JsonValue } : {}),
      state: { status: "invalid", message: stateMessage },
    },
  };
}

function completed(data: AccountData | null): ProviderResult {
  return data === null ? { status: "completed" } : {
    status: "completed",
    patch: { privateData: data as unknown as JsonValue, state: { status: "ready" } },
  };
}

async function streamGrok(
  data: AccountData,
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<void> {
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
}

/** 不触发刷新的失败分类;刷新后的重试也走这里。
 * `persist` 是本次调用已换到的新凭证——即使请求失败也要回写,避免下一轮重复刷新。 */
function failure(
  error: unknown,
  data: AccountData,
  persist: AccountData | null = null,
): ProviderResult {
  if (error instanceof HttpError) {
    if (error.status === 401 && !isQuotaHttpError(error)) {
      return invalidResult(error.message, AUTH_EXPIRED_MESSAGE, persist);
    }
    if (isQuotaHttpError(error)) {
      return {
        status: "resource-error",
        message: error.message,
        patch: quotaExhaustedPatch(data),
      };
    }
    if (error.status === 429) return rateLimitResult(error);
    return {
      status: "request-error",
      message: error.message,
      ...(persist ? { patch: { privateData: persist as unknown as JsonValue } } : {}),
    };
  }
  const message = error instanceof Error ? error.message : String(error);
  if (isQuotaError(message)) {
    return { status: "resource-error", message, patch: quotaExhaustedPatch(data) };
  }
  return {
    status: "request-error",
    message,
    ...(persist ? { patch: { privateData: persist as unknown as JsonValue } } : {}),
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
  const accountKey = input.resource.key;

  let refreshedData: AccountData | null = null;
  // 过期前刷新:access token 进入 5 分钟窗口就先换新。
  // 与 401 刷新共用按账号的 single-flight,并发请求不会各用一次 refresh token。
  if (accessTokenNeedsRefresh(data)) {
    const refreshed = await refreshCredentialForAccount(accountKey, data, context);
    if (refreshed.status === "invalid") {
      return invalidResult(refreshed.message, refreshed.message);
    }
    if (refreshed.status === "failed") {
      return { status: "request-error", message: refreshed.message };
    }
    data = refreshed.data;
    refreshedData = data;
  }

  try {
    await streamGrok(data, input, output, context);
    return completed(refreshedData);
  } catch (error) {
    // 403 可能只是模型权限或内容限制,不能据此停用整个账号;只有 401 才刷新。
    if (!(error instanceof HttpError) || error.status !== 401 || isQuotaHttpError(error)) {
      return failure(error, data, refreshedData);
    }
    // 本次请求已经刷新过:刷新后的令牌仍被拒,授权确实失效,不再刷新第二次。
    // 401 在协议库读到响应头时抛出,尚未发出任何事件,所以这里直接收尾是安全的。
    if (refreshedData !== null) {
      return invalidResult(error.message, AUTH_EXPIRED_MESSAGE, refreshedData);
    }
    // 401 说明 access token 被拒:刷新一次并重试一次,刷新判定失效才标 invalid。
    const refreshed = await refreshCredentialForAccount(accountKey, data, context);
    if (refreshed.status === "invalid") {
      return invalidResult(error.message, refreshed.message);
    }
    if (refreshed.status === "failed") {
      return { status: "request-error", message: `${error.message}; ${refreshed.message}` };
    }
    data = refreshed.data;
    try {
      await streamGrok(data, input, output, context);
      return completed(data);
    } catch (retryError) {
      return failure(retryError, data, data);
    }
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
