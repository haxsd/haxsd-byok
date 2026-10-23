import type {
  JsonValue,
  NetworkEventStream,
  NetworkResponse,
  PluginContext,
} from "cursor-byok:plugin";
import type { LlmRequest, ProviderResult } from "cursor-byok:provider";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { antigravityProvider } from "./provider.ts";
import { AUTH_EXPIRED_MESSAGE, resetRefreshStateForTests, RESOURCE_TYPE } from "./resources.ts";

const TOKEN_URL = "https://oauth2.googleapis.com/token";

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

type RequestInit = { body?: string; headers?: Record<string, string> };
type FetchHandler = (url: string, init?: RequestInit) => NetworkResponse | Promise<NetworkResponse>;
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
    maxOutputTokens: 65_536,
    cacheKey: "conv-1",
  };
}

function account(accessToken: string): JsonValue {
  return {
    accessToken,
    refreshToken: "refresh-old",
    displayName: "Fixture",
    projectId: "test-project-123",
    // 已过期:请求发出前先走主动刷新。
    expiresAtMs: Date.now() - 1000,
    quota: null,
  };
}

function invoke(token: string, handlers: { fetch?: FetchHandler; stream?: StreamHandler }) {
  return antigravityProvider.invoke(
    {
      model: { id: "gemini-2.5-flash", displayName: "Gemini 2.5 Flash" },
      resource: snapshot(account(token)),
      request: request(),
    },
    { emit: () => {} },
    context(handlers),
  );
}

function completedStream(): NetworkEventStream {
  return {
    status: 200,
    headers: {},
    lines: sse(['data: {"response":{"candidates":[{"finishReason":"STOP"}]}}']),
  };
}

function refreshResponse(accessToken: string): NetworkResponse {
  return {
    status: 200,
    headers: {},
    body: JSON.stringify({
      access_token: accessToken,
      refresh_token: "refresh-new",
      expires_in: 3600,
    }),
  };
}

function privateData(result: ProviderResult): Record<string, unknown> {
  assert(result.patch, "the result must carry a resource patch");
  return result.patch.privateData as Record<string, unknown>;
}

Deno.test("a 401 after the preflight refresh invalidates without refreshing twice", async () => {
  resetRefreshStateForTests();
  let refreshes = 0;
  let streams = 0;
  const result = await invoke("stale-access-token", {
    fetch: (url) => {
      if (url === TOKEN_URL) {
        refreshes += 1;
        return refreshResponse("fresh-access-token");
      }
      return { status: 500, headers: {}, body: "quota unavailable" };
    },
    stream: () => {
      streams += 1;
      return { status: 401, headers: {}, lines: sse(['{"error":"unauthorized"}']) };
    },
  });
  assertEquals(refreshes, 1);
  assertEquals(streams, 1);
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assertEquals(result.patch.state, { status: "invalid", message: AUTH_EXPIRED_MESSAGE });
  // 主动刷新已经换到新 refresh token,即使随后 401 也必须写回,避免下一轮重复消费旧值。
  assertEquals(privateData(result).accessToken, "fresh-access-token");
  assertEquals(privateData(result).refreshToken, "refresh-new");
});

Deno.test("two concurrent requests share one preflight refresh and persist the rotation", async () => {
  resetRefreshStateForTests();
  let refreshes = 0;
  const authorizations: string[] = [];
  const handlers = {
    fetch: async (url: string) => {
      if (url === TOKEN_URL) {
        refreshes += 1;
        // 让两个请求都先进入刷新等待,证明它们共享同一个 Promise。
        await new Promise((resolve) => setTimeout(resolve, 10));
        return refreshResponse("fresh-access-token");
      }
      return { status: 500, headers: {}, body: "quota unavailable" };
    },
    stream: (_url: string, init?: RequestInit) => {
      authorizations.push(init?.headers?.authorization ?? "");
      return completedStream();
    },
  };
  const result = await Promise.all([
    invoke("stale-access-token", handlers),
    invoke("stale-access-token", handlers),
  ]);
  assertEquals(refreshes, 1);
  assertEquals(authorizations, [
    "Bearer fresh-access-token",
    "Bearer fresh-access-token",
  ]);
  for (const item of result) {
    assertEquals(item.status, "completed");
    assertEquals(privateData(item).accessToken, "fresh-access-token");
    assertEquals(privateData(item).refreshToken, "refresh-new");
  }
});

Deno.test("a 401 from the token endpoint invalidates the credential", async () => {
  resetRefreshStateForTests();
  const result = await invoke("stale-access-token", {
    fetch: (url) => {
      if (url === TOKEN_URL) return { status: 401, headers: {}, body: "" };
      return { status: 500, headers: {}, body: "quota unavailable" };
    },
  });
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assertEquals(result.patch.state, { status: "invalid", message: AUTH_EXPIRED_MESSAGE });
});

Deno.test("a 503 refresh is retried twice while a transport error is not retried", async () => {
  resetRefreshStateForTests();
  let attempts = 0;
  const quotaUnavailable = { status: 500, headers: {}, body: "quota unavailable" };
  const unavailable = await invoke("stale-access-token", {
    fetch: (url) => {
      if (url === TOKEN_URL) {
        attempts += 1;
        return { status: 503, headers: {}, body: "token service unavailable" };
      }
      return quotaUnavailable;
    },
  });
  assertEquals(attempts, 3);
  assert(
    unavailable.status === "request-error",
    `expected request-error, received ${unavailable.status}`,
  );
  assert(!unavailable.patch, "an exhausted transient refresh must not rewrite the state");

  resetRefreshStateForTests();
  attempts = 0;
  const unreachable = await invoke("stale-access-token", {
    fetch: (url) => {
      if (url === TOKEN_URL) {
        attempts += 1;
        throw new Error("network down");
      }
      return quotaUnavailable;
    },
  });
  assertEquals(attempts, 1);
  assert(
    unreachable.status === "request-error",
    `expected request-error, received ${unreachable.status}`,
  );
  assert(!unreachable.patch, "a transport failure must not rewrite the state");
});

Deno.test("a shared refresh failure aborts both requests without invalidating the account", async () => {
  resetRefreshStateForTests();
  let refreshes = 0;
  let streams = 0;
  const handlers = {
    fetch: async (url: string) => {
      if (url === TOKEN_URL) {
        refreshes += 1;
        await new Promise((resolve) => setTimeout(resolve, 10));
        throw new Error("network down");
      }
      return { status: 500, headers: {}, body: "quota unavailable" };
    },
    stream: () => {
      streams += 1;
      return completedStream();
    },
  };
  const result = await Promise.all([
    invoke("stale-access-token", handlers),
    invoke("stale-access-token", handlers),
  ]);
  assertEquals(refreshes, 1);
  assertEquals(streams, 0);
  for (const item of result) {
    assert(item.status === "request-error", `expected request-error, received ${item.status}`);
    assert(!item.patch, "a transient refresh failure must not rewrite the resource state");
  }
});

Deno.test("a revoked refresh token invalidates both concurrent requests once", async () => {
  resetRefreshStateForTests();
  let refreshes = 0;
  let streams = 0;
  const handlers = {
    fetch: async (url: string) => {
      if (url === TOKEN_URL) {
        refreshes += 1;
        await new Promise((resolve) => setTimeout(resolve, 10));
        return {
          status: 400,
          headers: {},
          body: JSON.stringify({
            error: "invalid_grant",
            error_description: "Token has been revoked.",
          }),
        };
      }
      return { status: 500, headers: {}, body: "quota unavailable" };
    },
    stream: () => {
      streams += 1;
      return completedStream();
    },
  };
  const result = await Promise.all([
    invoke("stale-access-token", handlers),
    invoke("stale-access-token", handlers),
  ]);
  assertEquals(refreshes, 1);
  assertEquals(streams, 0);
  for (const item of result) {
    assert(item.status === "resource-error", `expected resource-error, received ${item.status}`);
    assertEquals(item.patch.state, { status: "invalid", message: AUTH_EXPIRED_MESSAGE });
    assertEquals(item.patch.privateData, undefined);
  }
});
