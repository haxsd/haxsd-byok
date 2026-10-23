import type { JsonValue, NetworkResponse, PluginContext } from "cursor-byok:plugin";
import type {
  ResourceDraft,
  ResourceMetric,
  ResourcePatch,
  ResourceSnapshot,
  ResourceView,
} from "cursor-byok:resource";
import { redactSecrets, truncate } from "./redact.ts";
import {
  jitteredBackoffMs,
  MAX_CONCURRENT_REQUESTS,
  MAX_REFRESH_RETRIES,
  noteRateLimit,
  noteRefreshFailure,
  parseRetryAfterMs,
  REFRESH_RETRY_BASE_MS,
  runtimeSnapshot,
  singleFlight,
} from "./runtime.ts";

/**
 * Claude Code 订阅账号插件。
 *
 * OAuth 事实来源（2026-10-23 核实，见 docs/claude-account.md）：
 * - 授权端点 https://claude.ai/oauth/authorize
 * - 令牌交换/刷新端点 https://platform.claude.com/v1/oauth/token
 * - 客户端 ID 与 scope 来自 CLIProxyAPI 与 opencode-claude-auth 的公开实现；
 *   Claude Code 自身未公开这些常量，因此它们属于"社区核实"而非官方文档。
 * - 固定回环回调 http://localhost:54545/callback；宿主按 callback.host 生成
 *   redirect_uri，插件因此可以声明 localhost 而不是 127.0.0.1。
 */
export const RESOURCE_TYPE = "claude-code-account";
export const CLAUDE_CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
export const CLAUDE_TOKEN_URL = "https://platform.claude.com/v1/oauth/token";
export const CLAUDE_API_URL = "https://api.anthropic.com/v1/messages";
export const CLAUDE_PROFILE_URL = "https://api.anthropic.com/api/oauth/profile";
export const CLAUDE_SCOPES =
  "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/**
 * 不伪造 `claude-cli/*` User-Agent。社区代理为了通过 Anthropic 的客户端校验会伪装成
 * Claude Code，本插件不这么做；因此上游可能把请求归为第三方并拒绝。
 */
export const CLAUDE_USER_AGENT = "haxsd-byok-claude-code-auth/0.1.0";
export const AUTH_INVALID_MESSAGE = "Claude authorization expired or revoked; sign in again";

/** 提前 4 小时刷新：与 CLIProxyAPI 的做法一致，避免请求边界上刚好过期。 */
export const REFRESH_AHEAD_MS = 4 * 60 * 60 * 1000;

export type AccountData = {
  accessToken: string;
  refreshToken: string | null;
  displayName: string;
  accountUuid: string | null;
  organizationName: string | null;
  expiresAtMs: number | null;
  /** 最近一次 429 的时间；只用于资源面板展示，不参与鉴权。 */
  lastRateLimitAtMs: number | null;
};

export type CredentialCandidate = {
  accessToken: string;
  refreshToken: string | null;
  displayName?: string | null;
  accountUuid?: string | null;
  organizationName?: string | null;
  expiresAtMs?: number | null;
};

export type RefreshedTokens = {
  accessToken: string;
  refreshToken: string;
  expiresAtMs: number | null;
};

export type ClaudeAuthFailureKind = "invalid" | "rate-limit" | "transient";

/**
 * 刷新失败的分类：
 * - invalid：上游明确拒绝 refresh token（4xx / invalid_grant）→ 需要重新登录；
 * - rate-limit：429 → 保留旧令牌并冷却；
 * - transient：5xx、传输层错误或协议异常 → 保留旧令牌并短暂冷却。
 */
export class ClaudeAuthError extends Error {
  constructor(
    message: string,
    readonly kind: ClaudeAuthFailureKind,
    readonly retryAfterMs: number | null = null,
  ) {
    super(message);
    this.name = "ClaudeAuthError";
  }

  get invalid(): boolean {
    return this.kind === "invalid";
  }
}

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

function tokenHeaders(): Record<string, string> {
  return {
    accept: "application/json",
    "content-type": "application/json",
    "user-agent": CLAUDE_USER_AGENT,
  };
}

/** Anthropic Messages API 的 OAuth 请求头；只保留官方版本头与 OAuth 必需的 beta。 */
export function authHeaders(
  accessToken: string,
  accept = "application/json",
): Record<string, string> {
  return {
    authorization: `Bearer ${accessToken}`,
    "anthropic-version": "2023-06-01",
    "anthropic-beta": "oauth-2025-04-20",
    "content-type": "application/json",
    accept,
    "user-agent": CLAUDE_USER_AGENT,
  };
}

async function tokenFingerprint(token: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(token));
  return Array.from(
    new Uint8Array(digest).slice(0, 8),
    (byte) => byte.toString(16).padStart(2, "0"),
  ).join("");
}

export async function credentialDraft(candidate: CredentialCandidate): Promise<ResourceDraft> {
  const fingerprint = await tokenFingerprint(candidate.accessToken);
  const identity = candidate.accountUuid ?? candidate.displayName ?? fingerprint;
  const data: AccountData = {
    accessToken: candidate.accessToken,
    refreshToken: candidate.refreshToken,
    displayName: candidate.displayName ?? `Claude (${fingerprint.slice(0, 6)})`,
    accountUuid: candidate.accountUuid ?? null,
    organizationName: candidate.organizationName ?? null,
    expiresAtMs: candidate.expiresAtMs ?? null,
    lastRateLimitAtMs: null,
  };
  return {
    key: `claude-code:${identity}`,
    privateData: data as unknown as JsonValue,
  };
}

export function accountData(resource: ResourceSnapshot): AccountData {
  const data = object(resource.privateData);
  const accessToken = text(data?.accessToken);
  if (!accessToken) throw new Error("Claude account resource is missing its access token");
  return {
    accessToken,
    refreshToken: text(data?.refreshToken),
    displayName: text(data?.displayName) ?? "Claude subscription",
    accountUuid: text(data?.accountUuid),
    organizationName: text(data?.organizationName),
    expiresAtMs: number(data?.expiresAtMs),
    lastRateLimitAtMs: number(data?.lastRateLimitAtMs),
  };
}

export function isTokenExpired(data: AccountData, nowMs = Date.now()): boolean {
  if (!data.refreshToken || data.expiresAtMs === null) return false;
  return nowMs >= data.expiresAtMs - REFRESH_AHEAD_MS;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function failureKind(status: number, code: string): ClaudeAuthFailureKind {
  if (
    status === 400 || status === 401 || status === 403 ||
    code === "invalid_grant" || code === "invalid_client" || code === "unauthorized_client"
  ) {
    return "invalid";
  }
  if (status === 429) return "rate-limit";
  return "transient";
}

function refreshFailureMessage(status: number, body: string, refreshToken: string): {
  message: string;
  kind: ClaudeAuthFailureKind;
  retryAfterMs: number | null;
} {
  const parsed = parseBody(body);
  const code = text(parsed.error) ?? "";
  const description = text(parsed.error_description);
  const detail = code && description
    ? `${code}: ${description}`
    : description ?? code ?? `HTTP ${status}`;
  return {
    message: truncate(
      redactSecrets(`Claude token refresh failed: ${detail}`, [refreshToken]),
    ),
    kind: failureKind(status, code),
    retryAfterMs: null,
  };
}

/**
 * 刷新 access token。
 *
 * 只对明确的 429/5xx 重试（最多 2 次）。传输层错误**不重试**：请求可能已经到达
 * 上游并消费了 refresh token，盲目重发会把用户彻底登出。刷新成功才返回新令牌；
 * 失败由调用方决定是否覆盖 privateData（调用方一律保留旧值）。
 */
export async function refreshTokens(
  refreshToken: string,
  context: PluginContext,
): Promise<RefreshedTokens> {
  for (let attempt = 0;; attempt += 1) {
    let response: NetworkResponse;
    try {
      response = await context.network.fetch(CLAUDE_TOKEN_URL, {
        method: "POST",
        headers: tokenHeaders(),
        body: JSON.stringify({
          client_id: CLAUDE_CLIENT_ID,
          grant_type: "refresh_token",
          refresh_token: refreshToken,
          scope: CLAUDE_SCOPES,
        }),
      });
    } catch (error) {
      const message = redactSecrets(
        error instanceof Error ? error.message : String(error),
        [refreshToken],
      );
      throw new ClaudeAuthError(
        truncate(
          `Claude token refresh could not reach the token endpoint (${message}); not retried because the refresh token may already be consumed`,
        ),
        "transient",
      );
    }

    if (response.status >= 200 && response.status < 300) {
      const body = parseBody(response.body);
      const accessToken = text(body.access_token);
      if (!accessToken) {
        throw new ClaudeAuthError(
          "Claude token refresh returned no access token",
          "transient",
        );
      }
      const expiresIn = number(body.expires_in);
      return {
        accessToken,
        refreshToken: text(body.refresh_token) ?? refreshToken,
        expiresAtMs: expiresIn === null ? null : Date.now() + expiresIn * 1000,
      };
    }

    const failure = refreshFailureMessage(response.status, response.body, refreshToken);
    const retryable = response.status === 429 || response.status >= 500;
    if (!retryable || attempt >= MAX_REFRESH_RETRIES) {
      throw new ClaudeAuthError(
        failure.message,
        failure.kind,
        parseRetryAfterMs(response.headers),
      );
    }
    // 令牌端点上的 429 也可能带很长的 retry-after；进程内只等一个短窗口，
    // 真正的冷却时间通过 ClaudeAuthError.retryAfterMs 交给资源状态。
    const retryAfter = parseRetryAfterMs(response.headers);
    await sleep(
      retryAfter !== null
        ? Math.min(retryAfter, REFRESH_RETRY_BASE_MS * 2)
        : jitteredBackoffMs(attempt, REFRESH_RETRY_BASE_MS),
    );
  }
}

const STATUS_LABELS = {
  "zh-CN": {
    ready: "状态：可用",
    cooling: (seconds: number, until: string) => `状态：冷却中（剩余 ${seconds} 秒，至 ${until}）`,
    invalid: "状态：需要重新登录",
    lastRateLimit: (at: string) => `最近一次 429：${at}`,
    concurrency: (active: number, queued: number) =>
      queued > 0
        ? `并发 ${active}/${MAX_CONCURRENT_REQUESTS}（排队 ${queued}）`
        : `并发 ${active}/${MAX_CONCURRENT_REQUESTS}`,
  },
  "en-US": {
    ready: "Status: ready",
    cooling: (seconds: number, until: string) => `Status: cooling for ${seconds}s (until ${until})`,
    invalid: "Status: sign in again",
    lastRateLimit: (at: string) => `Last 429: ${at}`,
    concurrency: (active: number, queued: number) =>
      queued > 0
        ? `Concurrent ${active}/${MAX_CONCURRENT_REQUESTS} (${queued} queued)`
        : `Concurrent ${active}/${MAX_CONCURRENT_REQUESTS}`,
  },
  "ru-RU": {
    ready: "Статус: готово",
    cooling: (seconds: number, until: string) => `Статус: охлаждение ${seconds} с (до ${until})`,
    invalid: "Статус: требуется повторный вход",
    lastRateLimit: (at: string) => `Последний 429: ${at}`,
    concurrency: (active: number, queued: number) =>
      queued > 0
        ? `Параллельно ${active}/${MAX_CONCURRENT_REQUESTS} (в очереди ${queued})`
        : `Параллельно ${active}/${MAX_CONCURRENT_REQUESTS}`,
  },
} as const;

type Locale = keyof typeof STATUS_LABELS;

const LOCALES: Locale[] = ["zh-CN", "en-US", "ru-RU"];

function formatUtc(ms: number): string {
  return new Date(ms).toISOString().replace(".000Z", "Z");
}

type CooldownInfo = {
  /** 仍在冷却窗口内；invalid 账号不算冷却。 */
  cooling: boolean;
  untilMs: number | null;
  lastRateLimitAtMs: number | null;
  activeRequests: number;
  queuedRequests: number;
};

function cooldownInfo(resource: ResourceSnapshot, data: AccountData): CooldownInfo {
  const now = Date.now();
  const runtime = runtimeSnapshot(resource.id);
  const persistedCooldown = resource.state.status === "cooling" ? resource.state.retryAtMs ?? 0 : 0;
  const cooldownUntilMs = Math.max(persistedCooldown, runtime.cooldownUntilMs ?? 0);
  return {
    cooling: resource.state.status !== "invalid" && cooldownUntilMs > now,
    untilMs: cooldownUntilMs > 0 ? cooldownUntilMs : null,
    lastRateLimitAtMs: data.lastRateLimitAtMs ?? runtime.lastRateLimitAtMs,
    activeRequests: runtime.activeRequests,
    queuedRequests: runtime.queuedRequests,
  };
}

function statusDescriptions(
  resource: ResourceSnapshot,
  data: AccountData,
  info: CooldownInfo,
): Record<Locale, string> {
  const description = {} as Record<Locale, string>;
  for (const locale of LOCALES) {
    const labels = STATUS_LABELS[locale];
    const lines: string[] = [];
    if (data.organizationName) lines.push(data.organizationName);
    if (resource.state.status === "invalid") {
      lines.push(labels.invalid);
    } else if (info.cooling) {
      lines.push(
        labels.cooling(
          Math.ceil((info.untilMs! - Date.now()) / 1000),
          formatUtc(info.untilMs!),
        ),
      );
    } else {
      lines.push(labels.ready);
    }
    if (info.lastRateLimitAtMs !== null) {
      lines.push(labels.lastRateLimit(formatUtc(info.lastRateLimitAtMs)));
    }
    lines.push(labels.concurrency(info.activeRequests, info.queuedRequests));
    description[locale] = lines.join(" · ");
  }
  return description;
}

function accountMetrics(info: CooldownInfo): ResourceMetric[] {
  const metrics: ResourceMetric[] = [{
    id: "claude-active-requests",
    label: {
      "zh-CN": "当前并发请求",
      "en-US": "Concurrent requests",
      "ru-RU": "Текущие запросы",
    },
    unit: "count",
    value: info.activeRequests,
  }];
  if (info.queuedRequests > 0) {
    metrics.push({
      id: "claude-queued-requests",
      label: {
        "zh-CN": "排队请求",
        "en-US": "Queued requests",
        "ru-RU": "Запросы в очереди",
      },
      unit: "count",
      value: info.queuedRequests,
    });
  }
  if (info.cooling && info.untilMs !== null) {
    metrics.push({
      id: "claude-cooldown-seconds",
      label: {
        "zh-CN": "冷却剩余（秒）",
        "en-US": "Cooldown remaining (seconds)",
        "ru-RU": "Осталось охлаждения (с)",
      },
      unit: "count",
      value: Math.ceil((info.untilMs - Date.now()) / 1000),
      resetAtMs: info.untilMs,
    });
  }
  return metrics;
}

/**
 * 资源面板是用户判断“账号还行不行”的唯一入口。状态、最近 429、冷却剩余时间和
 * 当前并发都在这里展示；凭证永远不进入展示数据。
 */
export function presentAccount(resource: ResourceSnapshot): ResourceView {
  const data = accountData(resource);
  const info = cooldownInfo(resource, data);
  return {
    displayName: data.displayName,
    description: statusDescriptions(resource, data, info),
    metrics: accountMetrics(info),
  };
}

function refreshPatch(error: unknown, resource: ResourceSnapshot): ResourcePatch {
  if (error instanceof ClaudeAuthError) {
    if (error.kind === "invalid") {
      return { state: { status: "invalid", message: AUTH_INVALID_MESSAGE } };
    }
    const until = error.kind === "rate-limit"
      ? noteRateLimit(resource.id, error.retryAfterMs)
      : noteRefreshFailure(resource.id);
    return {
      state: {
        status: "cooling",
        retryAtMs: until,
        message: truncate(error.message),
      },
    };
  }
  const until = noteRefreshFailure(resource.id);
  return {
    state: {
      status: "cooling",
      retryAtMs: until,
      message: truncate(error instanceof Error ? error.message : String(error)),
    },
  };
}

/**
 * 用户主动刷新。与请求路径共用 single-flight：同一账号在同一时刻只有一次刷新
 * 网络请求，refresh token 不会被并发用两次。失败只更新状态，保留旧 privateData。
 */
export async function refreshAccount(
  resource: ResourceSnapshot,
  context: PluginContext,
): Promise<ResourcePatch> {
  const data = accountData(resource);
  if (!data.refreshToken) {
    return {
      state: { status: "invalid", message: AUTH_INVALID_MESSAGE },
    };
  }

  try {
    const tokens = await singleFlight(
      `claude-refresh:${resource.id}`,
      () => refreshTokens(data.refreshToken!, context),
    );
    const updated: AccountData = {
      ...data,
      accessToken: tokens.accessToken,
      refreshToken: tokens.refreshToken,
      expiresAtMs: tokens.expiresAtMs,
    };
    return {
      privateData: updated as unknown as JsonValue,
      state: { status: "ready" },
    };
  } catch (error) {
    return refreshPatch(error, resource);
  }
}
