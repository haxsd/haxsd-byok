import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import type { ResourceSnapshot } from "cursor-byok:resource";
import {
  AUTH_EXPIRED_MESSAGE,
  parseCredentialFiles,
  refreshAccount,
  RESOURCE_TYPE,
} from "./resources.ts";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

function context(requests: string[]): PluginContext["network"] {
  return {
    fetch: (url, init = {}) => {
      requests.push(`${url}:${init.body ?? ""}`);
      return Promise.resolve({
        status: 200,
        headers: {},
        body: JSON.stringify({
          access_token: "access-token",
          refresh_token: "refresh-token",
          expires_in: 3600,
        }),
      });
    },
    stream: () => Promise.reject(new Error("stream is not expected")),
  };
}

function pluginContext(network: PluginContext["network"]): PluginContext {
  return { network, signal: new AbortController().signal };
}

function snapshot(privateData: JsonValue): ResourceSnapshot {
  return {
    id: "resource-1",
    type: RESOURCE_TYPE,
    key: "antigravity:user@example.com",
    privateData,
    state: { status: "ready" },
  };
}

Deno.test("imports an array of email and refresh_token credentials", async () => {
  const requests: string[] = [];
  const result = await parseCredentialFiles([
    {
      name: "antigravity.json",
      content: JSON.stringify([
        { email: "teocougar@gmail.com", refresh_token: "refresh-token" },
      ]),
    },
  ], context(requests));

  assert(result.warnings.length === 0, "the credential file should not produce warnings");
  assert(result.credentials.length === 1, "one credential should be imported");
  assert(
    result.credentials[0].displayName === "teocougar@gmail.com",
    "email should be used as display name",
  );
  assert(
    result.credentials[0].accessToken === "access-token",
    "refresh token should be exchanged for an access token",
  );
  assert(
    result.credentials[0].refreshToken === "refresh-token",
    "refresh token should be preserved",
  );
  assert(requests.length === 1, "the refresh token should be exchanged once");
});

Deno.test("Google API keys are rejected instead of imported as bearer tokens", async () => {
  const key = "AIzaSyA1234567890abcdefghijklmnopqrstuv";
  const result = await parseCredentialFiles([
    { name: "key.txt", content: key },
    { name: "service.json", content: JSON.stringify({ apiKey: key }) },
    { name: "vars.env", content: `GEMINI_API_KEY=${key}` },
  ]);
  assertEquals(result.credentials, []);
  assertEquals(result.warnings, [
    "key.txt: not valid JSON or Google OAuth token",
    "service.json: no Google OAuth token found",
    "vars.env: not valid JSON or Google OAuth token",
  ]);
});

Deno.test("a revoked refresh token marks the account invalid", async () => {
  const patch = await refreshAccount(
    snapshot({
      accessToken: "stale-access-token",
      refreshToken: "revoked-refresh",
      displayName: "Fixture",
      projectId: "test-project-123",
      quota: null,
    }),
    pluginContext({
      fetch: () =>
        Promise.resolve({
          status: 400,
          headers: {},
          body: JSON.stringify({ error: "invalid_grant", error_description: "revoked" }),
        }),
      stream: () => Promise.reject(new Error("stream is not expected")),
    }),
  );
  assertEquals(patch, { state: { status: "invalid", message: AUTH_EXPIRED_MESSAGE } });
});

Deno.test("a temporarily failed refresh raises instead of reporting ready", async () => {
  let rejected = false;
  try {
    await refreshAccount(
      snapshot({
        accessToken: "stale-access-token",
        refreshToken: "refresh-token",
        displayName: "Fixture",
        projectId: "test-project-123",
        quota: null,
      }),
      pluginContext({
        fetch: () => Promise.resolve({ status: 503, headers: {}, body: "unavailable" }),
        stream: () => Promise.reject(new Error("stream is not expected")),
      }),
    );
  } catch (error) {
    rejected = true;
    const message = String(error);
    assert(message.includes("HTTP 503"), `expected the status in ${message}`);
  }
  assert(rejected, "a failed refresh must not be reported as ready");
});

Deno.test("a successful refresh rotates the access token and keeps the refresh token", async () => {
  const patch = await refreshAccount(
    snapshot({
      accessToken: "stale-access-token",
      refreshToken: "refresh-token",
      displayName: "Fixture",
      projectId: "test-project-123",
      expiresAtMs: Date.now() - 1000,
      quota: null,
    }),
    pluginContext({
      fetch: (url, init) => {
        if (url === "https://oauth2.googleapis.com/token") {
          assertEquals(
            new URLSearchParams(init?.body).get("grant_type"),
            "refresh_token",
          );
          return Promise.resolve({
            status: 200,
            headers: {},
            body: JSON.stringify({ access_token: "fresh-access-token", expires_in: 3600 }),
          });
        }
        return Promise.resolve({ status: 500, headers: {}, body: "quota unavailable" });
      },
      stream: () => Promise.reject(new Error("stream is not expected")),
    }),
  );
  const data = patch.privateData as Record<string, unknown>;
  assertEquals(data.accessToken, "fresh-access-token");
  assertEquals(data.refreshToken, "refresh-token");
  assert(
    typeof data.expiresAtMs === "number" && data.expiresAtMs > Date.now(),
    "the new expiry must be persisted",
  );
  assertEquals(patch.state, { status: "ready" });
});
