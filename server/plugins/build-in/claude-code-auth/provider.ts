import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import type {
  LlmContentPart,
  LlmMessage,
  LlmRequest,
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { HttpError } from "cursor-byok:protocol/openai-chat";
import { claudeCodeModels } from "./models.ts";
import { redactSecrets, truncate } from "./redact.ts";
import {
  type AccountData,
  accountData,
  AUTH_INVALID_MESSAGE,
  authHeaders,
  CLAUDE_API_URL,
  isTokenExpired,
  refreshTokens,
  RESOURCE_TYPE,
} from "./resources.ts";
import {
  cooldownUntil,
  jitteredBackoffMs,
  MAX_SERVER_ERROR_RETRIES,
  noteRateLimit,
  noteRefreshFailure,
  noteSuccess,
  parseRetryAfterMs,
  SERVER_RETRY_BASE_MS,
  singleFlight,
  withAccountSlot,
} from "./runtime.ts";

/**
 * 回放状态里存的是带签名的 thinking / redacted_thinking 块。Anthropic 要求
 * 多轮工具调用时原样回传这些块，否则会 400；文本内容不入回放状态。
 */
export const REPLAY_KIND = "claude-code";

/** 模型目录没有声明输出上限时的保守值；避免把宿主的超大值原样透传。 */
export const DEFAULT_MAX_OUTPUT_TOKENS = 8_192;

const EFFORT_VALUES = ["low", "medium", "high", "xhigh", "max"] as const;

type JsonObject = Record<string, unknown>;

type WireMessage = {
  role: "user" | "assistant";
  content: JsonObject[];
};

function object(value: unknown): JsonObject | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as JsonObject
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

/** 流式文本不能 trim：SSE 的空白片段也是内容的一部分。 */
function rawString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function number(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string" && value.trim()) {
    const parsed = Number(value);
    return Number.isFinite(parsed) ? parsed : null;
  }
  return null;
}

function toWirePart(part: LlmContentPart): JsonObject {
  if (part.type === "image") {
    return {
      type: "image",
      source: {
        type: "base64",
        media_type: part.mediaType,
        data: part.dataBase64,
      },
    };
  }
  return { type: "text", text: part.text };
}

function wireParts(parts: LlmContentPart[]): JsonObject[] {
  return parts.map(toWirePart);
}

function pushMessage(wire: WireMessage[], role: "user" | "assistant", content: JsonObject[]): void {
  if (content.length === 0) return;
  const last = wire[wire.length - 1];
  if (last && last.role === role) {
    last.content.push(...content);
    return;
  }
  wire.push({ role, content });
}

function replayBlocks(message: Extract<LlmMessage, { role: "assistant" }>): JsonObject[] {
  if (message.replayState?.providerKind !== REPLAY_KIND) return [];
  const value = object(message.replayState.value);
  const blocks = value?.blocks;
  return Array.isArray(blocks)
    ? blocks.flatMap((block) => {
      const parsed = object(block);
      return parsed ? [parsed] : [];
    })
    : [];
}

function assistantContent(message: Extract<LlmMessage, { role: "assistant" }>): JsonObject[] {
  const blocks: JsonObject[] = [];
  for (const block of replayBlocks(message)) {
    const type = text(block.type);
    if (type === "thinking" && rawString(block.signature)) {
      blocks.push({
        type: "thinking",
        thinking: rawString(block.thinking) ?? "",
        signature: block.signature,
      });
    } else if (type === "redacted_thinking" && rawString(block.data)) {
      blocks.push({ type: "redacted_thinking", data: block.data });
    }
  }
  if (message.text) blocks.push({ type: "text", text: message.text });
  for (const call of message.toolCalls) {
    blocks.push({
      type: "tool_use",
      id: call.callId,
      name: call.name,
      input: object(call.arguments) ?? {},
    });
  }
  return blocks;
}

function toolResultContent(message: Extract<LlmMessage, { role: "tool" }>): JsonObject[] {
  if (message.parts.length > 0) return wireParts(message.parts);
  return [{ type: "text", text: message.content || " " }];
}

function buildMessages(request: LlmRequest): { system: string; messages: WireMessage[] } {
  const systemParts = [request.instructions.trim()];
  const messages: WireMessage[] = [];

  for (const message of request.messages) {
    if (message.role === "system") {
      const content = message.content
        .filter((part) => part.type === "text")
        .map((part) => part.text)
        .join("\n");
      if (content.trim()) systemParts.push(content.trim());
      continue;
    }
    if (message.role === "user") {
      const content = wireParts(message.content);
      pushMessage(messages, "user", content.length > 0 ? content : [{ type: "text", text: " " }]);
      continue;
    }
    if (message.role === "assistant") {
      const content = assistantContent(message);
      pushMessage(
        messages,
        "assistant",
        content.length > 0 ? content : [{ type: "text", text: " " }],
      );
      continue;
    }
    if (message.role === "tool") {
      pushMessage(messages, "user", [{
        type: "tool_result",
        tool_use_id: message.callId,
        content: toolResultContent(message),
        ...(message.isError ? { is_error: true } : {}),
      }]);
    }
  }

  return {
    system: systemParts.filter(Boolean).join("\n\n"),
    messages,
  };
}

function normalizeEffort(value: string | null): "low" | "medium" | "high" | "xhigh" | "max" | null {
  const lower = value?.toLowerCase();
  return EFFORT_VALUES.includes(lower as typeof EFFORT_VALUES[number])
    ? lower as typeof EFFORT_VALUES[number]
    : null;
}

/**
 * 末条 assistant 带 tool_use、却没有带签名的 thinking 块时，上游会拒绝继续开
 * thinking（工具续轮必须回放上一条助手消息的签名）。这类历史常来自其他模型，
 * 不能为了“支持 thinking”而让整个请求 400。
 */
function trailingTurnSupportsThinking(messages: WireMessage[]): boolean {
  const assistant = [...messages].reverse().find((message) => message.role === "assistant");
  if (!assistant) return true;
  if (!assistant.content.some((block) => block.type === "tool_use")) return true;
  return assistant.content.some(
    (block) => block.type === "thinking" || block.type === "redacted_thinking",
  );
}

/**
 * 只构造当前请求真正需要的字段：
 * - 不传 `metadata.user_id` 之类的身份字段；
 * - 不声明 1M 上下文、context_management 等未使用的 beta 能力；
 * - `max_tokens` 夹到模型目录声明的输出上限（未声明时 8192）。
 */
export function buildRequestBody(input: ProviderInvokeInput): JsonObject {
  const { system, messages } = buildMessages(input.request);
  const declaredMax = input.model.maxOutputTokens;
  const modelMax = declaredMax && declaredMax > 0 ? declaredMax : DEFAULT_MAX_OUTPUT_TOKENS;
  const requested = input.request.maxOutputTokens ?? modelMax;
  const maxTokens = Math.max(1, Math.floor(Math.min(requested, modelMax)));

  const tools = input.request.tools.map((tool) => ({
    name: tool.name,
    description: tool.description,
    input_schema: object(tool.parameters) ?? { type: "object", properties: {} },
  }));

  const body: JsonObject = {
    model: input.model.id,
    max_tokens: maxTokens,
    messages,
    stream: true,
  };
  if (system) body.system = system;
  if (tools.length > 0) body.tools = tools;

  if (input.request.reasoning.enabled && trailingTurnSupportsThinking(messages)) {
    body.thinking = { type: "adaptive", display: "summarized" };
    const effort = normalizeEffort(input.request.reasoning.effort);
    if (effort) body.output_config = { effort };
  }

  return body;
}

async function readBody(lines: AsyncIterable<string>): Promise<string> {
  const collected: string[] = [];
  for await (const line of lines) collected.push(line);
  return collected.join("\n");
}

function mergeUsage(
  usage: { input: number; output: number; cacheRead: number; cacheWrite: number },
  raw: JsonObject | null,
): boolean {
  if (!raw) return false;
  let seen = false;
  const input = number(raw.input_tokens);
  if (input !== null) {
    usage.input = Math.max(usage.input, input);
    seen = true;
  }
  const output = number(raw.output_tokens);
  if (output !== null) {
    usage.output = Math.max(usage.output, output);
    seen = true;
  }
  const cacheRead = number(raw.cache_read_input_tokens);
  if (cacheRead !== null) {
    usage.cacheRead = Math.max(usage.cacheRead, cacheRead);
    seen = true;
  }
  const cacheWrite = number(raw.cache_creation_input_tokens);
  if (cacheWrite !== null) {
    usage.cacheWrite = Math.max(usage.cacheWrite, cacheWrite);
    seen = true;
  }
  return seen;
}

function stopReason(reason: string | null, sawTool: boolean): "stop" | "length" | "tool-use" {
  switch (reason) {
    case "tool_use":
      return "tool-use";
    case "max_tokens":
    case "model_context_window_exceeded":
      return "length";
    case "end_turn":
    case "stop_sequence":
    case "pause_turn":
    case "refusal":
      return "stop";
    default:
      return sawTool ? "tool-use" : "stop";
  }
}

/**
 * 解析 Anthropic Messages SSE。`message_start` 给输入/缓存用量，`message_delta`
 * 给累计输出与 stop_reason，`message_stop` 收尾；thinking 的签名在
 * `signature_delta` 里，必须和 thinking 文本一起存入回放状态。
 */
async function streamMessages(
  accessToken: string,
  body: JsonObject,
  output: ProviderOutput,
  context: PluginContext,
): Promise<void> {
  const response = await context.network.stream(CLAUDE_API_URL, {
    method: "POST",
    headers: authHeaders(accessToken, "text/event-stream"),
    body: JSON.stringify(body),
  });
  if (response.status < 200 || response.status >= 300) {
    throw new HttpError(response.status, await readBody(response.lines), response.headers);
  }

  const kinds = new Map<number, string>();
  const thinkingText = new Map<number, string>();
  const thinkingSignatures = new Map<number, string>();
  const replay: JsonObject[] = [];
  const usage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
  let sawUsage = false;
  let sawTool = false;
  let finish: "stop" | "length" | "tool-use" | null = null;
  let terminal = false;

  const closeBlock = (index: number): void => {
    const kind = kinds.get(index);
    if (!kind) return;
    kinds.delete(index);
    if (kind === "text") {
      output.emit({ type: "text-end" });
      return;
    }
    if (kind === "thinking") {
      const signature = thinkingSignatures.get(index) ?? "";
      if (signature) {
        replay.push({
          type: "thinking",
          thinking: thinkingText.get(index) ?? "",
          signature,
        });
      }
      thinkingText.delete(index);
      thinkingSignatures.delete(index);
      output.emit({ type: "thinking-end" });
      return;
    }
    if (kind === "tool_use") output.emit({ type: "tool-call-end", index });
  };

  const closeAll = (): void => {
    for (const index of [...kinds.keys()].sort((left, right) => left - right)) {
      closeBlock(index);
    }
  };

  const finishStream = (): void => {
    closeAll();
    if (replay.length > 0) {
      output.emit({
        type: "replay-state",
        providerKind: REPLAY_KIND,
        value: { blocks: replay } as unknown as JsonValue,
      });
    }
    if (sawUsage) {
      output.emit({
        type: "usage",
        usage: {
          inputTokens: usage.input,
          outputTokens: usage.output,
          totalTokens: usage.input + usage.output + usage.cacheRead + usage.cacheWrite,
          cacheReadTokens: usage.cacheRead,
          cacheWriteTokens: usage.cacheWrite,
          reasoningTokens: null,
        },
      });
    }
    const reason = finish === "length" ? "length" : sawTool ? "tool-use" : finish ?? "stop";
    output.emit({ type: "done", reason });
  };

  const handle = (event: JsonObject): boolean => {
    const type = text(event.type) ?? "";
    if (type === "error") {
      const error = object(event.error);
      throw new Error(text(error?.message) ?? "Claude stream returned an error");
    }
    if (type === "message_start") {
      const message = object(event.message);
      sawUsage = mergeUsage(usage, object(message?.usage)) || sawUsage;
      return false;
    }
    if (type === "content_block_start") {
      const index = number(event.index);
      const block = object(event.content_block);
      const kind = text(block?.type);
      if (index === null || !block || !kind) return false;
      if (kind === "text") {
        kinds.set(index, "text");
        output.emit({ type: "text-start" });
      } else if (kind === "thinking") {
        kinds.set(index, "thinking");
        thinkingText.set(index, "");
        thinkingSignatures.set(index, "");
        output.emit({ type: "thinking-start" });
      } else if (kind === "redacted_thinking") {
        kinds.set(index, "redacted_thinking");
        const data = rawString(block.data);
        if (data) replay.push({ type: "redacted_thinking", data });
      } else if (kind === "tool_use") {
        const callId = text(block.id);
        const name = text(block.name);
        if (!callId || !name) return false;
        kinds.set(index, "tool_use");
        sawTool = true;
        output.emit({ type: "tool-call-start", index, callId, name });
      }
      return false;
    }
    if (type === "content_block_delta") {
      const index = number(event.index);
      const delta = object(event.delta);
      const kind = text(delta?.type);
      if (index === null || !delta || !kind) return false;
      if (kind === "text_delta") {
        if (!kinds.has(index)) {
          kinds.set(index, "text");
          output.emit({ type: "text-start" });
        }
        const value = rawString(delta.text);
        if (value !== null) output.emit({ type: "text-delta", text: value });
      } else if (kind === "thinking_delta") {
        if (!kinds.has(index)) {
          kinds.set(index, "thinking");
          thinkingText.set(index, "");
          thinkingSignatures.set(index, "");
          output.emit({ type: "thinking-start" });
        }
        const value = rawString(delta.thinking);
        if (value !== null) {
          thinkingText.set(index, (thinkingText.get(index) ?? "") + value);
          output.emit({ type: "thinking-delta", text: value });
        }
      } else if (kind === "signature_delta") {
        const value = rawString(delta.signature);
        if (value !== null) {
          thinkingSignatures.set(index, (thinkingSignatures.get(index) ?? "") + value);
        }
      } else if (kind === "input_json_delta") {
        const value = rawString(delta.partial_json);
        if (value !== null) {
          output.emit({ type: "tool-call-arguments-delta", index, delta: value });
        }
      }
      return false;
    }
    if (type === "content_block_stop") {
      const index = number(event.index);
      if (index !== null) closeBlock(index);
      return false;
    }
    if (type === "message_delta") {
      sawUsage = mergeUsage(usage, object(event.usage)) || sawUsage;
      const delta = object(event.delta);
      finish = stopReason(text(delta?.stop_reason), sawTool);
      return false;
    }
    if (type === "message_stop") {
      finishStream();
      terminal = true;
      return true;
    }
    return false;
  };

  for await (const line of response.lines) {
    if (!line.startsWith("data:")) continue;
    const raw = line.slice(5).trim();
    if (!raw || raw === "[DONE]") continue;
    let event: JsonObject;
    try {
      event = object(JSON.parse(raw)) ?? {};
    } catch {
      continue;
    }
    if (handle(event)) break;
  }

  if (!terminal) {
    // 上游偶尔在 message_delta 之后断开而没有 message_stop；只要拿到 stop_reason，
    // 已产出的内容仍应完整交付。两者都没有时才当作失败，避免静默截断。
    if (finish !== null) {
      finishStream();
    } else {
      throw new Error("Claude stream ended without message_stop");
    }
  }
}

function secretsOf(...accounts: Array<AccountData | null>): string[] {
  return accounts
    .filter((account): account is AccountData => account !== null)
    .flatMap((account) => [
      account.accessToken,
      ...(account.refreshToken ? [account.refreshToken] : []),
    ]);
}

function safeMessage(message: string, accounts: Array<AccountData | null>): string {
  return truncate(redactSecrets(message, secretsOf(...accounts)));
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function explain(error: HttpError, accounts: Array<AccountData | null>): string {
  const base = safeMessage(error.message, accounts);
  if (error.body.includes("third-party") || error.body.includes("extra usage")) {
    return `${base}\nAnthropic may reject subscription OAuth from third-party clients; an API key is the supported path.`;
  }
  return base;
}

type RefreshOutcome = {
  data: AccountData | null;
  result: ProviderResult | null;
};

function coolingResult(
  message: string,
  retryAtMs: number,
  data: AccountData | null,
): ProviderResult {
  const detail = truncate(message);
  return {
    status: "resource-error",
    message: detail,
    patch: {
      ...(data ? { privateData: data as unknown as JsonValue } : {}),
      state: {
        status: "cooling",
        retryAtMs,
        message: detail,
      },
    },
  };
}

function invalidResult(message: string, data: AccountData | null = null): ProviderResult {
  return {
    status: "resource-error",
    message: truncate(message),
    patch: {
      ...(data ? { privateData: data as unknown as JsonValue } : {}),
      state: { status: "invalid", message: AUTH_INVALID_MESSAGE },
    },
  };
}

function completedResult(data: AccountData | null): ProviderResult {
  return data
    ? {
      status: "completed",
      patch: { privateData: data as unknown as JsonValue, state: { status: "ready" } },
    }
    : { status: "completed" };
}

function refreshFailureResult(
  error: unknown,
  accountKey: string,
  data: AccountData,
): ProviderResult {
  const message = safeMessage(
    error instanceof Error ? error.message : String(error),
    [data],
  );
  const kind = (error as { kind?: string }).kind;
  if (kind === "invalid") {
    return invalidResult(message);
  }
  const until = kind === "rate-limit"
    ? noteRateLimit(accountKey, (error as { retryAfterMs?: number | null }).retryAfterMs ?? null)
    : noteRefreshFailure(accountKey);
  return coolingResult(message, until, null);
}

async function refreshForAccount(
  data: AccountData,
  accountKey: string,
  context: PluginContext,
): Promise<RefreshOutcome> {
  if (!data.refreshToken) {
    return { data: null, result: invalidResult("Claude account has no refresh token") };
  }
  try {
    const tokens = await singleFlight(
      `claude-refresh:${accountKey}`,
      () => refreshTokens(data.refreshToken!, context),
    );
    return {
      data: {
        ...data,
        accessToken: tokens.accessToken,
        refreshToken: tokens.refreshToken,
        expiresAtMs: tokens.expiresAtMs,
      },
      result: null,
    };
  } catch (error) {
    return { data: null, result: refreshFailureResult(error, accountKey, data) };
  }
}

function failure(
  error: unknown,
  accountKey: string,
  data: AccountData,
  refreshed: AccountData | null,
): ProviderResult {
  const persisted = refreshed ?? data;
  if (error instanceof HttpError) {
    if (error.status === 429 || error.status === 529) {
      const now = Date.now();
      const updated: AccountData = { ...persisted, lastRateLimitAtMs: now };
      const until = noteRateLimit(accountKey, parseRetryAfterMs(error.headers), now);
      const message = safeMessage(error.message, [data, refreshed]);
      return {
        status: "resource-error",
        message,
        patch: {
          privateData: updated as unknown as JsonValue,
          state: {
            status: "cooling",
            retryAtMs: until,
            message: truncate("Claude rate limit reached; cooling down"),
          },
        },
      };
    }
    return {
      status: "request-error",
      message: explain(error, [data, refreshed]),
      ...(refreshed ? { patch: { privateData: refreshed as unknown as JsonValue } } : {}),
    };
  }
  return {
    status: "request-error",
    message: safeMessage(error instanceof Error ? error.message : String(error), [data, refreshed]),
    ...(refreshed ? { patch: { privateData: refreshed as unknown as JsonValue } } : {}),
  };
}

/** 冷却状态既可能来自持久记录（重启后），也可能来自本进程刚记录的 429。 */
function blockedUntil(resourceState: ResourceSnapshot["state"], accountKey: string): number {
  const persisted = resourceState.status === "cooling" ? resourceState.retryAtMs ?? 0 : 0;
  return Math.max(persisted, cooldownUntil(accountKey) ?? 0);
}

async function execute(
  initial: AccountData,
  body: JsonObject,
  output: ProviderOutput,
  context: PluginContext,
  accountKey: string,
): Promise<ProviderResult> {
  let data = initial;
  let refreshed: AccountData | null = null;
  let emitted = false;
  let authRetried = false;
  let serverRetries = 0;

  const tracked: ProviderOutput = {
    emit: (event) => {
      emitted = true;
      output.emit(event);
    },
  };

  if (isTokenExpired(data)) {
    const outcome = await refreshForAccount(data, accountKey, context);
    if (!outcome.data) return outcome.result!;
    data = outcome.data;
    refreshed = data;
  }

  for (;;) {
    try {
      await streamMessages(data.accessToken, body, tracked, context);
      noteSuccess(accountKey);
      return completedResult(refreshed);
    } catch (error) {
      if (error instanceof HttpError) {
        // 401 只刷新一次、只重试一次；如果一个请求里已经刷新过（主动刷新或上一次 401），
        // 再收到 401 说明刷新后的令牌仍被拒绝，直接标 invalid，不再循环刷新。
        if (error.status === 401 && !emitted) {
          if (authRetried || refreshed !== null) {
            return invalidResult(
              "Claude rejected the refreshed authorization; sign in again",
              refreshed,
            );
          }
          authRetried = true;
          const outcome = await refreshForAccount(data, accountKey, context);
          if (!outcome.data) return outcome.result!;
          data = outcome.data;
          refreshed = data;
          continue;
        }
        // 5xx 有限重试；已经向用户交付内容后不再重试，避免重复文本。
        if (error.status >= 500 && !emitted && serverRetries < MAX_SERVER_ERROR_RETRIES) {
          await sleep(jitteredBackoffMs(serverRetries, SERVER_RETRY_BASE_MS));
          serverRetries += 1;
          continue;
        }
      }
      return failure(error, accountKey, data, refreshed);
    }
  }
}

export async function invoke(
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<ProviderResult> {
  if (!input.resource) {
    return {
      status: "request-error",
      message: "Add a Claude subscription account before calling Claude",
    };
  }

  const accountKey = input.resource.id;
  let data: AccountData;
  try {
    data = accountData(input.resource);
  } catch (error) {
    return invalidResult(error instanceof Error ? error.message : String(error));
  }

  const waitingUntil = blockedUntil(input.resource.state, accountKey);
  if (waitingUntil > Date.now()) {
    return coolingResult(
      "Claude account is cooling down; retry after the displayed time",
      waitingUntil,
      null,
    );
  }

  let body: JsonObject;
  try {
    body = buildRequestBody(input);
  } catch (error) {
    return {
      status: "request-error",
      message: error instanceof Error ? error.message : String(error),
    };
  }

  try {
    return await withAccountSlot(accountKey, context.signal, async () => {
      // 排队期间另一个请求可能已经触发 429；拿到槽位后再检查一次，避免冷却期继续发请求。
      const blocked = blockedUntil({ status: "ready" }, accountKey);
      if (blocked > Date.now()) {
        return coolingResult(
          "Claude account is cooling down; retry after the displayed time",
          blocked,
          null,
        );
      }
      return await execute(data, body, output, context, accountKey);
    });
  } catch (error) {
    if (error instanceof Error && error.name === "AbortError") {
      return { status: "request-error", message: error.message };
    }
    return failure(error, accountKey, data, null);
  }
}

export const claudeCodeProvider: ProviderSupport = {
  id: "claude-code",
  displayName: {
    "en-US": "Claude (subscription)",
    "zh-CN": "Claude（订阅）",
    "ru-RU": "Claude (подписка)",
  },
  description: {
    "en-US":
      "Claude Pro/Max subscription over OAuth. May violate Anthropic's terms; use at your own risk.",
    "zh-CN": "通过 OAuth 使用 Claude Pro/Max 订阅。可能违反 Anthropic 服务条款，风险自负。",
    "ru-RU":
      "Подписка Claude Pro/Max через OAuth. Может нарушать условия Anthropic; используйте на свой риск.",
  },
  providerType: "anthropic",
  resourceType: RESOURCE_TYPE,
  models: claudeCodeModels,
  invoke,
};
