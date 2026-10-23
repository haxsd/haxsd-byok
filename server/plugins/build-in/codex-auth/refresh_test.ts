import type {
  JsonValue,
  NetworkEventStream,
  NetworkResponse,
  PluginContext,
} from "cursor-byok:plugin";
import type { LlmRequest } from "cursor-byok:provider";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { codexProvider } from "./provider.ts";
import { AUTH_EXPIRED_MESSAGE, resetRefreshStateForTests, RESOURCE_TYPE } from "./resources.ts";

const TOKEN_URL = "https://auth.openai.com/oauth/token";

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

function jwt(payload: Record<string, unknown>): string {
  const encoded = btoa(JSON.stringify(payload)).replace(/=/g, "").replace(/\+/g, "-").replace(
    /\//g,
    "_",
  );
  return `header.${encoded}.signature`;
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
    key: "codex:acct-1",
    privateData,
    state: { status: "ready" },
  };
}

async function* sse(lines: string[]): AsyncGenerator<string> {
  for (const line of lines) yield line;
}

function request(): LlmRequest {
  return {
    instructions: "You are a coding assistant.",
    messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
    tools: [],
    reasoning: { enabled: true, effort: "medium" },
    latency: "fast",
    maxOutputTokens: 128_000,
    cacheKey: "conversation-1",
  };
}

function account(accessToken: string, refreshToken: string): JsonValue {
  return {
    accessToken,
    refreshToken,
    accountId: "acct-1",
    displayName: "person@example.com",
    quota: null,
  };
}

function staleToken(): string {
  return jwt({
    "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" },
    exp: Math.floor(Date.now() / 1000) - 60,
  });
}

function freshToken(): string {
  return jwt({
    "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" },
    exp: Math.floor(Date.now() / 1000) + 3600,
  });
}

function invoke(token: string, handlers: { fetch?: FetchHandler; stream?: StreamHandler }) {
  return codexProvider.invoke(
    {
      model: { id: "gpt-test", displayName: "GPT Test" },
      resource: snapshot(account(token, "refresh-old")),
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
    lines: sse(['data: {"type":"response.completed","response":{"id":"resp-1"}}']),
  };
}

Deno.test("a 401 after the preflight refresh invalidates without refreshing twice", async () => {
  resetRefreshStateForTests();
  const fresh = freshToken();
  let refreshes = 0;
  let streams = 0;
  const result = await invoke(staleToken(), {
    fetch: (url) => {
      refreshes += 1;
      assertEquals(url, TOKEN_URL);
      return {
        status: 200,
        headers: {},
        body: JSON.stringify({
          access_token: fresh,
          refresh_token: "refresh-new",
          expires_in: 3600,
        }),
      };
    },
    stream: () => {
      streams += 1;
      return { status: 401, headers: {}, lines: sse(['{"detail":"token expired"}']) };
    },
  });
  assertEquals(refreshes, 1);
  assertEquals(streams, 1);
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assertEquals(result.patch.state, { status: "invalid", message: AUTH_EXPIRED_MESSAGE });
  // 主动刷新已经换到新 refresh token,即使随后 401 也必须写回,避免下一轮重复消费旧值。
  assertEquals(
    (result.patch.privateData as Record<string, unknown>).refreshToken,
    "refresh-new",
  );
});

Deno.test("two concurrent requests share one preflight refresh and persist the rotation", async () => {
  resetRefreshStateForTests();
  const fresh = freshToken();
  const stale = staleToken();
  let refreshes = 0;
  const authorizations: string[] = [];
  const result = await Promise.all([
    invoke(stale, {
      fetch: async (url) => {
        refreshes += 1;
        assertEquals(url, TOKEN_URL);
        // 让两个请求都先进入刷新等待,证明它们共享同一个 Promise。
        await new Promise((resolve) => setTimeout(resolve, 10));
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            access_token: fresh,
            refresh_token: "refresh-new",
            expires_in: 3600,
          }),
        };
      },
      stream: (_url, init) => {
        authorizations.push(init?.headers?.authorization ?? "");
        return completedStream();
      },
    }),
    invoke(stale, {
      fetch: async (url) => {
        refreshes += 1;
        assertEquals(url, TOKEN_URL);
        await new Promise((resolve) => setTimeout(resolve, 10));
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            access_token: fresh,
            refresh_token: "refresh-new",
            expires_in: 3600,
          }),
        };
      },
      stream: (_url, init) => {
        authorizations.push(init?.headers?.authorization ?? "");
        return completedStream();
      },
    }),
  ]);
  assertEquals(refreshes, 1);
  assertEquals(authorizations, [`Bearer ${fresh}`, `Bearer ${fresh}`]);
  for (const item of result) {
    assertEquals(item.status, "completed");
    const privateData = item.patch?.privateData as Record<string, unknown>;
    assertEquals(privateData.accessToken, fresh);
    assertEquals(privateData.refreshToken, "refresh-new");
  }
});

Deno.test("a preflight refresh and a 401 refresh share the same account flight", async () => {
  resetRefreshStateForTests();
  const fresh = freshToken();
  // 第二个请求的令牌看起来仍然有效,只有上游 401 才会触发刷新。
  const stillValid = jwt({
    "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" },
    sub: "still-valid-subject",
    exp: Math.floor(Date.now() / 1000) + 3600,
  });
  let refreshes = 0;
  const authorizations: string[] = [];
  const handlers = {
    fetch: async (url: string) => {
      refreshes += 1;
      assertEquals(url, TOKEN_URL);
      // 预刷新先占住 single-flight,401 分支必须复用它而不是再发一次。
      await new Promise((resolve) => setTimeout(resolve, 10));
      return {
        status: 200,
        headers: {},
        body: JSON.stringify({
          access_token: fresh,
          refresh_token: "refresh-new",
          expires_in: 3600,
        }),
      };
    },
    stream: (_url: string, init?: RequestInit) => {
      const authorization = init?.headers?.authorization ?? "";
      authorizations.push(authorization);
      return authorization === `Bearer ${stillValid}`
        ? { status: 401, headers: {}, lines: sse(['{"detail":"token expired"}']) }
        : completedStream();
    },
  };
  const result = await Promise.all([
    invoke(staleToken(), handlers),
    invoke(stillValid, handlers),
  ]);
  assertEquals(refreshes, 1);
  assertEquals(authorizations.length, 3);
  assertEquals(authorizations.filter((value) => value === `Bearer ${fresh}`).length, 2);
  for (const item of result) {
    assertEquals(item.status, "completed");
    assertEquals(
      (item.patch?.privateData as Record<string, unknown>).refreshToken,
      "refresh-new",
    );
  }
});

Deno.test("a 503 refresh is retried twice while a transport error is not retried", async () => {
  resetRefreshStateForTests();
  let attempts = 0;
  const unavailable = await invoke(staleToken(), {
    fetch: () => {
      attempts += 1;
      return { status: 503, headers: {}, body: "token service unavailable" };
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
  const unreachable = await invoke(staleToken(), {
    fetch: () => {
      attempts += 1;
      throw new Error("network down");
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
  const stale = staleToken();
  let refreshes = 0;
  let streams = 0;
  const handlers = {
    fetch: async (url: string) => {
      refreshes += 1;
      assertEquals(url, TOKEN_URL);
      await new Promise((resolve) => setTimeout(resolve, 10));
      throw new Error("network down");
    },
    stream: () => {
      streams += 1;
      return completedStream();
    },
  };
  const result = await Promise.all([invoke(stale, handlers), invoke(stale, handlers)]);
  assertEquals(refreshes, 1);
  assertEquals(streams, 0);
  for (const item of result) {
    assert(item.status === "request-error", `expected request-error, received ${item.status}`);
    assert(!item.patch, "a transient refresh failure must not rewrite the resource state");
  }
});
