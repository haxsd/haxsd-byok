import type {
  JsonValue,
  NetworkEventStream,
  NetworkResponse,
  PluginContext,
} from "cursor-byok:plugin";
import type { LlmRequest, ModelEvent } from "cursor-byok:provider";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { buildRequestBody, claudeCodeProvider, REPLAY_KIND } from "./provider.ts";
import {
  accountData,
  CLAUDE_TOKEN_URL,
  isTokenExpired,
  REFRESH_AHEAD_MS,
  RESOURCE_TYPE,
} from "./resources.ts";
import { resetRuntimeForTests } from "./runtime.ts";

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

type RequestInit = { body?: string; headers?: Record<string, string> };
type WireMessage = { role: string; content: Array<Record<string, unknown>> };
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

function snapshot(privateData: JsonValue, id = "resource-1"): ResourceSnapshot {
  return {
    id,
    type: RESOURCE_TYPE,
    key: "claude-code:account-1",
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
    messages: [
      {
        role: "user",
        content: [
          { type: "text", text: "Inspect the file" },
          { type: "image", mediaType: "image/png", dataBase64: "BBBB" },
        ],
      },
      {
        role: "assistant",
        text: "checking",
        thinking: "plan",
        replayState: {
          providerKind: REPLAY_KIND,
          value: {
            blocks: [{ type: "thinking", thinking: "plan", signature: "sig-1" }],
          },
        },
        toolCalls: [{
          index: 0,
          callId: "toolu_1",
          name: "read_file",
          arguments: { path: "a.ts" },
        }],
      },
      {
        role: "tool",
        callId: "toolu_1",
        name: "read_file",
        content: "",
        isError: false,
        parts: [
          { type: "text", text: "file contents" },
          { type: "image", mediaType: "image/png", dataBase64: "AAAA" },
        ],
      },
    ],
    tools: [{
      name: "read_file",
      description: "Read a file",
      parameters: {
        type: "object",
        properties: { path: { type: "string" } },
        required: ["path"],
      },
    }],
    reasoning: { enabled: true, effort: "medium" },
    latency: "standard",
    maxOutputTokens: 128_000,
    cacheKey: "conversation-1",
  };
}

function account(
  accessToken: string,
  refreshToken = "refresh-token",
): JsonValue {
  return {
    accessToken,
    refreshToken,
    displayName: "person@example.com",
    accountUuid: "account-1",
    organizationName: "Example Org",
    expiresAtMs: Date.now() + 24 * 3600_000,
  };
}

/** 距到期不足 4 小时：请求应当在发出前先刷新。 */
function expiringAccount(accessToken = "expiring-token"): JsonValue {
  return {
    accessToken,
    refreshToken: "refresh-token",
    displayName: "person@example.com",
    accountUuid: "account-1",
    organizationName: "Example Org",
    expiresAtMs: Date.now() + REFRESH_AHEAD_MS - 60_000,
  };
}

/** 最小可用的 200 SSE 流：一段文本，正常结束。 */
function okStream(text = "ok"): NetworkEventStream {
  return {
    status: 200,
    headers: {},
    lines: sse([
      'data: {"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}',
      'data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}',
      `data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"${text}"}}`,
      'data: {"type":"content_block_stop","index":0}',
      'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}',
      'data: {"type":"message_stop"}',
    ]),
  };
}

Deno.test("request uses Anthropic blocks and stream parses thinking, text, tools, and usage", async () => {
  let requestBody = "";
  let requestHeaders: Record<string, string> = {};
  const events: ModelEvent[] = [];

  const result = await claudeCodeProvider.invoke(
    {
      model: {
        id: "claude-opus-5-5",
        displayName: "Claude Opus 5.5",
        maxOutputTokens: 128_000,
      },
      resource: snapshot(account("mock-access-token")),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      stream: (url, init) => {
        assert(url === "https://api.anthropic.com/v1/messages", "wrong messages endpoint");
        requestBody = init?.body ?? "";
        requestHeaders = init?.headers ?? {};
        return {
          status: 200,
          headers: {},
          lines: sse([
            'data: {"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5,"cache_creation_input_tokens":2,"output_tokens":0}}}',
            'data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}',
            'data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}',
            'data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}',
            'data: {"type":"content_block_stop","index":0}',
            'data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}',
            'data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"checking"}}',
            'data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" done"}}',
            'data: {"type":"content_block_stop","index":1}',
            'data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file"}}',
            'data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\\"path\\":\\"a.ts\\"}"}}',
            'data: {"type":"content_block_stop","index":2}',
            'data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}',
            'data: {"type":"message_stop"}',
          ]),
        };
      },
    }),
  );

  assertEquals(result, { status: "completed" });
  assertEquals(requestHeaders.authorization, "Bearer mock-access-token");
  assertEquals(requestHeaders["anthropic-version"], "2023-06-01");
  assertEquals(requestHeaders["anthropic-beta"], "oauth-2025-04-20");
  assertEquals(requestHeaders.accept, "text/event-stream");
  assert(
    !requestHeaders["user-agent"]?.startsWith("claude-cli/"),
    "the plugin must not impersonate Claude Code's user agent",
  );
  const headerKeys = Object.keys(requestHeaders).sort();
  assert(
    JSON.stringify(headerKeys) ===
      JSON.stringify([
        "accept",
        "anthropic-beta",
        "anthropic-version",
        "authorization",
        "content-type",
        "user-agent",
      ]),
    `the request must keep a minimal, honest header set, received ${headerKeys.join(", ")}`,
  );

  const body = JSON.parse(requestBody) as Record<string, unknown>;
  assertEquals(body.model, "claude-opus-5-5");
  assertEquals(body.max_tokens, 128_000);
  assertEquals(body.stream, true);
  assertEquals(body.system, "You are a coding assistant.");
  assertEquals(body.thinking, { type: "adaptive", display: "summarized" });
  assertEquals(body.output_config, { effort: "medium" });
  const bodyKeys = Object.keys(body).sort();
  assert(
    JSON.stringify(bodyKeys) ===
      JSON.stringify([
        "max_tokens",
        "messages",
        "model",
        "output_config",
        "stream",
        "system",
        "thinking",
        "tools",
      ]),
    `the body must not carry unused capabilities or identity metadata, received ${
      bodyKeys.join(", ")
    }`,
  );

  const messages = body.messages as WireMessage[];
  assertEquals(messages.map((message) => message.role), ["user", "assistant", "user"]);
  const image = messages[0].content[1] as Record<string, unknown>;
  assertEquals(image.type, "image");
  const assistant = messages[1].content;
  assertEquals(assistant[0].type, "thinking");
  assertEquals(assistant[0].signature, "sig-1");
  assertEquals(assistant[1], { type: "text", text: "checking" });
  assertEquals(assistant[2].type, "tool_use");
  const toolResult = messages[2].content[0] as Record<string, unknown>;
  assertEquals(toolResult.type, "tool_result");
  assertEquals(toolResult.tool_use_id, "toolu_1");
  const toolContent = toolResult.content as Array<Record<string, unknown>>;
  assertEquals(toolContent[1].type, "image");
  const tools = body.tools as Array<Record<string, unknown>>;
  assertEquals((tools[0].input_schema as Record<string, unknown>).type, "object");

  assertEquals(events.slice(0, 8), [
    { type: "thinking-start" },
    { type: "thinking-delta", text: "plan" },
    { type: "thinking-end" },
    { type: "text-start" },
    { type: "text-delta", text: "checking" },
    { type: "text-delta", text: " done" },
    { type: "text-end" },
    { type: "tool-call-start", index: 2, callId: "toolu_1", name: "read_file" },
  ]);
  assertEquals(events[8], {
    type: "tool-call-arguments-delta",
    index: 2,
    delta: '{"path":"a.ts"}',
  });
  assertEquals(events[9], { type: "tool-call-end", index: 2 });
  assertEquals(events[10], {
    type: "replay-state",
    providerKind: REPLAY_KIND,
    value: { blocks: [{ type: "thinking", thinking: "plan", signature: "sig-1" }] },
  });
  assertEquals(events[11], {
    type: "usage",
    usage: {
      inputTokens: 10,
      outputTokens: 20,
      totalTokens: 37,
      cacheReadTokens: 5,
      cacheWriteTokens: 2,
      reasoningTokens: null,
    },
  });
  assertEquals(events[12], { type: "done", reason: "tool-use" });
});

Deno.test("omits thinking when a trailing tool call has no signed thinking block", () => {
  const base = request();
  const body = buildRequestBody({
    model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
    resource: snapshot(account("mock-access-token")),
    request: {
      ...base,
      messages: [
        base.messages[0],
        {
          role: "assistant",
          text: "",
          thinking: "",
          replayState: null,
          toolCalls: [{ index: 0, callId: "toolu_2", name: "read_file", arguments: {} }],
        },
        {
          role: "tool",
          callId: "toolu_2",
          name: "read_file",
          content: "contents",
          isError: false,
          parts: [],
        },
      ],
    },
  });

  assert(!("thinking" in body), "unsigned tool continuations must not enable thinking");
  assert(!("output_config" in body), "effort must not be sent when thinking is omitted");
});

Deno.test("a 401 refreshes the token and retries the request once", async () => {
  const streamHeaders: Array<Record<string, string>> = [];
  let streams = 0;
  let refreshBody = "";
  const events: ModelEvent[] = [];

  const result = await claudeCodeProvider.invoke(
    {
      model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
      resource: snapshot(account("stale-token")),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      fetch: (url, init) => {
        assert(url === "https://platform.claude.com/v1/oauth/token", "wrong refresh endpoint");
        refreshBody = init?.body ?? "";
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            access_token: "fresh-token",
            refresh_token: "fresh-refresh",
            expires_in: 3600,
          }),
        };
      },
      stream: (_url, init) => {
        streams += 1;
        streamHeaders.push(init?.headers ?? {});
        if (streams === 1) {
          return {
            status: 401,
            headers: {},
            lines: sse([
              'data: {"type":"error","error":{"type":"authentication_error","message":"expired"}}',
            ]),
          };
        }
        return {
          status: 200,
          headers: {},
          lines: sse([
            'data: {"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}',
            'data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}',
            'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}',
            'data: {"type":"content_block_stop","index":0}',
            'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}',
            'data: {"type":"message_stop"}',
          ]),
        };
      },
    }),
  );

  if (result.status !== "completed") {
    throw new Error(`expected completed, received ${result.status}`);
  }
  assertEquals(streams, 2);
  assertEquals(streamHeaders[1].authorization, "Bearer fresh-token");
  const refresh = JSON.parse(refreshBody) as Record<string, unknown>;
  assertEquals(refresh.grant_type, "refresh_token");
  assertEquals(refresh.refresh_token, "refresh-token");
  assert(result.patch, "a successful refresh must persist the new tokens");
  const data = result.patch.privateData as Record<string, unknown>;
  assertEquals(data.accessToken, "fresh-token");
  assertEquals(data.refreshToken, "fresh-refresh");
  assertEquals(events.at(-1), { type: "done", reason: "stop" });
});

Deno.test("a failed refresh after 401 marks the resource invalid", async () => {
  const result = await claudeCodeProvider.invoke(
    {
      model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
      resource: snapshot(account("stale-token")),
      request: request(),
    },
    { emit: () => undefined },
    context({
      fetch: () => ({
        status: 400,
        headers: {},
        body: JSON.stringify({
          error: "invalid_grant",
          error_description: "refresh token revoked",
        }),
      }),
      stream: () => ({
        status: 401,
        headers: {},
        lines: sse([
          'data: {"type":"error","error":{"type":"authentication_error","message":"expired"}}',
        ]),
      }),
    }),
  );

  if (result.status !== "resource-error") {
    throw new Error(`expected a resource error, received ${result.status}`);
  }
  assertEquals(result.patch.state?.status, "invalid");
  assert(
    result.message.includes("invalid_grant"),
    "the upstream reason should be visible in the message",
  );
});

Deno.test("5xx is retried at most twice and then surfaced as a request error", async () => {
  resetRuntimeForTests();
  let streams = 0;
  const result = await claudeCodeProvider.invoke(
    {
      model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
      resource: snapshot(account("mock-access-token"), "resource-5xx"),
      request: request(),
    },
    { emit: () => undefined },
    context({
      stream: () => {
        streams += 1;
        return {
          status: 503,
          headers: {},
          lines: sse(['data: {"type":"error","error":{"message":"upstream unavailable"}}']),
        };
      },
    }),
  );

  assertEquals(streams, 3);
  assertEquals(result.status, "request-error");
});

Deno.test("4xx other than 401 is not retried", async () => {
  resetRuntimeForTests();
  let streams = 0;
  const result = await claudeCodeProvider.invoke(
    {
      model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
      resource: snapshot(account("mock-access-token"), "resource-400"),
      request: request(),
    },
    { emit: () => undefined },
    context({
      stream: () => {
        streams += 1;
        return {
          status: 400,
          headers: {},
          lines: sse(['data: {"type":"error","error":{"message":"third-party usage"}}']),
        };
      },
    }),
  );

  assertEquals(streams, 1);
  assertEquals(result.status, "request-error");
});

Deno.test("429 honors retry-after, cools the account, hides tokens, and blocks new requests", async () => {
  resetRuntimeForTests();
  let streams = 0;
  const resource = snapshot(account("mock-access-token"), "resource-429");
  const result = await claudeCodeProvider.invoke(
    {
      model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
      resource,
      request: request(),
    },
    { emit: () => undefined },
    context({
      stream: () => {
        streams += 1;
        return {
          status: 429,
          headers: { "retry-after": "120" },
          lines: sse([
            'data: {"type":"error","error":{"message":"rate limited for mock-access-token"}}',
          ]),
        };
      },
    }),
  );

  if (result.status !== "resource-error") {
    throw new Error(`expected a resource error, received ${result.status}`);
  }
  assertEquals(result.patch.state?.status, "cooling");
  const state = result.patch.state;
  if (state?.status !== "cooling") throw new Error("expected a cooling state");
  assert(
    (state.retryAtMs ?? 0) > Date.now() + 100_000,
    "retry-after must drive the cooldown window",
  );
  assert(!result.message.includes("mock-access-token"), "the access token must be redacted");

  const blocked = await claudeCodeProvider.invoke(
    {
      model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
      resource,
      request: request(),
    },
    { emit: () => undefined },
    context({
      stream: () => {
        streams += 1;
        return { status: 200, headers: {}, lines: sse([]) };
      },
    }),
  );

  assertEquals(streams, 1);
  assert(streams === 1, "a cooling account must not send another request");
  assertEquals(blocked.status, "resource-error");
  if (blocked.status === "resource-error") {
    assertEquals(blocked.patch.state?.status, "cooling");
  }
});

Deno.test("repeated 401s refresh once and then mark the account invalid", async () => {
  resetRuntimeForTests();
  let streams = 0;
  let refreshes = 0;
  const result = await claudeCodeProvider.invoke(
    {
      model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
      resource: snapshot(account("stale-token"), "resource-401"),
      request: request(),
    },
    { emit: () => undefined },
    context({
      fetch: () => {
        refreshes += 1;
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            access_token: "fresh-token",
            refresh_token: "fresh-refresh",
            expires_in: 3600,
          }),
        };
      },
      stream: () => {
        streams += 1;
        return {
          status: 401,
          headers: {},
          lines: sse([
            'data: {"type":"error","error":{"type":"authentication_error","message":"expired"}}',
          ]),
        };
      },
    }),
  );

  if (result.status !== "resource-error") {
    throw new Error(`expected a resource error, received ${result.status}`);
  }
  assertEquals(streams, 2);
  assertEquals(refreshes, 1);
  assert(streams === 2, "exactly one retry after the refresh");
  assert(refreshes === 1, "the refresh must happen exactly once");
  assertEquals(result.patch.state?.status, "invalid");
});

Deno.test("max_tokens is clamped to the declared model limit or 8192", () => {
  const base = {
    resource: snapshot(account("mock-access-token"), "resource-tokens"),
    request: request(),
  };
  const declared = buildRequestBody({
    ...base,
    model: { id: "claude-sonnet-4-5", displayName: "Sonnet 4.5", maxOutputTokens: 4096 },
  });
  assertEquals(declared.max_tokens, 4096);

  const undeclared = buildRequestBody({
    ...base,
    model: { id: "custom-model", displayName: "Custom" },
  });
  assertEquals(undeclared.max_tokens, 8192);
});

Deno.test("refresh failures never echo the refresh token", async () => {
  resetRuntimeForTests();
  const secret = "refresh-secret-123456";
  const result = await claudeCodeProvider.invoke(
    {
      model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
      resource: snapshot(account("stale-token", secret), "resource-refresh-secret"),
      request: request(),
    },
    { emit: () => undefined },
    context({
      fetch: () => ({
        status: 400,
        headers: {},
        body: JSON.stringify({
          error: "invalid_grant",
          error_description: `refresh token ${secret} was revoked`,
        }),
      }),
      stream: () => ({
        status: 401,
        headers: {},
        lines: sse([
          'data: {"type":"error","error":{"type":"authentication_error","message":"expired"}}',
        ]),
      }),
    }),
  );

  if (result.status !== "resource-error") {
    throw new Error(`expected a resource error, received ${result.status}`);
  }
  assert(!result.message.includes(secret), "the refresh token must not appear in the message");
});

Deno.test("a transport error during refresh is not retried and keeps the old token", async () => {
  resetRuntimeForTests();
  let fetches = 0;
  const resource = snapshot(expiringAccount("unused-token"), "resource-transport");

  const result = await claudeCodeProvider.invoke(
    {
      model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
      resource,
      request: request(),
    },
    { emit: () => undefined },
    context({
      fetch: () => {
        fetches += 1;
        // 请求可能已经到达令牌端点并消费了 refresh token，重发会把账号彻底登出。
        return Promise.reject(new Error("socket closed while sending refresh-token"));
      },
      stream: () => {
        throw new Error("the request must not reach the Messages API without a token");
      },
    }),
  );

  assertEquals(fetches, 1);
  assert(fetches === 1, "transport errors must not be retried");
  if (result.status !== "resource-error") {
    throw new Error(`expected a resource error, received ${result.status}`);
  }
  assertEquals(result.patch.state?.status, "cooling");
  assert(
    result.message.includes("not retried"),
    "the message must explain why the refresh was not retried",
  );
  assert(!result.message.includes("refresh-token"), "the refresh token must be redacted");
  assert(
    result.patch.privateData === undefined,
    "a failed refresh must not overwrite the stored credentials",
  );
});

Deno.test("access tokens refresh four hours before they expire", () => {
  const at = (expiresInMs: number) =>
    accountData(snapshot({
      accessToken: "token",
      refreshToken: "refresh-token",
      displayName: "person@example.com",
      accountUuid: null,
      organizationName: null,
      expiresAtMs: Date.now() + expiresInMs,
    }));

  assertEquals(REFRESH_AHEAD_MS, 4 * 60 * 60 * 1000);
  assert(
    !isTokenExpired(at(REFRESH_AHEAD_MS + 60_000)),
    "a token with more than 4 hours left must not be refreshed",
  );
  assert(
    isTokenExpired(at(REFRESH_AHEAD_MS - 60_000)),
    "a token inside the 4 hour window must be refreshed",
  );
});

Deno.test("an expiring token is refreshed before the first request", async () => {
  resetRuntimeForTests();
  let fetches = 0;
  const streamHeaders: Array<Record<string, string>> = [];

  const result = await claudeCodeProvider.invoke(
    {
      model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
      resource: snapshot(expiringAccount(), "resource-proactive"),
      request: request(),
    },
    { emit: () => undefined },
    context({
      fetch: (url) => {
        fetches += 1;
        assert(url === CLAUDE_TOKEN_URL, "wrong refresh endpoint");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            access_token: "fresh-token",
            refresh_token: "rotated-refresh",
            expires_in: 3600,
          }),
        };
      },
      stream: (_url, init) => {
        streamHeaders.push(init?.headers ?? {});
        return okStream();
      },
    }),
  );

  if (result.status !== "completed") {
    throw new Error(`expected completed, received ${result.status}`);
  }
  assertEquals(fetches, 1);
  assertEquals(streamHeaders.length, 1);
  assertEquals(streamHeaders[0].authorization, "Bearer fresh-token");
  assert(result.patch, "a proactive refresh must be persisted");
  const data = result.patch.privateData as Record<string, unknown>;
  assertEquals(data.refreshToken, "rotated-refresh");
});

Deno.test("concurrent requests on one account share a single refresh", async () => {
  resetRuntimeForTests();
  let fetches = 0;
  const authorizations: string[] = [];

  const network = context({
    fetch: async (url) => {
      fetches += 1;
      assert(url === CLAUDE_TOKEN_URL, "wrong refresh endpoint");
      // 让两个请求都停在同一个 flight 里，验证的是去重而不是“恰好串行”。
      await new Promise((resolve) => setTimeout(resolve, 10));
      return {
        status: 200,
        headers: {},
        body: JSON.stringify({
          access_token: "fresh-token",
          refresh_token: "rotated-refresh",
          expires_in: 3600,
        }),
      };
    },
    stream: (_url, init) => {
      authorizations.push(init?.headers?.authorization ?? "");
      return okStream();
    },
  });

  const call = () =>
    claudeCodeProvider.invoke(
      {
        model: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
        resource: snapshot(expiringAccount(), "resource-single-flight"),
        request: request(),
      },
      { emit: () => undefined },
      network,
    );

  const results = await Promise.all([call(), call()]);
  assertEquals(fetches, 1);
  assert(fetches === 1, "a single refresh token must not be spent twice");
  assertEquals(authorizations.length, 2);
  for (const value of authorizations) assertEquals(value, "Bearer fresh-token");
  for (const result of results) assertEquals(result.status, "completed");
});
