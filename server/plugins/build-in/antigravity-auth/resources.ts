import type { JsonValue, NetworkRequestInit, PluginContext } from "cursor-byok:plugin";
import type {
  ResourceDraft,
  ResourceImportFile,
  ResourceImportResult,
  ResourceImportSupport,
  ResourceMetric,
  ResourcePatch,
  ResourceSnapshot,
  ResourceState,
  ResourceView,
} from "cursor-byok:resource";
import {
  ANTIGRAVITY_CLIENT_HEADERS,
  ANTIGRAVITY_ENDPOINTS,
  ANTIGRAVITY_USER_AGENT,
} from "./models.ts";
import { CLIENT_ID, CLIENT_SECRET } from "./google_oauth.ts";

export const RESOURCE_TYPE = "antigravity-account";

const REFRESH_TOKEN_URL = "https://oauth2.googleapis.com/token";
/** 令牌端点明确返回 429/5xx 时最多重试 2 次;传输层错误不重试。 */
const MAX_REFRESH_RETRIES = 2;
const REFRESH_RETRY_BASE_MS = 400;
export const AUTH_EXPIRED_MESSAGE = "Google authorization expired; sign in again";

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function fetchText(
  network: PluginContext["network"] | undefined,
  url: string,
  init: NetworkRequestInit,
): Promise<{ status: number; body: string }> {
  if (network) {
    const response = await network.fetch(url, init);
    return { status: response.status, body: response.body };
  }
  const response = await fetch(url, init);
  return { status: response.status, body: await response.text() };
}

export type QuotaMetric = {
  remainingPercent: number;
  resetAtMs: number | null;
};

export type AccountQuota = {
  planLabel: string | null;
  limitReached: boolean;
  coolingUntilMs: number | null;
  updatedAtMs: number;
  claude?: QuotaMetric | null;
  gemini?: QuotaMetric | null;
};

export type AccountData = {
  accessToken: string;
  refreshToken: string | null;
  displayName: string;
  projectId?: string | null;
  expiresAtMs?: number | null;
  quota: AccountQuota | null;
};

export type CredentialCandidate = {
  accessToken: string;
  refreshToken: string | null;
  displayName: string | null;
  projectId?: string | null;
  expiresAtMs?: number | null;
  quota?: AccountQuota | null;
};

export async function fetchAccountProjectAndTier(
  accessToken: string,
  network: PluginContext["network"],
): Promise<{ projectId: string; planLabel: string }> {
  for (const endpoint of ANTIGRAVITY_ENDPOINTS) {
    try {
      const assistRes = await network.fetch(`${endpoint}/v1internal:loadCodeAssist`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${accessToken}`,
          "content-type": "application/json",
          "user-agent": ANTIGRAVITY_USER_AGENT,
          ...ANTIGRAVITY_CLIENT_HEADERS,
        },
        body: JSON.stringify({ metadata: { ideType: "ANTIGRAVITY" } }),
      });
      if (assistRes.status >= 200 && assistRes.status < 300) {
        const body = object(JSON.parse(assistRes.body));
        const project = text(body?.cloudaicompanionProject);
        const paid = object(body?.paidTier);
        const current = object(body?.currentTier);
        const tierName = text(paid?.name) ?? text(paid?.id) ?? text(current?.name) ??
          text(current?.id);
        let planLabel = "FREE";
        if (tierName) {
          const lower = tierName.toLowerCase();
          if (lower.includes("ultra")) planLabel = "ULTRA";
          else if (
            lower.includes("pro") || lower.includes("premium") || lower.includes("advanced")
          ) planLabel = "PRO";
        }
        return { projectId: project ?? "bamboo-precept-lgxtn", planLabel };
      }
    } catch {
      // Continue next endpoint
    }
  }
  return { projectId: "bamboo-precept-lgxtn", planLabel: "FREE" };
}

export async function queryAccountQuota(
  accessToken: string,
  network: PluginContext["network"],
): Promise<{ quota: AccountQuota | null; projectId: string }> {
  const { projectId, planLabel } = await fetchAccountProjectAndTier(accessToken, network);

  for (const endpoint of ANTIGRAVITY_ENDPOINTS) {
    try {
      const response = await network.fetch(`${endpoint}/v1internal:fetchAvailableModels`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${accessToken}`,
          "content-type": "application/json",
          accept: "application/json",
          "user-agent": ANTIGRAVITY_USER_AGENT,
          ...ANTIGRAVITY_CLIENT_HEADERS,
        },
        body: JSON.stringify({ project: projectId }),
      });
      if (response.status < 200 || response.status >= 300) continue;
      const root = object(JSON.parse(response.body));
      const models = object(root?.models);
      if (!models) continue;

      let claudeFraction: number | null = null;
      let claudeResetAtMs: number | null = null;
      let geminiFraction: number | null = null;
      let geminiResetAtMs: number | null = null;

      for (const [key, value] of Object.entries(models)) {
        const info = object(value);
        const quota = object(info?.quotaInfo);
        const fraction = typeof quota?.remainingFraction === "number"
          ? quota.remainingFraction
          : null;
        const resetTime = text(quota?.resetTime);
        const resetAtMs = resetTime ? Date.parse(resetTime) : null;
        if (fraction === null) continue;

        const k = key.toLowerCase();
        if (k.includes("claude") || k.includes("sonnet") || k.includes("opus")) {
          if (claudeFraction === null || fraction < claudeFraction) {
            claudeFraction = fraction;
            claudeResetAtMs = resetAtMs;
          }
        } else if (k.includes("gemini") || k.includes("flash") || k.includes("pro")) {
          if (geminiFraction === null || fraction < geminiFraction) {
            geminiFraction = fraction;
            geminiResetAtMs = resetAtMs;
          }
        }
      }

      return {
        projectId,
        quota: {
          planLabel,
          limitReached: false,
          coolingUntilMs: null,
          updatedAtMs: Date.now(),
          claude: claudeFraction !== null
            ? { remainingPercent: Math.round(claudeFraction * 100), resetAtMs: claudeResetAtMs }
            : null,
          gemini: geminiFraction !== null
            ? { remainingPercent: Math.round(geminiFraction * 100), resetAtMs: geminiResetAtMs }
            : null,
        },
      };
    } catch {
      // Continue next endpoint
    }
  }

  return {
    projectId,
    quota: {
      planLabel,
      limitReached: false,
      coolingUntilMs: null,
      updatedAtMs: Date.now(),
      claude: null,
      gemini: null,
    },
  };
}

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function decodeJwtPayload(token: string): Record<string, unknown> | null {
  const parts = token.split(".");
  if (parts.length < 2) return null;
  try {
    const normalized = parts[1].replace(/-/g, "+").replace(/_/g, "/");
    const padded = normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "=");
    const bytes = Uint8Array.from(atob(padded), (char) => char.charCodeAt(0));
    return object(JSON.parse(new TextDecoder().decode(bytes)));
  } catch {
    return null;
  }
}

function claim(payload: Record<string, unknown> | null, key: string): string | null {
  return payload ? text(payload[key]) : null;
}

export function isJwtExpired(token: string, bufferSeconds = 300): boolean {
  if (!token.includes(".")) return false;
  const payload = decodeJwtPayload(token);
  if (!payload) return false;
  const exp = typeof payload.exp === "number" ? payload.exp : null;
  if (!exp) return false;
  const nowSeconds = Math.floor(Date.now() / 1000);
  return exp <= (nowSeconds + bufferSeconds);
}

export function isTokenExpired(data: AccountData, bufferSeconds = 300): boolean {
  if (!data.refreshToken) return false;
  if (typeof data.expiresAtMs === "number" && data.expiresAtMs > 0) {
    return Date.now() >= data.expiresAtMs - bufferSeconds * 1000;
  }
  return isJwtExpired(data.accessToken, bufferSeconds);
}

async function tokenFingerprint(token: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(token));
  return Array.from(
    new Uint8Array(digest).slice(0, 8),
    (byte) => byte.toString(16).padStart(2, "0"),
  ).join("");
}

export async function accountIdentity(
  token: string,
  providedDisplayName?: string | null,
): Promise<{ key: string; displayName: string }> {
  const payload = decodeJwtPayload(token);
  const email = claim(payload, "email");
  const sub = claim(payload, "sub");
  const name = claim(payload, "name") ?? claim(payload, "preferred_username");

  const fingerprint = await tokenFingerprint(token);
  const identity = (providedDisplayName && !providedDisplayName.includes("Antigravity"))
    ? providedDisplayName
    : (email ?? sub ?? fingerprint);
  const displayName = providedDisplayName ?? email ?? name ?? identity;
  return { key: `antigravity:${identity}`, displayName };
}

export async function credentialDraft(credential: CredentialCandidate): Promise<ResourceDraft> {
  const identity = await accountIdentity(credential.accessToken, credential.displayName);
  const data: AccountData = {
    accessToken: credential.accessToken,
    refreshToken: credential.refreshToken,
    displayName: credential.displayName ?? identity.displayName,
    projectId: credential.projectId ?? "bamboo-precept-lgxtn",
    expiresAtMs: credential.expiresAtMs ??
      (credential.refreshToken ? Date.now() + 3500 * 1000 : null),
    quota: credential.quota ?? null,
  };
  return { key: identity.key, privateData: data as unknown as JsonValue };
}

export function accountData(resource: ResourceSnapshot): AccountData {
  const data = object(resource.privateData);
  const accessToken = text(data?.accessToken);
  if (!accessToken) throw new Error("Antigravity account resource is missing its access token");
  return {
    accessToken,
    refreshToken: text(data?.refreshToken),
    displayName: text(data?.displayName) ?? "Antigravity account",
    projectId: text(data?.projectId) ?? "bamboo-precept-lgxtn",
    expiresAtMs: typeof data?.expiresAtMs === "number" ? data.expiresAtMs : null,
    quota: (data?.quota ?? null) as AccountQuota | null,
  };
}

export function accountHeaders(data: AccountData): Record<string, string> {
  return {
    authorization: `Bearer ${data.accessToken}`,
    accept: "application/json",
    "user-agent": ANTIGRAVITY_USER_AGENT,
    ...ANTIGRAVITY_CLIENT_HEADERS,
  };
}

export function quotaState(quota: AccountQuota | null, nowMs = Date.now()): ResourceState {
  if (!quota || !quota.limitReached) return { status: "ready" };
  const coolingUntil = quota.coolingUntilMs;
  if (coolingUntil !== null && coolingUntil > nowMs) {
    return {
      status: "cooling",
      retryAtMs: coolingUntil,
      message: "Antigravity rate limit reached; cooling down",
    };
  }
  return { status: "ready" };
}

export function quotaExhaustedPatch(
  data: AccountData,
  error?: string,
  nowMs = Date.now(),
): ResourcePatch {
  let retryAfterMs = 60 * 1000;
  if (error) {
    const match = error.match(/retry(?:_after|\s+after)?\s*[:=]?\s*(\d+)/i);
    if (match?.[1]) {
      const parsed = Number(match[1]);
      if (Number.isFinite(parsed) && parsed > 0) {
        retryAfterMs = parsed > 10_000_000 ? parsed - nowMs : parsed * 1000;
      }
    }
  }
  const coolingUntilMs = nowMs + Math.max(5000, retryAfterMs);
  const quota: AccountQuota = {
    planLabel: data.quota?.planLabel ?? "Antigravity / Gemini",
    limitReached: true,
    coolingUntilMs,
    updatedAtMs: nowMs,
    claude: data.quota?.claude ?? null,
    gemini: data.quota?.gemini ?? null,
  };
  return {
    privateData: { ...data, quota } as unknown as JsonValue,
    state: quotaState(quota, nowMs),
  };
}

export function presentAccount(resource: ResourceSnapshot): ResourceView {
  const data = accountData(resource);
  const metrics: ResourceMetric[] = [];
  if (data.quota?.claude) {
    metrics.push({
      id: "claude",
      label: { "en-US": "Claude", "zh-CN": "Claude" },
      unit: "percent",
      value: data.quota.claude.remainingPercent,
      ...(data.quota.claude.resetAtMs ? { resetAtMs: data.quota.claude.resetAtMs } : {}),
    });
  }
  if (data.quota?.gemini) {
    metrics.push({
      id: "gemini",
      label: { "en-US": "Gemini", "zh-CN": "Gemini" },
      unit: "percent",
      value: data.quota.gemini.remainingPercent,
      ...(data.quota.gemini.resetAtMs ? { resetAtMs: data.quota.gemini.resetAtMs } : {}),
    });
  }
  return {
    displayName: data.displayName,
    ...(data.quota?.planLabel ? { description: data.quota.planLabel } : {}),
    ...(metrics.length > 0 ? { metrics } : {}),
  };
}

export type CredentialRefresh =
  | { status: "refreshed"; data: AccountData }
  | { status: "invalid"; message: string }
  | { status: "failed"; message: string };

/**
 * 用 refresh_token 换新的 access token。只有上游明确 invalid_grant /
 * unauthorized_client 才判定凭证失效;网络或上游临时故障返回 failed,
 * 调用方不得据此把账号标成 invalid。429/5xx 是明确的拒绝响应,按有限次数重试;
 * 传输层错误不重试,因为请求可能已被上游消费。
 */
async function refreshCredential(
  data: AccountData,
  context: PluginContext,
): Promise<CredentialRefresh> {
  const refreshToken = data.refreshToken;
  if (!refreshToken) return { status: "invalid", message: AUTH_EXPIRED_MESSAGE };

  for (let attempt = 0;; attempt += 1) {
    let response;
    try {
      response = await context.network.fetch(REFRESH_TOKEN_URL, {
        method: "POST",
        headers: {
          accept: "application/json",
          "content-type": "application/x-www-form-urlencoded",
        },
        body: new URLSearchParams({
          client_id: CLIENT_ID,
          client_secret: CLIENT_SECRET,
          grant_type: "refresh_token",
          refresh_token: refreshToken,
        }).toString(),
      });
    } catch (error) {
      return {
        status: "failed",
        message: `Google token refresh failed: ${
          error instanceof Error ? error.message : String(error)
        }`,
      };
    }

    if (response.status < 200 || response.status >= 300) {
      const bodyText = response.body.toLowerCase();
      if (
        response.status === 401 ||
        bodyText.includes("invalid_grant") ||
        bodyText.includes("invalid_token") ||
        bodyText.includes("unauthorized_client")
      ) {
        return { status: "invalid", message: AUTH_EXPIRED_MESSAGE };
      }
      if (
        (response.status === 429 || response.status >= 500) && attempt < MAX_REFRESH_RETRIES
      ) {
        await sleep(REFRESH_RETRY_BASE_MS * 2 ** attempt);
        continue;
      }
      // 其它失败不能继续用旧 token,也不能谎报 ready,而是抛出可诊断错误让调用方中止。
      return {
        status: "failed",
        message:
          `Google token refresh failed (HTTP ${response.status}); the stale access token was not used: ${
            response.body.trim().slice(0, 200)
          }`,
      };
    }

    let body: Record<string, unknown> | null;
    try {
      body = object(JSON.parse(response.body));
    } catch {
      return { status: "failed", message: "Google token refresh returned invalid JSON" };
    }
    const accessToken = text(body?.access_token);
    if (!accessToken) {
      return {
        status: "failed",
        message: "Google token refresh response did not include an access token",
      };
    }
    const expiresIn = typeof body?.expires_in === "number" ? body.expires_in : 3600;
    return {
      status: "refreshed",
      data: {
        ...data,
        accessToken,
        refreshToken: text(body?.refresh_token) ?? refreshToken,
        expiresAtMs: Date.now() + expiresIn * 1000,
      },
    };
  }
}

/**
 * 同一账号的并发刷新共用一个 Promise。refresh token 会轮换：并发各发一次时，
 * 后到的一方拿着已经作废的旧值换回 invalid_grant，正常账号会被误标成 invalid。
 * 成功与失败都共享同一个结果，各调用方只负责回写。
 */
const refreshFlights = new Map<string, Promise<CredentialRefresh>>();

function refreshCredentialForAccount(
  accountKey: string,
  data: AccountData,
  context: PluginContext,
): Promise<CredentialRefresh> {
  const inflight = refreshFlights.get(accountKey);
  if (inflight) return inflight;
  const operation = refreshCredential(data, context).finally(() => {
    if (refreshFlights.get(accountKey) === operation) refreshFlights.delete(accountKey);
  });
  refreshFlights.set(accountKey, operation);
  return operation;
}

/** 只给测试用：丢弃进程内刷新记录，避免用例之间互相影响。 */
export function resetRefreshStateForTests(): void {
  refreshFlights.clear();
}

export async function refreshAccount(
  resource: ResourceSnapshot,
  context: PluginContext,
): Promise<ResourcePatch> {
  const data = accountData(resource);
  let refreshed = data;

  if (data.refreshToken) {
    const result = await refreshCredentialForAccount(resource.key, data, context);
    if (result.status === "invalid") {
      return { state: { status: "invalid", message: result.message } };
    }
    if (result.status === "failed") throw new Error(result.message);
    refreshed = result.data;
  }

  // Fetch real-time quota and project ID
  const result = await queryAccountQuota(refreshed.accessToken, context.network);

  const updatedData: AccountData = {
    ...refreshed,
    projectId: result.projectId || refreshed.projectId || "bamboo-precept-lgxtn",
    quota: result.quota ?? refreshed.quota,
  };
  return {
    privateData: updatedData as unknown as JsonValue,
    state: { status: "ready" },
  };
}

function firstText(source: Record<string, unknown>, keys: string[]): string | null {
  for (const key of keys) {
    const value = text(source[key]);
    if (value) return value;
  }
  return null;
}

function collectCredentials(value: unknown, output: CredentialCandidate[]): void {
  if (Array.isArray(value)) {
    for (const item of value) collectCredentials(item, output);
    return;
  }
  const item = object(value);
  if (!item || item.disabled === true) return;
  for (const key of ["accounts", "credentials", "items", "keys"]) {
    if (Array.isArray(item[key])) {
      collectCredentials(item[key], output);
      return;
    }
  }
  const tokens = object(item.tokens) ?? item;
  // Antigravity 只接受 Google OAuth access token(Bearer)。`AIza…` API key 不是
  // 可用凭证,按 Bearer 发送必然 401,所以导入路径不再收集任何 API key 字段。
  let accessToken = firstText(tokens, [
    "access",
    "accessToken",
    "access_token",
    "token",
  ]) ?? firstText(item, [
    "access",
    "accessToken",
    "access_token",
    "token",
  ]);
  const refreshToken = firstText(tokens, ["refresh", "refresh_token", "refreshToken"]) ??
    firstText(item, ["refresh", "refresh_token", "refreshToken"]);
  const displayName = firstText(item, ["email", "display_name", "displayName", "name"]) ??
    firstText(tokens, ["email", "display_name", "displayName", "name"]);
  const projectId =
    firstText(item, ["project", "projectId", "project_id", "cloudaicompanionProject"]) ??
      firstText(tokens, ["project", "projectId", "project_id", "cloudaicompanionProject"]);

  if (!accessToken && !refreshToken) return;
  if (!accessToken && refreshToken) {
    accessToken = refreshToken;
  }
  if (!accessToken) return;
  output.push({ accessToken, refreshToken, displayName, projectId });
}

/** Google OAuth access token 的三段式 JWT 形状;AIza API key 不匹配。 */
function isOAuthToken(value: string): boolean {
  return /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/.test(value);
}

export async function parseCredentialFiles(
  files: ResourceImportFile[],
  network?: PluginContext["network"],
): Promise<{
  credentials: CredentialCandidate[];
  warnings: string[];
}> {
  const credentials: CredentialCandidate[] = [];
  const warnings: string[] = [];
  for (const file of files) {
    const raw = file.content.trim();
    if (!raw) continue;

    // 原始文件只接受 OAuth token 本身;`AIza…` API key 按 Bearer 用必然失败,
    // 因此直接拒绝而不是造一条永远 invalid 的资源。
    if (isOAuthToken(raw)) {
      credentials.push({ accessToken: raw, refreshToken: null, displayName: file.name });
      continue;
    }

    // Try parsing as JSON
    let content: unknown;
    try {
      content = JSON.parse(raw);
    } catch {
      const envMatch = raw.match(
        /(ACCESS_TOKEN|REFRESH_TOKEN|TOKEN)\s*=\s*["']?([^"'\r\n]+)/i,
      );
      const envValue = envMatch?.[2]?.trim() ?? null;
      if (envMatch?.[1] && envValue && isOAuthToken(envValue)) {
        const isRefresh = envMatch[1].toLowerCase() === "refresh_token";
        credentials.push({
          accessToken: envValue,
          refreshToken: isRefresh ? envValue : null,
          displayName: file.name,
        });
        continue;
      }
      warnings.push(`${file.name}: not valid JSON or Google OAuth token`);
      continue;
    }

    if (typeof content === "string") {
      const value = content.trim();
      if (!isOAuthToken(value)) {
        warnings.push(`${file.name}: not a Google OAuth access token`);
        continue;
      }
      credentials.push({ accessToken: value, refreshToken: null, displayName: file.name });
      continue;
    }

    const found: CredentialCandidate[] = [];
    collectCredentials(content, found);
    if (found.length === 0) {
      warnings.push(`${file.name}: no Google OAuth token found`);
      continue;
    }
    for (const candidate of found) {
      if (candidate.refreshToken && candidate.accessToken === candidate.refreshToken) {
        try {
          const response = await fetchText(network, REFRESH_TOKEN_URL, {
            method: "POST",
            headers: {
              accept: "application/json",
              "content-type": "application/x-www-form-urlencoded",
            },
            body: new URLSearchParams({
              client_id: CLIENT_ID,
              client_secret: CLIENT_SECRET,
              grant_type: "refresh_token",
              refresh_token: candidate.refreshToken,
            }).toString(),
          });
          const body = object(JSON.parse(response.body));
          if (response.status >= 200 && response.status < 300 && text(body?.access_token)) {
            candidate.accessToken = text(body?.access_token)!;
            candidate.refreshToken = text(body?.refresh_token) ?? candidate.refreshToken;
            candidate.expiresAtMs = Date.now() +
              ((typeof body?.expires_in === "number" ? body.expires_in : 3600) * 1000);
          }
        } catch {
          // Keep placeholder
        }
      }
      credentials.push(candidate);
    }
  }
  return { credentials, warnings };
}

export const credentialImport: ResourceImportSupport = {
  displayName: {
    "en-US": "Import Google / Antigravity Credentials",
    "zh-CN": "导入 Google / Antigravity 凭证",
  },
  description: {
    "en-US": "Import a JSON, TXT, or environment file containing Google OAuth tokens.",
    "zh-CN": "导入包含 Google OAuth Token 的 JSON、TXT 或环境变量文件。",
  },
  accept: [".json", ".txt", ".env"],
  multiple: true,
  parse: async (
    files: ResourceImportFile[],
    context: PluginContext,
  ): Promise<ResourceImportResult> => {
    const { credentials, warnings } = await parseCredentialFiles(files, context.network);
    if (credentials.length === 0) {
      throw new Error(
        warnings.join("; ") ||
          "credential file does not contain a Google OAuth token",
      );
    }
    const drafts = await Promise.all(
      credentials.map(async (c) => {
        try {
          const res = await queryAccountQuota(c.accessToken, context.network);
          c.quota = res.quota;
          c.projectId = res.projectId;
        } catch {
          // ignore error
        }
        return credentialDraft(c);
      }),
    );
    return {
      resources: drafts,
      ...(warnings.length > 0 ? { warnings } : {}),
    };
  },
};
