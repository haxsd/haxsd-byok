import type {
  JsonValue,
  NetworkEventStream,
  NetworkResponse,
  PluginContext,
} from "cursor-byok:plugin";
import type { LlmRequest, ModelEvent } from "cursor-byok:provider";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { antigravityProvider, isQuotaError, outputTokenLimit } from "./provider.ts";
import { RESOURCE_TYPE } from "./resources.ts";

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

type RequestInit = { body?: string; headers?: Record<string, string> };
type FetchHandler = (url: string, init?: RequestInit) => NetworkResponse;
type StreamHandler = (url: string, init?: RequestInit) => NetworkEventStream;

function context(handlers: { fetch?: FetchHandler; stream?: StreamHandler }): PluginContext {
  return {
    network: {
      fetch: (url, init) => {
        if (!handlers.fetch) throw new Error("fetch was not expected");
        return Promise.resolve(handlers.fetch(url, init));
      },
      stream: (url, init) => {
        if (!handlers.stream) throw new Error("stream was not expected");
        return Promise.resolve(handlers.stream(url, init));
      },
    },
    signal: new AbortController().signal,
  };
}

function snapshot(privateData: JsonValue): ResourceSnapshot {
  return {
    id: "resource-1",
    type: RESOURCE_TYPE,
    key: "antigravity:user-1",
    privateData,
    state: { status: "ready" },
  };
}

async function* sse(lines: string[]): AsyncGenerator<string> {
  for (const line of lines) yield line;
}

function request(): LlmRequest {
  return {
    instructions: "You are an AI coding assistant.",
    messages: [{ role: "user", content: [{ type: "text", text: "Hello" }] }],
    tools: [],
    reasoning: { enabled: true, effort: "medium" },
    latency: "standard",
    maxOutputTokens: 65536,
    cacheKey: "conv-1",
  };
}

Deno.test("provider parses usage metadata including cachedContentTokenCount as cacheReadTokens", async () => {
  let requestBody = "";
  let requestHeaders: Record<string, string> = {};
  const events: ModelEvent[] = [];

  const result = await antigravityProvider.invoke(
    {
      model: {
        id: "gemini-3.7-flash-medium",
        displayName: "Gemini 3.7 Flash Medium",
        privateData: {},
      },
      resource: snapshot({
        accessToken: "mock-access-token",
        refreshToken: "mock-refresh-token",
        expiresAt: Date.now() + 3600_000,
        projectId: "test-project-123",
      }),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      stream: (url, init) => {
        assert(url.includes("/v1internal:streamGenerateContent?alt=sse"));
        requestBody = init?.body ?? "";
        requestHeaders = init?.headers ?? {};
        return {
          status: 200,
          headers: {},
          lines: sse([
            'data: {"response":{"candidates":[{"content":{"parts":[{"thought":true,"text":"Thinking..."}]}}]}}',
            'data: {"response":{"candidates":[{"content":{"parts":[{"text":"Hello world"}]}}]}}',
            'data: {"response":{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":120,"candidatesTokenCount":15,"totalTokenCount":135,"cachedContentTokenCount":80}}}',
          ]),
        };
      },
    }),
  );

  assertEquals(result, { status: "completed" });
  assertEquals(requestHeaders["authorization"], "Bearer mock-access-token");
  assertEquals(requestHeaders.accept, "text/event-stream");
  assert(!("session-id" in requestHeaders), "Codex-only session-id header was sent");
  assert(!("thread-id" in requestHeaders), "Codex-only thread-id header was sent");
  assert(!("x-client-request-id" in requestHeaders), "request-id header was reused as cache key");
  const body = JSON.parse(requestBody) as Record<string, unknown>;
  assertEquals(body.project, "test-project-123");
  assertEquals(body.model, "gemini-3.7-flash-medium");
  assert(!("sessionId" in body), "unsupported sessionId field was sent");
  assert(!("conversationId" in body), "unsupported conversationId field was sent");

  assertEquals(events, [
    { type: "thinking-start" },
    { type: "thinking-delta", text: "Thinking..." },
    { type: "thinking-end" },
    { type: "text-start" },
    { type: "text-delta", text: "Hello world" },
    { type: "text-end" },
    {
      type: "usage",
      usage: {
        inputTokens: 120,
        outputTokens: 15,
        totalTokens: 135,
        cacheReadTokens: 80,
        cacheWriteTokens: null,
        reasoningTokens: null,
      },
    },
    { type: "done", reason: "stop" },
  ]);
});

Deno.test("isQuotaError identifies rate limits and quota exhaustion", () => {
  assert(isQuotaError("RESOURCE_EXHAUSTED: quota exceeded"));
  assert(isQuotaError("Rate limit exceeded for model"));
  assert(isQuotaError("HTTP 429 Too Many Requests"));
  assert(!isQuotaError("Invalid authorization header"));
});

Deno.test("output tokens come from the request and are clamped to the catalog limit", () => {
  assertEquals(outputTokenLimit(65536, 64000), 64000);
  assertEquals(outputTokenLimit(2048, 64000), 2048);
  assertEquals(outputTokenLimit(65536, null), 65536);
  assertEquals(outputTokenLimit(null, 64000), 64000);
  assertEquals(outputTokenLimit(null, null), 65536);
});

Deno.test("the request body carries the clamped output limit", async () => {
  let requestBody = "";
  const result = await antigravityProvider.invoke(
    {
      model: {
        id: "gemini-3.7-flash-medium",
        displayName: "Gemini 3.7 Flash Medium",
        maxOutputTokens: 64000,
        privateData: {},
      },
      resource: snapshot({
        accessToken: "mock-access-token",
        refreshToken: null,
        expiresAtMs: Date.now() + 3600_000,
        projectId: "test-project-123",
        displayName: "Fixture",
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: (_url, init) => {
        requestBody = init?.body ?? "";
        return {
          status: 200,
          headers: {},
          lines: sse(['data: {"response":{"candidates":[{"finishReason":"STOP"}]}}']),
        };
      },
    }),
  );
  assertEquals(result.status, "completed");
  const body = JSON.parse(requestBody) as {
    request: { generationConfig: { maxOutputTokens: number } };
  };
  assertEquals(body.request.generationConfig.maxOutputTokens, 64000);
});

Deno.test("a revoked refresh token marks the resource invalid instead of staying ready", async () => {
  let streams = 0;
  const events: ModelEvent[] = [];
  const result = await antigravityProvider.invoke(
    {
      model: { id: "gemini-2.5-flash", displayName: "Gemini 2.5 Flash" },
      resource: snapshot({
        accessToken: "mock-access-token",
        refreshToken: "revoked-refresh",
        displayName: "Fixture",
        projectId: "test-project-123",
        quota: null,
      }),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      fetch: () => ({
        status: 400,
        headers: {},
        body: JSON.stringify({
          error: "invalid_grant",
          error_description: "Token has been revoked.",
        }),
      }),
      stream: () => {
        streams += 1;
        return { status: 401, headers: {}, lines: sse(['{"error":"unauthorized"}']) };
      },
    }),
  );
  assertEquals(streams, 1);
  assertEquals(events, []);
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assertEquals(result.patch.state?.status, "invalid");
  assertEquals(result.patch.privateData, undefined);
});

Deno.test("a rejected access token is refreshed once and the request retried once", async () => {
  const refreshedToken = "refreshed-access-token";
  let streams = 0;
  let tokenRefreshes = 0;
  const result = await antigravityProvider.invoke(
    {
      model: { id: "gemini-2.5-flash", displayName: "Gemini 2.5 Flash" },
      resource: snapshot({
        accessToken: "stale-access-token",
        refreshToken: "refresh-secret",
        displayName: "Fixture",
        projectId: "test-project-123",
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: (url) => {
        if (url === "https://oauth2.googleapis.com/token") {
          tokenRefreshes += 1;
          return {
            status: 200,
            headers: {},
            body: JSON.stringify({ access_token: refreshedToken, expires_in: 3600 }),
          };
        }
        // 额度查询的失败被 refreshAccount 自己吞掉,不影响刷新结果。
        return { status: 500, headers: {}, body: "unavailable" };
      },
      stream: (_url, init) => {
        streams += 1;
        if (streams === 1) {
          return { status: 401, headers: {}, lines: sse(['{"error":"unauthorized"}']) };
        }
        assertEquals(init?.headers?.authorization, `Bearer ${refreshedToken}`);
        return {
          status: 200,
          headers: {},
          lines: sse(['data: {"response":{"candidates":[{"finishReason":"STOP"}]}}']),
        };
      },
    }),
  );
  assertEquals(streams, 2);
  assertEquals(tokenRefreshes, 1);
  assert(result.status === "completed", `expected completed, received ${result.status}`);
  assertEquals(
    (result.patch?.privateData as Record<string, unknown>).accessToken,
    refreshedToken,
  );
});

Deno.test("a transient refresh failure aborts instead of reusing the stale token", async () => {
  let streams = 0;
  const result = await antigravityProvider.invoke(
    {
      model: { id: "gemini-2.5-flash", displayName: "Gemini 2.5 Flash" },
      resource: snapshot({
        accessToken: "stale-access-token",
        refreshToken: "refresh-secret",
        displayName: "Fixture",
        projectId: "test-project-123",
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: () => ({ status: 503, headers: {}, body: "token service unavailable" }),
      stream: () => {
        streams += 1;
        return { status: 401, headers: {}, lines: sse(['{"error":"unauthorized"}']) };
      },
    }),
  );
  assertEquals(streams, 1);
  assert(result.status === "request-error", `expected request-error, received ${result.status}`);
});

Deno.test("an expired credential refreshes before the first request", async () => {
  const refreshedToken = "preflight-access-token";
  let streams = 0;
  const result = await antigravityProvider.invoke(
    {
      model: { id: "gemini-2.5-flash", displayName: "Gemini 2.5 Flash" },
      resource: snapshot({
        accessToken: "stale-access-token",
        refreshToken: "refresh-secret",
        displayName: "Fixture",
        projectId: "test-project-123",
        expiresAtMs: Date.now() - 1000,
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: (url) => {
        if (url === "https://oauth2.googleapis.com/token") {
          return {
            status: 200,
            headers: {},
            body: JSON.stringify({ access_token: refreshedToken, expires_in: 3600 }),
          };
        }
        return { status: 500, headers: {}, body: "unavailable" };
      },
      stream: (_url, init) => {
        streams += 1;
        assertEquals(init?.headers?.authorization, `Bearer ${refreshedToken}`);
        return {
          status: 200,
          headers: {},
          lines: sse(['data: {"response":{"candidates":[{"finishReason":"STOP"}]}}']),
        };
      },
    }),
  );
  assertEquals(streams, 1);
  assert(result.status === "completed", `expected completed, received ${result.status}`);
  assertEquals(
    (result.patch?.privateData as Record<string, unknown>).accessToken,
    refreshedToken,
  );
});

Deno.test("an expired credential with a failing refresh is never called", async () => {
  let streams = 0;
  const result = await antigravityProvider.invoke(
    {
      model: { id: "gemini-2.5-flash", displayName: "Gemini 2.5 Flash" },
      resource: snapshot({
        accessToken: "stale-access-token",
        refreshToken: "refresh-secret",
        displayName: "Fixture",
        projectId: "test-project-123",
        expiresAtMs: Date.now() - 1000,
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: () => ({ status: 503, headers: {}, body: "token service unavailable" }),
      stream: () => {
        streams += 1;
        return { status: 200, headers: {}, lines: sse([]) };
      },
    }),
  );
  assertEquals(streams, 0);
  assert(result.status === "request-error", `expected request-error, received ${result.status}`);
});
