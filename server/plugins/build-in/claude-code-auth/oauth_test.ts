import type { NetworkResponse, PluginContext } from "cursor-byok:plugin";
import { CLAUDE_CALLBACK_PORT, claudeCodeOAuth } from "./oauth.ts";
import { CLAUDE_CLIENT_ID, CLAUDE_SCOPES, CLAUDE_TOKEN_URL } from "./resources.ts";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

type RequestInit = { method?: string; headers?: Record<string, string>; body?: string };

function context(
  handler: (url: string, init?: RequestInit) => NetworkResponse,
): PluginContext {
  return {
    network: {
      fetch: (url, init) => Promise.resolve(handler(url, init)),
      stream: () => Promise.reject(new Error("stream is not expected")),
    },
    signal: new AbortController().signal,
  };
}

Deno.test("authorization URL carries Claude's client id, scopes, callback, and Core PKCE", async () => {
  const result = await claudeCodeOAuth.begin(
    {
      redirectUri: "http://127.0.0.1:54545/callback",
      state: "core-state",
      codeChallenge: "core-challenge",
    },
    context(() => ({ status: 500, headers: {}, body: "{}" })),
  );
  const url = new URL(result.authorizationUrl);
  assert(
    url.origin + url.pathname === "https://claude.ai/oauth/authorize",
    "wrong authorize endpoint",
  );
  assert(url.searchParams.get("code") === "true", "Claude requires code=true");
  assert(url.searchParams.get("client_id") === CLAUDE_CLIENT_ID, "client id mismatch");
  assert(url.searchParams.get("response_type") === "code", "response type must be code");
  assert(url.searchParams.get("scope") === CLAUDE_SCOPES, "scope mismatch");
  assert(url.searchParams.get("state") === "core-state", "state must come from Core");
  assert(
    url.searchParams.get("redirect_uri") === "http://127.0.0.1:54545/callback",
    "callback must be forwarded",
  );
  assert(url.searchParams.get("code_challenge") === "core-challenge", "PKCE challenge missing");
  assert(url.searchParams.get("code_challenge_method") === "S256", "PKCE must use S256");
  assert(claudeCodeOAuth.callback?.port === CLAUDE_CALLBACK_PORT, "fixed callback port missing");
  assert(claudeCodeOAuth.callback?.path === "/callback", "callback path mismatch");
  assert(claudeCodeOAuth.callback?.host === "localhost", "callback host mismatch");
});

Deno.test("completion exchanges the code with the Core verifier and reads the OAuth profile", async () => {
  type RecordedRequest = { url: string; init?: RequestInit };
  const requests: RecordedRequest[] = [];
  const resources = await claudeCodeOAuth.complete(
    { state: "core-state", createdAtMs: Date.now() },
    {
      code: "authorization-code#fragment-state",
      redirectUri: "http://127.0.0.1:54545/callback",
      codeVerifier: "core-verifier",
    },
    context((url, init) => {
      requests.push({ url, init });
      if (url === CLAUDE_TOKEN_URL) {
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            access_token: "access-token",
            refresh_token: "refresh-token",
            expires_in: 28800,
            account: { uuid: "token-account", email_address: "fallback@example.com" },
            organization: { uuid: "org-1", name: "Fallback Org" },
          }),
        };
      }
      if (url === "https://api.anthropic.com/api/oauth/profile") {
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            account: { uuid: "account-1", email: "person@example.com" },
            organization: { uuid: "org-1", name: "Example Org" },
          }),
        };
      }
      return { status: 500, headers: {}, body: "{}" };
    }),
  );

  const tokenRequest = requests.find((request) => request.url === CLAUDE_TOKEN_URL);
  assert(tokenRequest, "token endpoint must be called");
  assert(
    tokenRequest.init?.headers?.["content-type"] === "application/json",
    "Claude token exchange uses a JSON body",
  );
  const body = JSON.parse(tokenRequest.init?.body ?? "{}") as Record<string, unknown>;
  assert(body.grant_type === "authorization_code", "wrong grant type");
  assert(body.code === "authorization-code", "code must be stripped of the state fragment");
  assert(body.state === "fragment-state", "fragment state must take precedence");
  assert(body.code_verifier === "core-verifier", "PKCE verifier must come from Core");
  assert(body.redirect_uri === "http://127.0.0.1:54545/callback", "redirect uri mismatch");
  assert(body.client_id === CLAUDE_CLIENT_ID, "client id mismatch");

  const profileRequest = requests.find((request) =>
    request.url === "https://api.anthropic.com/api/oauth/profile"
  );
  assert(profileRequest, "profile endpoint must be called");
  assert(
    profileRequest.init?.headers?.authorization === "Bearer access-token",
    "profile must use the OAuth bearer token",
  );

  assert(resources.length === 1, "one Claude account resource must be returned");
  assert(resources[0].key === "claude-code:account-1", "account uuid must be the key");
  const data = resources[0].privateData as Record<string, unknown>;
  assert(data.accessToken === "access-token", "access token missing");
  assert(data.refreshToken === "refresh-token", "refresh token missing");
  assert(data.displayName === "person@example.com", "profile email should win");
  assert(data.organizationName === "Example Org", "organization name missing");
  assert(typeof data.expiresAtMs === "number", "expiry must be persisted");
});

Deno.test("expired authorization sessions are rejected before the token exchange", async () => {
  let calls = 0;
  try {
    await claudeCodeOAuth.complete(
      { state: "core-state", createdAtMs: Date.now() - 11 * 60 * 1000 },
      { code: "code", redirectUri: "http://127.0.0.1:54545/callback", codeVerifier: "v" },
      context(() => {
        calls += 1;
        return { status: 200, headers: {}, body: "{}" };
      }),
    );
    throw new Error("expired session must not complete");
  } catch (error) {
    assert(
      error instanceof Error && error.message.includes("expired"),
      "expired session should report an explicit error",
    );
  }
  assert(calls === 0, "no network call should happen for an expired session");
});
