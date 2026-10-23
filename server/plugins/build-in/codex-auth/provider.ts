import type {
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import { HttpError, streamOpenAiResponses } from "cursor-byok:protocol/openai-responses";
import { codexModels, reasoningEfforts } from "./models.ts";
import {
  accessTokenNeedsRefresh,
  type AccountData,
  accountData,
  AUTH_EXPIRED_MESSAGE,
  chatGptAccountId,
  quotaExhaustedPatch,
  refreshCredentialForAccount,
  RESOURCE_TYPE,
} from "./resources.ts";

const RESPONSES_URL = "https://chatgpt.com/backend-api/codex/responses";

/** 流内错误只有文本可用,按额度关键词分类。 */
export function isQuotaError(error: string): boolean {
  const message = error.toLowerCase();
  return message.includes("insufficient_quota") ||
    message.includes("usage_limit_reached") ||
    message.includes("exceeded your current quota") ||
    message.includes("quota_exceeded") ||
    message.includes("5-hour") ||
    message.includes("5 hour") ||
    (message.includes("429") &&
      (message.includes("quota") || message.includes("usage_limit") ||
        message.includes("insufficient")));
}

/** HTTP 失败携带结构化状态码,429 时放宽响应体的匹配条件。 */
function isQuotaHttpError(error: HttpError): boolean {
  const body = error.body.toLowerCase();
  return body.includes("insufficient_quota") ||
    body.includes("usage_limit_reached") ||
    body.includes("exceeded your current quota") ||
    body.includes("quota_exceeded") ||
    body.includes("5-hour") ||
    body.includes("5 hour") ||
    (error.status === 429 &&
      (body.includes("quota") || body.includes("usage_limit") || body.includes("insufficient")));
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

function headers(data: AccountData, cacheKey: string | null): Record<string, string> {
  const result: Record<string, string> = {
    authorization: `Bearer ${data.accessToken}`,
    originator: "codex_cli_rs",
  };
  const accountId = chatGptAccountId(data.accessToken);
  if (accountId) result["ChatGPT-Account-Id"] = accountId;
  // Codex 后端的缓存亲和契约:session-id / thread-id / prompt_cache_key
  // 三者同源(见 codex-rs client.rs);缺头会导致请求落在随机分片上。
  if (cacheKey !== null) {
    result["session-id"] = cacheKey;
    result["thread-id"] = cacheKey;
    result["x-client-request-id"] = cacheKey;
  }
  return result;
}

async function streamCodex(
  data: AccountData,
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<void> {
  const efforts = reasoningEfforts(input.model);
  const reasoning = input.request.reasoning;
  const effort = reasoning.effort !== null && efforts.includes(reasoning.effort)
    ? reasoning.effort
    : null;
  await streamOpenAiResponses(
    {
      url: RESPONSES_URL,
      model: input.model.id,
      // Codex 订阅端点不接受 max_output_tokens;fast 档位经协议库映射为
      // service_tier: "priority" 后透传。
      request: {
        ...input.request,
        reasoning: { enabled: reasoning.enabled, effort },
        maxOutputTokens: null,
      },
      headers: headers(data, input.request.cacheKey),
      extraBody: { store: false },
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
    if (error.status === 401) return invalidResult(error.message, AUTH_EXPIRED_MESSAGE, persist);
    if (isQuotaHttpError(error)) {
      return {
        status: "resource-error",
        message: error.message,
        patch: quotaExhaustedPatch(data, error.body),
      };
    }
    return {
      status: "request-error",
      message: error.message,
      ...(persist ? { patch: { privateData: persist as unknown as JsonValue } } : {}),
    };
  }
  const message = error instanceof Error ? error.message : String(error);
  if (isQuotaError(message)) {
    return { status: "resource-error", message, patch: quotaExhaustedPatch(data, message) };
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
    return { status: "request-error", message: "add a ChatGPT account before calling Codex" };
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
  // 过期前刷新:access token 进入 5 分钟窗口就先换新,避免每次多一次 401 往返。
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
    await streamCodex(data, input, output, context);
    return completed(refreshedData);
  } catch (error) {
    if (!(error instanceof HttpError) || error.status !== 401) {
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
      return {
        status: "request-error",
        message: `${error.message}; ${refreshed.message}`,
      };
    }
    data = refreshed.data;
    try {
      await streamCodex(data, input, output, context);
      return completed(data);
    } catch (retryError) {
      return failure(retryError, data, data);
    }
  }
}

export const codexProvider: ProviderSupport = {
  id: "codex",
  displayName: "OpenAI Codex",
  description: {
    "en-US": "ChatGPT subscription access through the official Codex Responses API.",
    "zh-CN": "通过官方 Codex Responses API 使用 ChatGPT 订阅。",
  },
  providerType: "openai",
  resourceType: RESOURCE_TYPE,
  models: codexModels,
  invoke,
};
