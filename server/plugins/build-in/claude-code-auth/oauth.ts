import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import type {
  OAuth2AuthorizationCodeAddMethod,
  OAuth2AuthorizationCodeBegin,
  ResourceDraft,
} from "cursor-byok:resource";
import {
  CLAUDE_CLIENT_ID,
  CLAUDE_PROFILE_URL,
  CLAUDE_SCOPES,
  CLAUDE_TOKEN_URL,
  CLAUDE_USER_AGENT,
  credentialDraft,
} from "./resources.ts";

export const CLAUDE_AUTHORIZE_URL = "https://claude.ai/oauth/authorize";
/**
 * Claude Code 的固定回环回调端口；见 docs/claude-account.md 的“回调地址”一节。
 * 上游登记的地址是 `localhost`，所以这里显式声明 host，宿主据此生成
 * `http://localhost:54545/callback` 并仍绑定在 127.0.0.1。
 */
export const CLAUDE_CALLBACK_PORT = 54545;
export const CLAUDE_CALLBACK_PATH = "/callback";
export const CLAUDE_CALLBACK_HOST = "localhost";

const AUTHORIZATION_LIFETIME_MS = 10 * 60 * 1000;

type Session = {
  state: string;
  createdAtMs: number;
};

type Profile = {
  email: string | null;
  accountUuid: string | null;
  organizationName: string | null;
};

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function number(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string" && value.trim()) {
    const parsed = Number(value);
    return Number.isFinite(parsed) ? parsed : null;
  }
  return null;
}

function parseBody(body: string): Record<string, unknown> {
  try {
    return object(JSON.parse(body)) ?? {};
  } catch {
    return {};
  }
}

function parseSession(value: JsonValue): Session {
  const session = object(value);
  const state = text(session?.state);
  const createdAtMs = session?.createdAtMs;
  if (!state || typeof createdAtMs !== "number") {
    throw new Error("Claude OAuth session is invalid");
  }
  if (Date.now() - createdAtMs > AUTHORIZATION_LIFETIME_MS) {
    throw new Error("Claude authorization expired. Please try again.");
  }
  return { state, createdAtMs };
}

/**
 * 授权码可能带 `#state` 片段（Claude 的网页回调会附加它）。回环回调通常没有，
 * 但保留这个分支可以同时兼容两种返回形式。
 */
function splitCode(raw: string): { code: string; state: string | null } {
  const index = raw.indexOf("#");
  if (index < 0) return { code: raw.trim(), state: null };
  return {
    code: raw.slice(0, index).trim(),
    state: text(raw.slice(index + 1)),
  };
}

function begin(
  input: { redirectUri: string; state: string; codeChallenge: string },
  _context: PluginContext,
): Promise<OAuth2AuthorizationCodeBegin> {
  const params = new URLSearchParams({
    code: "true",
    client_id: CLAUDE_CLIENT_ID,
    response_type: "code",
    redirect_uri: input.redirectUri,
    scope: CLAUDE_SCOPES,
    code_challenge: input.codeChallenge,
    code_challenge_method: "S256",
    state: input.state,
  });
  return Promise.resolve({
    session: { state: input.state, createdAtMs: Date.now() } as unknown as JsonValue,
    authorizationUrl: `${CLAUDE_AUTHORIZE_URL}?${params.toString()}`,
    expiresAtMs: Date.now() + AUTHORIZATION_LIFETIME_MS,
    pollIntervalMs: 1000,
  });
}

async function fetchProfile(accessToken: string, context: PluginContext): Promise<Profile | null> {
  try {
    const response = await context.network.fetch(CLAUDE_PROFILE_URL, {
      method: "GET",
      headers: {
        authorization: `Bearer ${accessToken}`,
        "anthropic-version": "2023-06-01",
        "anthropic-beta": "oauth-2025-04-20",
        accept: "application/json",
        "user-agent": CLAUDE_USER_AGENT,
      },
    });
    if (response.status < 200 || response.status >= 300) return null;
    const root = parseBody(response.body);
    const account = object(root.account);
    const organization = object(root.organization);
    return {
      email: text(account?.email),
      accountUuid: text(account?.uuid),
      organizationName: text(organization?.name),
    };
  } catch {
    // 头像/显示名是可选信息；拿不到时退回令牌响应里的账号字段或令牌指纹。
    return null;
  }
}

async function complete(
  sessionValue: JsonValue,
  input: { code: string; redirectUri: string; codeVerifier: string },
  context: PluginContext,
): Promise<ResourceDraft[]> {
  const session = parseSession(sessionValue);
  const parsed = splitCode(input.code);
  const response = await context.network.fetch(CLAUDE_TOKEN_URL, {
    method: "POST",
    headers: {
      accept: "application/json",
      "content-type": "application/json",
      "user-agent": CLAUDE_USER_AGENT,
    },
    body: JSON.stringify({
      grant_type: "authorization_code",
      code: parsed.code,
      redirect_uri: input.redirectUri,
      client_id: CLAUDE_CLIENT_ID,
      code_verifier: input.codeVerifier,
      state: parsed.state ?? session.state,
    }),
  });
  const body = parseBody(response.body);
  if (response.status < 200 || response.status >= 300) {
    const detail = text(body.error_description) ?? text(body.error) ?? `HTTP ${response.status}`;
    throw new Error(`Claude token exchange failed: ${detail}`);
  }

  const accessToken = text(body.access_token);
  if (!accessToken) throw new Error("Claude token response did not include an access token");

  const profile = await fetchProfile(accessToken, context);
  const account = object(body.account);
  const organization = object(body.organization);
  const email = profile?.email ?? text(account?.email_address);
  const accountUuid = profile?.accountUuid ?? text(account?.uuid);
  const organizationName = profile?.organizationName ?? text(organization?.name);
  const expiresIn = number(body.expires_in);

  return [
    await credentialDraft({
      accessToken,
      refreshToken: text(body.refresh_token),
      displayName: email,
      accountUuid,
      organizationName,
      expiresAtMs: expiresIn === null ? null : Date.now() + expiresIn * 1000,
    }),
  ];
}

export const claudeCodeOAuth: OAuth2AuthorizationCodeAddMethod = {
  type: "oauth2.authorization-code",
  id: "claude-code-subscription",
  displayName: {
    "en-US": "Sign in with Claude subscription",
    "zh-CN": "使用 Claude 订阅登录",
    "ru-RU": "Войти через подписку Claude",
  },
  description: {
    "en-US":
      "Authorize with a Claude Pro/Max subscription. This may violate Anthropic's terms and lead to rate limits or account suspension; credentials are stored locally. Prefer an API key.",
    "zh-CN":
      "使用 Claude Pro/Max 订阅完成 OAuth 授权。这可能违反 Anthropic 服务条款，并导致限流或封号；凭据保存在本机。建议优先使用 API Key。",
    "ru-RU":
      "Авторизация через подписку Claude Pro/Max. Это может нарушать условия Anthropic и привести к ограничениям или блокировке аккаунта; учётные данные хранятся локально. Предпочтительнее API-ключ.",
  },
  callback: {
    host: CLAUDE_CALLBACK_HOST,
    port: CLAUDE_CALLBACK_PORT,
    path: CLAUDE_CALLBACK_PATH,
  },
  begin,
  complete,
};
