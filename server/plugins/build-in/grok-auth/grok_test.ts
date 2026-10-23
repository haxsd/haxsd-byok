import type {
  JsonValue,
  NetworkEventStream,
  NetworkResponse,
  PluginContext,
} from "cursor-byok:plugin";
import type { LlmRequest, ModelEvent } from "cursor-byok:provider";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { grokDeviceOAuth } from "./oauth.ts";
import { grokModels, parseGrokModels } from "./models.ts";
import { grokProvider, isQuotaError } from "./provider.ts";
import {
  accessTokenNeedsRefresh,
  accountData,
  accountIdentity,
  credentialDraft,
  parseCredentialFiles,
  parseGrokUsage,
  presentAccount,
  quotaState,
  refreshAccount,
  RESOURCE_TYPE,
} from "./resources.ts";

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
    key: "grok:user-1",
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
    maxOutputTokens: 32_000,
    cacheKey: "conversation-1",
  };
}

Deno.test("account identity uses the JWT subject and drafts keep tokens private-side", async () => {
  const token = jwt({ sub: "user-1", email: "person@x.ai" });
  assertEquals(await accountIdentity(token), {
    key: "grok:user-1",
    displayName: "person@x.ai",
  });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  assertEquals(draft.key, "grok:user-1");
  const view = presentAccount(snapshot(draft.privateData));
  assert(!JSON.stringify(view).includes(token), "resource view exposed an access token");
  assertEquals(view.displayName, "person@x.ai");
});

Deno.test("credential import accepts Grok credential JSON files", () => {
  const { credentials, warnings } = parseCredentialFiles([
    {
      name: "accounts.json",
      content: JSON.stringify({
        accounts: [
          { access_token: "token-1", refresh_token: "refresh-1", email: "a@x.ai" },
          { access_token: "token-2", disabled: true },
        ],
      }),
    },
    { name: "broken.json", content: "{not json" },
  ]);
  assertEquals(credentials, [{
    accessToken: "token-1",
    refreshToken: "refresh-1",
    displayName: "a@x.ai",
  }]);
  assertEquals(warnings, ["broken.json: not valid JSON"]);
});

Deno.test("credit usage percent is inverted to remaining and drives cooling", () => {
  const quota = parseGrokUsage({
    config: {
      creditUsagePercent: 34,
      subscriptionTierDisplay: "SuperGrok",
      currentPeriod: { end: "2026-09-01T00:00:00Z" },
    },
  }, 1_700_000_000_000);
  assertEquals(quota.planLabel, "SuperGrok");
  assertEquals(quota.remainingPercent, 66);
  assertEquals(quota.resetAtMs, Date.parse("2026-09-01T00:00:00Z"));
  assertEquals(quotaState(quota, 1_700_000_000_000), { status: "ready" });

  const exhausted = parseGrokUsage({
    config: { creditUsagePercent: 100, currentPeriod: { end: 1_900_000_000 } },
  }, 1_700_000_000_000);
  assertEquals(quotaState(exhausted, 1_700_000_000_000), {
    status: "cooling",
    retryAtMs: 1_900_000_000_000,
    message: "Grok credits are exhausted",
  });
});

Deno.test("missing usage stays unknown instead of counting as unused", () => {
  const quota = parseGrokUsage({ config: { currentPeriod: { end: 1_900_000_000 } } });
  assertEquals(quota.usedPercent, null);
  assertEquals(quota.remainingPercent, null);
  assertEquals(quota.limitReached, false);
  assertEquals(quota.resetAtMs, 1_900_000_000_000);
});

Deno.test("model discovery parses both language-models and standard list shapes", () => {
  const richModels = parseGrokModels({
    models: [
      { id: "grok-4", input_modalities: ["text", "image"], context_window: 256_000 },
      { id: "grok-3-mini", input_modalities: ["text"] },
      { id: "grok-4" },
    ],
  });
  assertEquals(richModels.map((model) => model.id), ["grok-4", "grok-3-mini"]);
  assertEquals(richModels[0].displayName, "Grok 4");
  assertEquals(richModels[0].capabilities, { images: true });
  assertEquals(richModels[1].capabilities, { images: false });

  const plainModels = parseGrokModels({ data: [{ id: "grok-4-fast" }] });
  assertEquals(plainModels.map((model) => model.id), ["grok-4-fast"]);
  assertEquals(plainModels[0].displayName, "Grok 4 Fast");
});

Deno.test("model discovery propagates failures instead of substituting a static catalog", async () => {
  const draft = await credentialDraft({
    accessToken: jwt({ sub: "user-1" }),
    refreshToken: null,
    displayName: null,
  });
  const requested: string[] = [];
  let rejected = false;
  try {
    await grokModels.list(
      { resource: snapshot(draft.privateData) },
      context({
        fetch: (url) => {
          requested.push(url);
          return {
            status: 403,
            headers: {},
            body: JSON.stringify({ code: "personal-team-blocked:spending-limit" }),
          };
        },
      }),
    );
  } catch (error) {
    rejected = true;
    const message = String(error);
    assert(message.includes("HTTP 403"), `expected the status in ${message}`);
    assert(message.includes("previous catalog is kept"), `expected a hint in ${message}`);
  }
  assert(rejected, "a failed discovery must not resolve with a static catalog");
  assertEquals(requested, [
    "https://api.x.ai/v1/language-models",
    "https://api.x.ai/v1/models",
  ]);
});

Deno.test("model discovery surfaces a rejected credential instead of falling back", async () => {
  const draft = await credentialDraft({
    accessToken: jwt({ sub: "user-1" }),
    refreshToken: null,
    displayName: null,
  });
  let rejected = false;
  try {
    await grokModels.list(
      { resource: snapshot(draft.privateData) },
      context({
        fetch: () => ({ status: 401, headers: {}, body: '{"error":"token expired"}' }),
      }),
    );
  } catch (error) {
    rejected = true;
    const message = String(error);
    assert(message.includes("HTTP 401"), `expected the status in ${message}`);
    assert(message.includes("sign in again"), `expected a re-auth hint in ${message}`);
  }
  assert(rejected, "a 401 must reach the caller");
});

Deno.test("refresh is scheduled from the stored expiry and the JWT expiry", () => {
  const token = jwt({ sub: "user-1", exp: Math.floor(Date.now() / 1000) + 3600 });
  const fresh = accountData(snapshot({
    accessToken: token,
    refreshToken: "refresh-secret",
    displayName: "Fixture",
    quota: null,
  }));
  assert(!accessTokenNeedsRefresh(fresh), "a fresh token must not be refreshed");
  assert(
    accessTokenNeedsRefresh({ ...fresh, expiresAtMs: Date.now() - 1 }),
    "a stored expiry in the past must refresh",
  );
  assert(
    !accessTokenNeedsRefresh({ ...fresh, refreshToken: null }),
    "no refresh token means no preflight refresh",
  );
  const expiredToken = jwt({ sub: "user-1", exp: Math.floor(Date.now() / 1000) - 60 });
  const expired = accountData(snapshot({
    accessToken: expiredToken,
    refreshToken: "refresh-secret",
    displayName: "Fixture",
    quota: null,
  }));
  assert(accessTokenNeedsRefresh(expired), "an expired JWT must be refreshed");
});

Deno.test("an expired access token is refreshed before calling Grok and persisted", async () => {
  const staleToken = jwt({ sub: "user-1", exp: Math.floor(Date.now() / 1000) - 60 });
  const freshToken = jwt({ sub: "user-1", exp: Math.floor(Date.now() / 1000) + 3600 });
  let refreshes = 0;
  let authorization: string | null = null;
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot({
        accessToken: staleToken,
        refreshToken: "refresh-old",
        displayName: "person@x.ai",
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: (url, init) => {
        refreshes += 1;
        assertEquals(url, "https://auth.x.ai/oauth2/token");
        const body = new URLSearchParams(init?.body);
        assertEquals(body.get("grant_type"), "refresh_token");
        assertEquals(body.get("refresh_token"), "refresh-old");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            access_token: freshToken,
            refresh_token: "refresh-new",
            expires_in: 3600,
          }),
        };
      },
      stream: (_url, init) => {
        authorization = init?.headers?.authorization ?? null;
        return {
          status: 200,
          headers: {},
          lines: sse(['data: {"choices":[{"delta":{},"finish_reason":"stop"}]}', "data: [DONE]"]),
        };
      },
    }),
  );
  assertEquals(refreshes, 1);
  assertEquals(authorization, `Bearer ${freshToken}`);
  assert(result.status === "completed", `expected completed, received ${result.status}`);
  const privateData = result.patch?.privateData as Record<string, unknown>;
  assertEquals(privateData.accessToken, freshToken);
  assertEquals(privateData.refreshToken, "refresh-new");
  assert(typeof privateData.expiresAtMs === "number" && privateData.expiresAtMs > Date.now());
});

Deno.test("a rejected access token is refreshed once and the request retried once", async () => {
  const freshToken = jwt({ sub: "user-1", exp: Math.floor(Date.now() / 1000) + 3600 });
  let streams = 0;
  let refreshes = 0;
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot({
        accessToken: jwt({ sub: "user-1" }),
        refreshToken: "refresh-old",
        displayName: "person@x.ai",
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: () => {
        refreshes += 1;
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({ access_token: freshToken, expires_in: 3600 }),
        };
      },
      stream: () => {
        streams += 1;
        return streams === 1
          ? { status: 401, headers: {}, lines: sse(['{"error":"token expired"}']) }
          : {
            status: 200,
            headers: {},
            lines: sse(['data: {"choices":[{"delta":{},"finish_reason":"stop"}]}', "data: [DONE]"]),
          };
      },
    }),
  );
  assertEquals(streams, 2);
  assertEquals(refreshes, 1);
  assert(result.status === "completed", `expected completed, received ${result.status}`);
  assertEquals((result.patch?.privateData as Record<string, unknown>).accessToken, freshToken);
});

Deno.test("a revoked refresh token invalidates the Grok credential", async () => {
  let streams = 0;
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot({
        accessToken: jwt({ sub: "user-1" }),
        refreshToken: "revoked",
        displayName: "person@x.ai",
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: () => ({
        status: 400,
        headers: {},
        body: JSON.stringify({ error: "invalid_grant", error_description: "revoked" }),
      }),
      stream: () => {
        streams += 1;
        return { status: 401, headers: {}, lines: sse(['{"error":"token expired"}']) };
      },
    }),
  );
  assertEquals(streams, 1);
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assertEquals(result.patch.state?.status, "invalid");
});

Deno.test("a transient refresh failure neither retries with the stale token nor invalidates", async () => {
  let streams = 0;
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot({
        accessToken: jwt({ sub: "user-1" }),
        refreshToken: "refresh-old",
        displayName: "person@x.ai",
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: () => ({ status: 503, headers: {}, body: "token service unavailable" }),
      stream: () => {
        streams += 1;
        return { status: 401, headers: {}, lines: sse(['{"error":"token expired"}']) };
      },
    }),
  );
  assert(streams === 1, "the stale token must not be reused");
  assert(result.status === "request-error", `expected request-error, received ${result.status}`);
});

Deno.test("device OAuth begins with a host-held session and completes with a resource draft", async () => {
  const accessToken = jwt({ sub: "user-oauth", email: "oauth@x.ai" });
  let requestNumber = 0;
  const flowContext = context({
    fetch: (url, init) => {
      requestNumber += 1;
      if (requestNumber === 1) {
        assertEquals(url, "https://auth.x.ai/oauth2/device/code");
        assert(init?.body?.includes("scope="), "device code request must carry the scope");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            device_code: "private-device-code",
            user_code: "ABCD-EFGH",
            verification_uri: "https://accounts.x.ai/activate",
            verification_uri_complete: "https://accounts.x.ai/activate?code=ABCD-EFGH",
            expires_in: 900,
            interval: 5,
          }),
        };
      }
      assertEquals(url, "https://auth.x.ai/oauth2/token");
      assert(init?.body?.includes("device_code=private-device-code"));
      if (requestNumber === 2) {
        return {
          status: 400,
          headers: {},
          body: JSON.stringify({ error: "authorization_pending" }),
        };
      }
      return {
        status: 200,
        headers: {},
        body: JSON.stringify({ access_token: accessToken, refresh_token: "refresh-secret" }),
      };
    },
  });

  const begun = await grokDeviceOAuth.begin(flowContext);
  assertEquals(begun.userCode, "ABCD-EFGH");
  assertEquals(begun.pollIntervalMs, 5000);

  const pending = await grokDeviceOAuth.poll(begun.session, flowContext);
  assertEquals(pending.status, "pending");

  const polled = await grokDeviceOAuth.poll(begun.session, flowContext);
  assert(polled.status === "completed", `expected completed, received ${polled.status}`);
  assertEquals(polled.resources[0].key, "grok:user-oauth");
  assertEquals(requestNumber, 3);
});

Deno.test("invoke streams normalized events from the xAI Chat Completions API", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  let requestBody = "";
  let requestHeaders: Record<string, string> = {};
  const events: ModelEvent[] = [];
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      stream: (url, init) => {
        assertEquals(url, "https://api.x.ai/v1/chat/completions");
        requestBody = init?.body ?? "";
        requestHeaders = init?.headers ?? {};
        return {
          status: 200,
          headers: {},
          lines: sse([
            'data: {"choices":[{"delta":{"content":"Hel"}}]}',
            'data: {"choices":[{"delta":{"content":"lo"}}]}',
            'data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4}}}',
            "data: [DONE]",
          ]),
        };
      },
    }),
  );
  assertEquals(result, { status: "completed" });
  const body = JSON.parse(requestBody) as Record<string, unknown>;
  assertEquals(body.model, "grok-4");
  assertEquals(body.stream, true);
  assertEquals(body.prompt_cache_key, "conversation-1");
  assert(!("reasoning_effort" in body), "xAI endpoint rejects reasoning_effort");
  assert(!("service_tier" in body), "xAI endpoint rejects service_tier");
  assertEquals(requestHeaders["authorization"], `Bearer ${token}`);
  assertEquals(events, [
    { type: "text-start" },
    { type: "text-delta", text: "Hel" },
    { type: "text-delta", text: "lo" },
    { type: "text-end" },
    {
      type: "usage",
      usage: {
        inputTokens: 10,
        outputTokens: 2,
        totalTokens: null,
        cacheReadTokens: 4,
        cacheWriteTokens: null,
        reasoningTokens: null,
      },
    },
    { type: "done", reason: "stop" },
  ]);
});

Deno.test("invoke streams incremental tool calls and reasoning replay state", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const events: ModelEvent[] = [];
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      stream: () => ({
        status: 200,
        headers: {},
        lines: sse([
          'data: {"choices":[{"delta":{"reasoning_content":"thinking"}}]}',
          'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{\\"path\\":"}}]}}]}',
          'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\\"a.ts\\"}"}}]}}]}',
          'data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}',
          "data: [DONE]",
        ]),
      }),
    }),
  );
  assertEquals(result, { status: "completed" });
  assertEquals(events, [
    { type: "thinking-start" },
    { type: "thinking-delta", text: "thinking" },
    { type: "tool-call-start", index: 0, callId: "call-1", name: "read_file" },
    { type: "tool-call-arguments-delta", index: 0, delta: '{"path":' },
    { type: "tool-call-arguments-delta", index: 0, delta: '"a.ts"}' },
    { type: "thinking-end" },
    { type: "tool-call-end", index: 0 },
    {
      type: "replay-state",
      providerKind: "openai_chat",
      value: { reasoning_content: "thinking" },
    },
    { type: "done", reason: "tool-use" },
  ]);
});

Deno.test("temporary rate limits preserve credits and honor Retry-After", async () => {
  const draft = await credentialDraft({
    accessToken: jwt({ sub: "rate-limited" }),
    refreshToken: null,
    displayName: null,
  });
  const before = Date.now();
  let calls = 0;
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => {
        calls += 1;
        return {
          status: 429,
          headers: { "retry-after": "120" },
          lines: sse(['{"error":{"code":"rate_limit_exceeded","message":"Too many requests"}}']),
        };
      },
    }),
  );
  assert(result.status === "resource-error");
  assert(result.patch.state?.status === "cooling");
  assert(
    result.patch.privateData === undefined,
    "rate limiting must not mark credits as exhausted",
  );
  const retryAt = result.patch.state.retryAtMs ?? 0;
  assert(
    retryAt >= before + 120_000 && retryAt <= Date.now() + 120_000,
    "rate limiting must respect the server retry deadline",
  );
  assertEquals(calls, 1);
});

Deno.test("HTTP-date rate limits resume at the server deadline", async () => {
  const draft = await credentialDraft({
    accessToken: "fixture-only",
    refreshToken: null,
    displayName: null,
  });
  const deadline = Math.floor(Date.now() / 1000) * 1000 + 90_000;
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => ({
        status: 429,
        headers: { "Retry-After": new Date(deadline).toUTCString() },
        lines: sse(['{"error":"rate_limit_exceeded"}']),
      }),
    }),
  );
  assert(result.status === "resource-error");
  assert(result.patch.state?.status === "cooling");
  assertEquals(result.patch.state.retryAtMs, deadline);
});

Deno.test("usage refresh cannot cancel an active rate-limit cooldown", async () => {
  const draft = await credentialDraft({
    accessToken: "fixture-only",
    refreshToken: null,
    displayName: null,
  });
  const resource = snapshot(draft.privateData);
  resource.state = { status: "cooling", retryAtMs: Date.now() + 120_000, message: "Rate limited" };
  const patch = await refreshAccount(
    resource,
    context({
      fetch: () => ({
        status: 200,
        headers: {},
        body: JSON.stringify({ config: { creditUsagePercent: 34 } }),
      }),
    }),
  );
  assertEquals(patch.state, resource.state);
});

Deno.test("usage refresh releases exhausted credits when the server grants quota again", async () => {
  const resource = snapshot({
    accessToken: "fixture-only",
    refreshToken: null,
    displayName: "Fixture",
    quota: parseGrokUsage({ config: { creditUsagePercent: 100 } }),
  });
  resource.state = { status: "cooling", retryAtMs: Date.now() + 120_000 };
  const patch = await refreshAccount(
    resource,
    context({
      fetch: () => ({
        status: 200,
        headers: {},
        body: JSON.stringify({ config: { creditUsagePercent: 34 } }),
      }),
    }),
  );
  assertEquals(patch.state, { status: "ready" });
});

Deno.test("usage refresh rotates a rejected credential instead of invalidating it", async () => {
  const freshToken = jwt({ sub: "user-1", exp: Math.floor(Date.now() / 1000) + 3600 });
  let refreshes = 0;
  let usageCalls = 0;
  const patch = await refreshAccount(
    snapshot({
      accessToken: jwt({ sub: "user-1" }),
      refreshToken: "refresh-old",
      displayName: "person@x.ai",
      quota: null,
    }),
    context({
      fetch: (url, init) => {
        if (url === "https://auth.x.ai/oauth2/token") {
          refreshes += 1;
          return {
            status: 200,
            headers: {},
            body: JSON.stringify({ access_token: freshToken, expires_in: 3600 }),
          };
        }
        assertEquals(url, "https://cli-chat-proxy.grok.com/v1/billing?format=credits");
        usageCalls += 1;
        if (usageCalls === 1) return { status: 401, headers: {}, body: "expired" };
        assertEquals(init?.headers?.authorization, `Bearer ${freshToken}`);
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({ config: { creditUsagePercent: 34 } }),
        };
      },
    }),
  );
  assertEquals(refreshes, 1);
  assertEquals(usageCalls, 2);
  assertEquals((patch.privateData as Record<string, unknown>).accessToken, freshToken);
  assertEquals(patch.state, { status: "ready" });
});

Deno.test("billing permission errors do not invalidate inference authorization", async () => {
  const resource = snapshot({
    accessToken: "fixture-only",
    refreshToken: null,
    displayName: "Fixture",
    quota: null,
  });
  let rejected = false;
  try {
    await refreshAccount(
      resource,
      context({
        fetch: () => ({ status: 403, headers: {}, body: "Billing access is forbidden" }),
      }),
    );
  } catch (error) {
    rejected = true;
    assert(String(error).includes("403"));
  }
  assert(rejected, "a billing-only failure must be reported without invalidating the account");
});

Deno.test("a forbidden model does not invalidate the entire Grok account", async () => {
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot({
        accessToken: "fixture-only",
        refreshToken: null,
        displayName: "Fixture",
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => ({
        status: 403,
        headers: {},
        lines: sse(['{"error":"model access is forbidden"}']),
      }),
    }),
  );
  assertEquals(result.status, "request-error");
});

Deno.test("an unauthorized token still invalidates the Grok account", async () => {
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot({
        accessToken: "fixture-only",
        refreshToken: null,
        displayName: "Fixture",
        quota: null,
      }),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => ({
        status: 401,
        headers: {},
        lines: sse(['{"error":"token expired"}']),
      }),
    }),
  );
  assert(result.status === "resource-error");
  assertEquals(result.patch.state?.status, "invalid");
});

Deno.test("invoke maps quota failures to a cooling resource error", async () => {
  assert(!isQuotaError("400 invalid request"));
  assert(isQuotaError("429 credits exhausted"));
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => ({
        status: 429,
        headers: {},
        lines: sse(['{"error":"credits exhausted"}']),
      }),
    }),
  );
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assert(result.patch.state?.status === "cooling", "quota failure should cool the resource");
});
