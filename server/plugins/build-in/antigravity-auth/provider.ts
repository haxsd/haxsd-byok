import type {
  LlmMessage,
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import type { ResourcePatch, ResourceSnapshot } from "cursor-byok:resource";
import { HttpError } from "cursor-byok:protocol/openai-chat";
import {
  ANTIGRAVITY_CLIENT_HEADERS,
  ANTIGRAVITY_ENDPOINTS,
  ANTIGRAVITY_USER_AGENT,
  antigravityModels,
} from "./models.ts";
import {
  type AccountData,
  accountData,
  AUTH_EXPIRED_MESSAGE,
  isTokenExpired,
  quotaExhaustedPatch,
  refreshAccount,
  RESOURCE_TYPE,
} from "./resources.ts";

export function isQuotaError(error: string): boolean {
  const message = error.toLowerCase();
  return message.includes("resource_exhausted") ||
    message.includes("quota_exceeded") ||
    message.includes("quota_exhausted") ||
    message.includes("rate_limit_exceeded") ||
    message.includes("rate limit") ||
    message.includes("model_capacity_exhausted") ||
    message.includes("too many requests") ||
    message.includes("429");
}

function isQuotaHttpError(error: HttpError): boolean {
  if (error.status === 429) return true;
  const body = error.body.toLowerCase();
  return body.includes("resource_exhausted") ||
    body.includes("quota_exceeded") ||
    body.includes("quota_exhausted") ||
    body.includes("rate_limit_exceeded") ||
    body.includes("rate limit") ||
    body.includes("model_capacity_exhausted") ||
    body.includes("user rate limit exceeded") ||
    body.includes("too many requests");
}

function invalidResult(
  message: string,
  stateMessage: string,
  privateData: JsonValue | null = null,
): ProviderResult {
  return {
    status: "resource-error",
    message,
    patch: {
      ...(privateData ? { privateData } : {}),
      state: { status: "invalid", message: stateMessage },
    },
  };
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 不触发刷新的失败分类;刷新后的重试也走这里。 */
function failure(error: unknown, data: AccountData): ProviderResult {
  if (error instanceof HttpError) {
    // 401 是授权失败;403 还可能是权限/模型限制,不能据此停用整个账号。
    if (error.status === 401) return invalidResult(error.message, AUTH_EXPIRED_MESSAGE);
    if (isQuotaHttpError(error)) {
      return {
        status: "resource-error",
        message: error.message,
        patch: quotaExhaustedPatch(data, error.body),
      };
    }
    return { status: "request-error", message: error.message };
  }
  const message = errorMessage(error);
  if (isQuotaError(message)) {
    return { status: "resource-error", message, patch: quotaExhaustedPatch(data, message) };
  }
  return { status: "request-error", message };
}

async function readBody(lines: AsyncIterable<string>): Promise<string> {
  const collected: string[] = [];
  for await (const line of lines) collected.push(line);
  return collected.join("\n");
}

function resolveAntigravityModel(modelId: string): string {
  const raw = modelId.trim();
  const lower = raw.toLowerCase();

  // 1. If explicit tier is already specified in the model ID, pass it directly!
  if (
    lower.startsWith("gemini-3.8-flash-") ||
    lower.startsWith("gemini-3.7-flash-") ||
    lower.startsWith("gemini-3.6-flash-") ||
    lower.startsWith("gemini-3.1-pro-") ||
    lower === "gemini-3.8-flash" ||
    lower === "gemini-3.7-flash" ||
    lower === "gemini-3.6-flash" ||
    lower === "gemini-2.5-flash" ||
    lower === "gemini-2.5-pro" ||
    lower === "gemini-2.0-flash" ||
    lower === "claude-sonnet-4-6" ||
    lower === "claude-sonnet-4-6-thinking" ||
    lower === "claude-opus-4-6-thinking" ||
    lower === "gemini-3.1-flash-image" ||
    lower === "gpt-oss-120b-medium"
  ) {
    if (lower === "gemini-3.1-pro-high") return "gemini-pro-agent";
    return raw;
  }

  // 2. Canonical Antigravity-Manager mapping for aliases
  if (
    lower === "claude-3-7-sonnet" || lower === "claude-3-5-sonnet" || lower === "claude-sonnet-4-5"
  ) {
    return "claude-sonnet-4-6";
  }
  if (lower === "claude-3-5-haiku" || lower === "claude-haiku-4") {
    return "claude-sonnet-4-6";
  }
  if (
    lower === "claude-3-7-opus" || lower === "claude-opus-4" || lower === "claude-opus-4.6" ||
    lower === "claude-opus-4-5-thinking"
  ) {
    return "claude-opus-4-6-thinking";
  }
  if (
    lower === "gpt-4" || lower === "gpt-4o" || lower === "gpt-4o-mini" || lower === "gpt-3.5-turbo"
  ) {
    return "gemini-2.5-flash";
  }
  if (lower === "gemini-2.5-flash-lite") {
    return "gemini-2.5-flash";
  }
  if (lower === "gemini-3-flash" || lower === "gemini-3.5-flash") {
    return "gemini-3.7-flash";
  }
  if (lower === "gemini-3-pro" || lower === "gemini-3.1-pro") {
    return "gemini-3.1-pro-preview";
  }
  if (lower === "gemini-3-pro-high") {
    return "gemini-pro-agent";
  }

  return raw;
}

function randomHex(length = 8): string {
  const array = new Uint8Array(Math.ceil(length / 2));
  crypto.getRandomValues(array);
  return Array.from(array, (byte) => byte.toString(16).padStart(2, "0")).join("").slice(0, length);
}

function generateRequestId(): string {
  return `agent/${Date.now()}/${randomHex(8)}`;
}

/** 上游 CloudCode 的默认输出上限;模型目录没有声明值时使用。 */
const DEFAULT_MAX_OUTPUT_TOKENS = 65_536;

/**
 * 输出上限来自客户端请求,并夹到模型目录声明的上限内:
 * 请求更大时按目录截断,请求更小时尊重请求。
 */
export function outputTokenLimit(requested: number | null, declared: number | null): number {
  const declaredLimit = declared !== null && Number.isFinite(declared) && declared > 0
    ? Math.floor(declared)
    : null;
  const requestedLimit = requested !== null && Number.isFinite(requested) && requested > 0
    ? Math.floor(requested)
    : null;
  if (requestedLimit === null) return declaredLimit ?? DEFAULT_MAX_OUTPUT_TOKENS;
  if (declaredLimit === null) return requestedLimit;
  return Math.min(requestedLimit, declaredLimit);
}

// Keys the CloudCode v1internal Schema proto rejects with "Cannot find field"
const UNSUPPORTED_SCHEMA_KEYS: Record<string, true> = {
  "$schema": true,
  "$ref": true,
  "$defs": true,
  "$comment": true,
  "examples": true,
  "unevaluatedProperties": true,
  "unevaluatedItems": true,
  "patternProperties": true,
  "propertyNames": true,
  "exclusiveMinimum": true,
  "exclusiveMaximum": true,
  "multipleOf": true,
  "dependencies": true,
  "dependentSchemas": true,
  "dependentRequired": true,
  "deprecated": true,
  "readOnly": true,
  "writeOnly": true,
  "x-mcp-header": true,
  "const": true,
  "default": true,
  "additionalProperties": true,
  "title": true,
  "format": true,
};

const PROTO_TYPE_MAP: Record<string, string> = {
  string: "STRING",
  number: "NUMBER",
  integer: "INTEGER",
  boolean: "BOOLEAN",
  array: "ARRAY",
  object: "OBJECT",
};

function enforceUppercaseTypes(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(enforceUppercaseTypes);
  if (value === null || typeof value !== "object") return value;
  const out: Record<string, unknown> = {};
  for (const [key, child] of Object.entries(value as Record<string, unknown>)) {
    if (UNSUPPORTED_SCHEMA_KEYS[key]) continue;
    if (key === "type" && typeof child === "string") {
      out[key] = PROTO_TYPE_MAP[child.toLowerCase()] ?? child.toUpperCase();
    } else {
      out[key] = enforceUppercaseTypes(child);
    }
  }
  if (!out.type && out.properties) {
    out.type = "OBJECT";
  }
  return out;
}

function sanitizeSchema(value: unknown): unknown {
  if (!value || typeof value !== "object") {
    return { type: "OBJECT", properties: {} };
  }
  const clean = enforceUppercaseTypes(value) as Record<string, unknown>;
  if (!clean.type) clean.type = "OBJECT";
  return clean;
}

function convertToCloudCodeContents(
  instructions: string,
  messages: LlmMessage[],
): {
  contents: Array<{ role: string; parts: Array<Record<string, unknown>> }>;
  systemInstruction?: { role: string; parts: Array<{ text: string }> };
} {
  const rawContents: Array<{ role: string; parts: Array<Record<string, unknown>> }> = [];
  let systemText = instructions || "";

  for (const msg of messages) {
    if (msg.role === "system") {
      const txt = msg.content
        .map((p) => (p.type === "text" ? p.text : ""))
        .filter(Boolean)
        .join("\n");
      if (txt) {
        systemText += (systemText ? "\n\n" : "") + txt;
      }
      continue;
    }

    if (msg.role === "assistant") {
      const parts: Array<Record<string, unknown>> = [];
      if (msg.text) {
        parts.push({ text: msg.text });
      }

      const replayVal = msg.replayState?.providerKind === "antigravity"
        ? (msg.replayState.value as Record<string, unknown> | null)
        : null;
      const sig = typeof replayVal?.thoughtSignature === "string"
        ? replayVal.thoughtSignature
        : null;

      for (const call of msg.toolCalls) {
        parts.push({
          functionCall: {
            name: call.name,
            args: typeof call.arguments === "object" && call.arguments !== null
              ? call.arguments
              : {},
          },
          thoughtSignature: sig || "skip_thought_signature_validator",
        });
      }

      if (parts.length > 0) {
        rawContents.push({ role: "model", parts });
      }
    } else if (msg.role === "tool") {
      rawContents.push({
        role: "user",
        parts: [
          {
            functionResponse: {
              name: msg.name || "function",
              response: { result: msg.content },
            },
          },
        ],
      });
    } else if (msg.role === "user") {
      const parts: Array<Record<string, unknown>> = [];
      for (const p of msg.content) {
        if (p.type === "text") {
          if (p.text) parts.push({ text: p.text });
        } else if (p.type === "image") {
          parts.push({
            inlineData: {
              mimeType: p.mediaType,
              data: p.dataBase64,
            },
          });
        }
      }
      if (parts.length === 0) {
        parts.push({ text: " " });
      }
      rawContents.push({ role: "user", parts });
    }
  }

  // Merge consecutive same-role messages so contents strictly alternate user -> model -> user -> model
  const contents: Array<{ role: string; parts: Array<Record<string, unknown>> }> = [];
  for (const item of rawContents) {
    if (item.parts.length === 0) continue;
    const last = contents[contents.length - 1];
    if (last && last.role === item.role) {
      last.parts.push(...item.parts);
    } else {
      contents.push(item);
    }
  }

  if (contents.length > 0 && contents[0].role !== "user") {
    contents.unshift({ role: "user", parts: [{ text: " " }] });
  }

  return {
    contents,
    ...(systemText.trim()
      ? { systemInstruction: { role: "system", parts: [{ text: systemText.trim() }] } }
      : {}),
  };
}

async function streamCloudCode(
  accessToken: string,
  projectId: string,
  modelId: string,
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<void> {
  const actualModel = resolveAntigravityModel(modelId);
  const { contents, systemInstruction } = convertToCloudCodeContents(
    input.request.instructions,
    input.request.messages,
  );

  const tools = input.request.tools && input.request.tools.length > 0
    ? [
      {
        functionDeclarations: input.request.tools.map((t) => ({
          name: t.name,
          description: t.description || "",
          parameters: sanitizeSchema(t.parameters),
        })),
      },
    ]
    : undefined;

  const toolConfig = tools
    ? {
      functionCallingConfig: { mode: "AUTO" },
    }
    : undefined;

  const payload = {
    project: projectId || "bamboo-precept-lgxtn",
    model: actualModel,
    userAgent: "antigravity",
    requestType: "agent",
    requestId: generateRequestId(),
    enabledCreditTypes: ["GOOGLE_ONE_AI"],
    request: {
      contents,
      ...(systemInstruction ? { systemInstruction } : {}),
      ...(tools ? { tools } : {}),
      ...(toolConfig ? { toolConfig } : {}),
      generationConfig: {
        maxOutputTokens: outputTokenLimit(
          input.request.maxOutputTokens,
          input.model.maxOutputTokens ?? null,
        ),
      },
    },
  };

  const headers: Record<string, string> = {
    authorization: `Bearer ${accessToken}`,
    "content-type": "application/json",
    accept: "text/event-stream",
    "user-agent": ANTIGRAVITY_USER_AGENT,
    ...ANTIGRAVITY_CLIENT_HEADERS,
  };
  if (actualModel.toLowerCase().includes("claude")) {
    headers["anthropic-beta"] =
      "claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
  }

  let lastError: Error | null = null;
  let hasEmittedAnyChunk = false;

  for (const endpoint of ANTIGRAVITY_ENDPOINTS) {
    if (hasEmittedAnyChunk) break;

    try {
      const response = await context.network.stream(
        `${endpoint}/v1internal:streamGenerateContent?alt=sse`,
        {
          method: "POST",
          headers,
          body: JSON.stringify(payload),
        },
      );

      if (response.status < 200 || response.status >= 300) {
        const errorBody = await readBody(response.lines);
        lastError = new HttpError(response.status, errorBody);
        if (
          response.status === 503 || response.status === 502 || response.status === 504 ||
          response.status === 404
        ) {
          continue;
        }
        throw lastError;
      }

      let textStarted = false;
      let thinkingStarted = false;
      let doneEmitted = false;
      let hasTools = false;
      let toolIndex = 0;
      let lastThoughtSignature: string | null = null;
      let finalUsage: {
        inputTokens: number | null;
        outputTokens: number | null;
        totalTokens: number | null;
        cacheReadTokens: number | null;
      } | null = null;

      for await (const line of response.lines) {
        if (!line.startsWith("data:")) continue;
        const raw = line.slice(5).trim();
        if (!raw || raw === "[DONE]") break;

        let json: Record<string, unknown>;
        try {
          json = JSON.parse(raw) as Record<string, unknown>;
        } catch {
          continue;
        }

        const resp = (json.response as Record<string, unknown> | undefined) ?? json;
        if (!resp) continue;

        const usage = resp.usageMetadata as Record<string, number> | undefined;
        if (usage) {
          finalUsage = {
            inputTokens: typeof usage.promptTokenCount === "number" ? usage.promptTokenCount : null,
            outputTokens: typeof usage.candidatesTokenCount === "number"
              ? usage.candidatesTokenCount
              : null,
            totalTokens: typeof usage.totalTokenCount === "number" ? usage.totalTokenCount : null,
            cacheReadTokens: typeof usage.cachedContentTokenCount === "number"
              ? usage.cachedContentTokenCount
              : null,
          };
        }

        const candidates = resp.candidates as Array<Record<string, unknown>> | undefined;
        const candidate = candidates?.[0];
        const content = candidate?.content as Record<string, unknown> | undefined;
        const parts = content?.parts as Array<Record<string, unknown>> | undefined;

        if (parts) {
          for (const part of parts) {
            const sig = typeof part.thoughtSignature === "string" ? part.thoughtSignature : null;
            if (sig) {
              lastThoughtSignature = sig;
            }

            const isThought = part.thought === true;
            const textPart = typeof part.text === "string" ? part.text : null;

            if (isThought && textPart) {
              const cleanThought = textPart.replace(/<\/?think>/gi, "");
              if (cleanThought) {
                hasEmittedAnyChunk = true;
                if (!thinkingStarted) {
                  thinkingStarted = true;
                  output.emit({ type: "thinking-start" });
                }
                output.emit({ type: "thinking-delta", text: cleanThought });
              }
            } else if (textPart) {
              if (thinkingStarted) {
                thinkingStarted = false;
                output.emit({ type: "thinking-end" });
              }
              const cleanText = textPart.replace(/<\/?think>/gi, "");
              if (cleanText) {
                hasEmittedAnyChunk = true;
                if (!textStarted) {
                  textStarted = true;
                  output.emit({ type: "text-start" });
                }
                output.emit({ type: "text-delta", text: cleanText });
              }
            }

            const fnCall = part.functionCall as { name: string; args: unknown } | undefined;
            if (fnCall) {
              hasEmittedAnyChunk = true;
              if (thinkingStarted) {
                thinkingStarted = false;
                output.emit({ type: "thinking-end" });
              }
              if (textStarted) {
                textStarted = false;
                output.emit({ type: "text-end" });
              }
              hasTools = true;
              const currentIdx = toolIndex++;
              const callId = `call_${Date.now()}_${currentIdx}`;
              output.emit({
                type: "tool-call-start",
                index: currentIdx,
                callId,
                name: fnCall.name,
              });
              const argsStr = typeof fnCall.args === "string"
                ? fnCall.args
                : JSON.stringify(fnCall.args || {});
              output.emit({
                type: "tool-call-arguments-delta",
                index: currentIdx,
                delta: argsStr,
              });
              output.emit({
                type: "tool-call-end",
                index: currentIdx,
              });
            }
          }
        }

        const finishReason = typeof candidate?.finishReason === "string"
          ? candidate.finishReason
          : null;
        if (finishReason) {
          if (thinkingStarted) {
            thinkingStarted = false;
            output.emit({ type: "thinking-end" });
          }
          if (textStarted) {
            textStarted = false;
            output.emit({ type: "text-end" });
          }
          if (lastThoughtSignature) {
            output.emit({
              type: "replay-state",
              providerKind: "antigravity",
              value: { thoughtSignature: lastThoughtSignature },
            });
            lastThoughtSignature = null;
          }
          if (finalUsage) {
            output.emit({
              type: "usage",
              usage: {
                inputTokens: finalUsage.inputTokens,
                outputTokens: finalUsage.outputTokens,
                totalTokens: finalUsage.totalTokens,
                cacheReadTokens: finalUsage.cacheReadTokens,
                cacheWriteTokens: null,
                reasoningTokens: null,
              },
            });
            finalUsage = null;
          }
          const isTool = finishReason === "STOP" &&
            (hasTools || parts?.some((p) => p.functionCall));
          output.emit({
            type: "done",
            reason: isTool ? "tool-use" : "stop",
          });
          doneEmitted = true;
          break;
        }
      }

      if (thinkingStarted) {
        output.emit({ type: "thinking-end" });
      }
      if (textStarted) {
        output.emit({ type: "text-end" });
      }
      if (lastThoughtSignature) {
        output.emit({
          type: "replay-state",
          providerKind: "antigravity",
          value: { thoughtSignature: lastThoughtSignature },
        });
      }
      if (finalUsage) {
        output.emit({
          type: "usage",
          usage: {
            inputTokens: finalUsage.inputTokens,
            outputTokens: finalUsage.outputTokens,
            totalTokens: finalUsage.totalTokens,
            cacheReadTokens: finalUsage.cacheReadTokens,
            cacheWriteTokens: null,
            reasoningTokens: null,
          },
        });
      }
      if (!doneEmitted) {
        output.emit({
          type: "done",
          reason: hasTools ? "tool-use" : "stop",
        });
      }
      return;
    } catch (err) {
      lastError = err instanceof Error ? err : new Error(String(err));
      if (hasEmittedAnyChunk) {
        throw lastError;
      }
      // 授权失败与端点无关:换端点重试只会把同一个 401 再发两遍。
      // 交给上层刷新凭证后重试一次。
      if (lastError instanceof HttpError && lastError.status === 401) {
        throw lastError;
      }
    }
  }

  if (lastError) throw lastError;
}

async function invoke(
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<ProviderResult> {
  const resource = input.resource;
  if (!resource) {
    return {
      status: "request-error",
      message: "Add a Google Antigravity account before calling Antigravity",
    };
  }
  let data: AccountData;
  try {
    data = accountData(resource);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return invalidResult(message, message);
  }

  let patch: ResourcePatch | null = null;

  // Auto-refresh token if expired or close to expiration (skew 5 mins).
  // 失效结果必须原样上报:没有 privateData 的 patch 不意味着可以继续用旧 token。
  if (data.refreshToken && isTokenExpired(data)) {
    try {
      patch = await refreshAccount(resource, context);
    } catch (error) {
      return {
        status: "request-error",
        message: `Antigravity token refresh failed: ${errorMessage(error)}`,
      };
    }
    if (patch.state?.status === "invalid") {
      return {
        status: "resource-error",
        message: patch.state.message ?? AUTH_EXPIRED_MESSAGE,
        patch,
      };
    }
    if (!patch.privateData) {
      return {
        status: "request-error",
        message: "Antigravity token refresh returned no credential",
      };
    }
    data = patch.privateData as unknown as AccountData;
  }

  const projectId = data.projectId ?? "bamboo-precept-lgxtn";

  try {
    await streamCloudCode(data.accessToken, projectId, input.model.id, input, output, context);
    return patch === null ? { status: "completed" } : { status: "completed", patch };
  } catch (error) {
    if (
      error instanceof HttpError && (error.status === 401 || error.status === 403) &&
      !isQuotaHttpError(error)
    ) {
      return await recoverAuthorization(error, resource, data, patch, input, output, context);
    }
    return failure(error, data);
  }
}

/**
 * 授权被拒后的恢复:刷新一次并重试一次。刷新判定 invalid 时把资源标为 invalid;
 * 刷新只是暂时失败时中止本次请求,绝不拿旧 token 再发一次。
 * 401/403 在读到响应头时抛出,首轮尚未发出任何事件,重试不会重复输出。
 */
async function recoverAuthorization(
  authError: HttpError,
  resource: ResourceSnapshot,
  data: AccountData,
  patch: ResourcePatch | null,
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<ProviderResult> {
  // 本次请求已经主动刷新过:刷新后的令牌仍被拒,授权确实失效,不再刷新第二次。
  // 401 是授权失败;403 还可能是权限/模型限制,不能据此停用整个账号。
  if (patch !== null) {
    return authError.status === 401
      ? invalidResult(authError.message, AUTH_EXPIRED_MESSAGE, patch.privateData ?? null)
      : { status: "request-error", message: authError.message };
  }
  if (!data.refreshToken) {
    // 没有刷新令牌时,401 只能重新登录;403 仍按请求错误上报。
    return authError.status === 401
      ? invalidResult(authError.message, AUTH_EXPIRED_MESSAGE)
      : { status: "request-error", message: authError.message };
  }
  let refreshed: ResourcePatch;
  try {
    refreshed = await refreshAccount(resource, context);
  } catch (error) {
    return {
      status: "request-error",
      message: `${authError.message}; token refresh failed: ${errorMessage(error)}`,
    };
  }
  if (refreshed.state?.status === "invalid") {
    return {
      status: "resource-error",
      message: refreshed.state.message ?? AUTH_EXPIRED_MESSAGE,
      patch: refreshed,
    };
  }
  if (!refreshed.privateData) {
    return {
      status: "request-error",
      message: `${authError.message}; token refresh returned no credential`,
    };
  }
  const fresh = refreshed.privateData as unknown as AccountData;
  try {
    await streamCloudCode(
      fresh.accessToken,
      fresh.projectId ?? "bamboo-precept-lgxtn",
      input.model.id,
      input,
      output,
      context,
    );
    return { status: "completed", patch: refreshed };
  } catch (retryError) {
    return failure(retryError, fresh);
  }
}

export const antigravityProvider: ProviderSupport = {
  id: "antigravity",
  displayName: {
    "en-US": "Google Antigravity",
    "zh-CN": "Google Antigravity",
  },
  description: {
    "en-US": "Google Antigravity / Gemini model access with hybrid reasoning & agent tools.",
    "zh-CN": "通过 Google Antigravity / Gemini API 使用混合推理与 Agent 工具。",
  },
  providerType: "google",
  resourceType: RESOURCE_TYPE,
  models: antigravityModels,
  invoke,
};
