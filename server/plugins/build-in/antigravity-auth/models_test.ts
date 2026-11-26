import type { JsonValue, NetworkResponse, PluginContext } from "cursor-byok:plugin";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { antigravityModels, parseAntigravityModels, STATIC_ANTIGRAVITY_MODELS } from "./models.ts";
import { RESOURCE_TYPE } from "./resources.ts";

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

type FetchHandler = () => NetworkResponse | Promise<NetworkResponse>;

function context(fetch: FetchHandler): PluginContext {
  return {
    network: {
      fetch: () => Promise.resolve(fetch()),
      stream: () => {
        throw new Error("stream was not expected");
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

function account(): ResourceSnapshot {
  return snapshot({
    accessToken: "access-token",
    refreshToken: "refresh-token",
    displayName: "Fixture",
    projectId: "project-1",
    expiresAtMs: null,
    quota: null,
  } as unknown as JsonValue);
}

const dynamicPayload = {
  models: {
    "gemini-3.8-flash": {
      displayName: "Gemini 3.8 Flash",
      supportsThinking: true,
      supportsImages: true,
      maxOutputTokens: 12_345,
    },
    "chat_20706": {
      displayName: "Denied model",
    },
  },
};

Deno.test("model parsing returns only the upstream catalog and filters denied models", () => {
  const models = parseAntigravityModels(dynamicPayload);
  assertEquals(models.map((model) => model.id), ["gemini-3.8-flash"]);
  assertEquals(models[0].capabilities, { images: true });
  assertEquals(models[0].maxOutputTokens, 12_345);
  assertEquals(models[0].privateData, { reasoningEfforts: ["low", "medium", "high"] });
  assert(
    !models.some((model) => model.id === STATIC_ANTIGRAVITY_MODELS[0].id),
    "a successful dynamic catalog must not merge the static fallback",
  );
});

Deno.test("model discovery uses the dynamic catalog as the sole source of truth", async () => {
  const models = await antigravityModels.list(
    { resource: account() },
    context(() => ({
      status: 200,
      headers: {},
      body: JSON.stringify(dynamicPayload),
    })),
  );
  assertEquals(models.map((model) => model.id), ["gemini-3.8-flash"]);
});

Deno.test("model discovery surfaces an authorization failure instead of the static catalog", async () => {
  let rejected = false;
  try {
    await antigravityModels.list(
      { resource: account() },
      context(() => ({
        status: 401,
        headers: {},
        body: '{"error":"invalid_token"}',
      })),
    );
  } catch (error) {
    rejected = true;
    assert(String(error).includes("sign in again"), "expected a re-authentication hint");
  }
  assert(rejected, "an unauthorized account must not resolve with the static catalog");
});

Deno.test("model discovery keeps the static catalog for non-auth failures", async () => {
  const models = await antigravityModels.list(
    { resource: account() },
    context(() => {
      throw new Error("network unavailable");
    }),
  );
  assertEquals(models.map((model) => model.id), STATIC_ANTIGRAVITY_MODELS.map((model) => model.id));
});

Deno.test("model discovery requires an account before returning the static catalog", async () => {
  const models = await antigravityModels.list(
    { resource: null },
    context(() => {
      throw new Error("fetch was not expected");
    }),
  );
  assertEquals(models.map((model) => model.id), STATIC_ANTIGRAVITY_MODELS.map((model) => model.id));
});
