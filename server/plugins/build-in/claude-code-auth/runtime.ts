/**
 * 账号级运行时状态。只存在于当前插件 worker 进程内：
 * - 并发闸门：同一账号最多同时跑 2 个推理请求，超出的排队；
 * - 冷却窗口：429 后按 retry-after 或指数退避暂停该账号；
 * - 刷新 single-flight：同一账号的并发刷新共用一次网络请求，避免单次
 *   refresh token 被用两次。
 *
 * 持久状态（令牌、状态）仍由宿主资源记录保存；这里只放“进程内、可丢失”的调度信息。
 */

/** 每个账号允许并行的推理请求数。排队而不是打满，减少触发上游限额的机会。 */
export const MAX_CONCURRENT_REQUESTS = 2;
/** 5xx 最多重试 2 次（初次请求之外）。 */
export const MAX_SERVER_ERROR_RETRIES = 2;
/** 刷新请求遇到明确的 5xx/429 时最多重试 2 次；传输层错误不重试。 */
export const MAX_REFRESH_RETRIES = 2;
export const SERVER_RETRY_BASE_MS = 400;
export const REFRESH_RETRY_BASE_MS = 400;
export const RATE_LIMIT_BASE_MS = 30_000;
export const MAX_BACKOFF_MS = 60 * 60 * 1000;
/** 刷新暂时不可用（5xx/传输层错误）时，账号短暂冷却而不是连续重试。 */
export const REFRESH_COOLDOWN_MS = 30_000;

export type CooldownKind = "rate-limit" | "refresh";

export type AccountRuntimeSnapshot = {
  activeRequests: number;
  queuedRequests: number;
  cooldownUntilMs: number | null;
  cooldownKind: CooldownKind | null;
  lastRateLimitAtMs: number | null;
};

type Waiter = { grant: () => void };

type AccountRuntime = {
  activeRequests: number;
  waiters: Set<Waiter>;
  cooldownUntilMs: number;
  cooldownKind: CooldownKind | null;
  lastRateLimitAtMs: number | null;
  rateLimitStrikes: number;
};

const accounts = new Map<string, AccountRuntime>();
const refreshFlights = new Map<string, Promise<unknown>>();

function runtime(accountKey: string): AccountRuntime {
  let state = accounts.get(accountKey);
  if (!state) {
    state = {
      activeRequests: 0,
      waiters: new Set(),
      cooldownUntilMs: 0,
      cooldownKind: null,
      lastRateLimitAtMs: null,
      rateLimitStrikes: 0,
    };
    accounts.set(accountKey, state);
  }
  return state;
}

function abortError(): Error {
  const error = new Error("Claude request was cancelled while waiting for a concurrency slot");
  error.name = "AbortError";
  return error;
}

function acquire(state: AccountRuntime, signal: AbortSignal): Promise<void> {
  if (signal.aborted) return Promise.reject(abortError());
  if (state.activeRequests < MAX_CONCURRENT_REQUESTS) {
    state.activeRequests += 1;
    return Promise.resolve();
  }
  return new Promise<void>((resolve, reject) => {
    let settled = false;
    const waiter: Waiter = {
      grant: () => {
        if (settled) return;
        settled = true;
        signal.removeEventListener("abort", onAbort);
        state.activeRequests += 1;
        resolve();
      },
    };
    const onAbort = () => {
      if (settled) return;
      settled = true;
      state.waiters.delete(waiter);
      reject(abortError());
    };
    signal.addEventListener("abort", onAbort, { once: true });
    state.waiters.add(waiter);
  });
}

/** 释放槽位并把下一个排队请求放进来；`grant` 自己负责增加 active。 */
function release(state: AccountRuntime): void {
  state.activeRequests = Math.max(0, state.activeRequests - 1);
  const next = state.waiters.values().next();
  if (next.done) return;
  state.waiters.delete(next.value);
  next.value.grant();
}

/** 在账号并发槽位里执行任务；任务抛错或取消也会释放槽位。 */
export async function withAccountSlot<T>(
  accountKey: string,
  signal: AbortSignal,
  task: () => Promise<T>,
): Promise<T> {
  const state = runtime(accountKey);
  await acquire(state, signal);
  try {
    return await task();
  } finally {
    release(state);
  }
}

/** 同一 key 的并发调用只执行一次任务，其余调用等待同一个结果。 */
export function singleFlight<T>(key: string, task: () => Promise<T>): Promise<T> {
  const existing = refreshFlights.get(key) as Promise<T> | undefined;
  if (existing) return existing;
  const operation: Promise<T> = task().finally(() => {
    if (refreshFlights.get(key) === operation) refreshFlights.delete(key);
  });
  refreshFlights.set(key, operation);
  return operation;
}

/**
 * 解析 `retry-after`。上游这里给的是整数秒；HTTP-date 等其它形式不猜，
 * 返回 null 让调用方走指数退避 + 抖动。
 */
export function parseRetryAfterMs(
  headers: Record<string, string>,
): number | null {
  for (const [name, value] of Object.entries(headers)) {
    if (name.toLowerCase() !== "retry-after") continue;
    const seconds = Number(value.trim());
    if (Number.isInteger(seconds) && seconds > 0) return seconds * 1000;
    return null;
  }
  return null;
}

/** 指数退避 + 抖动；返回区间约为 base·2^attempt 的 0.5~1.5 倍。 */
export function jitteredBackoffMs(
  attempt: number,
  baseMs: number,
  random: () => number = Math.random,
): number {
  const exponential = baseMs * 2 ** Math.max(0, attempt);
  const jittered = Math.round(exponential * (0.5 + random()));
  return Math.min(MAX_BACKOFF_MS, Math.max(0, jittered));
}

/** 记录一次 429，返回冷却截止时间。有 retry-after 时严格按它，没有才退避。 */
export function noteRateLimit(
  accountKey: string,
  retryAfterMs: number | null,
  nowMs = Date.now(),
): number {
  const state = runtime(accountKey);
  state.lastRateLimitAtMs = nowMs;
  state.rateLimitStrikes += 1;
  const until = retryAfterMs !== null
    ? nowMs + retryAfterMs
    : nowMs + jitteredBackoffMs(state.rateLimitStrikes - 1, RATE_LIMIT_BASE_MS);
  state.cooldownUntilMs = Math.max(state.cooldownUntilMs, until);
  state.cooldownKind = "rate-limit";
  return state.cooldownUntilMs;
}

/** 刷新暂时失败时短暂冷却，避免同一账号连续打令牌端点。 */
export function noteRefreshFailure(accountKey: string, nowMs = Date.now()): number {
  const state = runtime(accountKey);
  const until = nowMs + jitteredBackoffMs(0, REFRESH_COOLDOWN_MS);
  state.cooldownUntilMs = Math.max(state.cooldownUntilMs, until);
  state.cooldownKind = "refresh";
  return state.cooldownUntilMs;
}

export function cooldownUntil(accountKey: string): number | null {
  const state = runtime(accountKey);
  return state.cooldownUntilMs > 0 ? state.cooldownUntilMs : null;
}

/** 成功调用后清除已到期的冷却；未到期的冷却不会因为一次成功而提前解除。 */
export function noteSuccess(accountKey: string, nowMs = Date.now()): void {
  const state = runtime(accountKey);
  state.rateLimitStrikes = 0;
  if (state.cooldownUntilMs <= nowMs) {
    state.cooldownUntilMs = 0;
    state.cooldownKind = null;
  }
}

export function runtimeSnapshot(accountKey: string): AccountRuntimeSnapshot {
  const state = runtime(accountKey);
  return {
    activeRequests: state.activeRequests,
    queuedRequests: state.waiters.size,
    cooldownUntilMs: state.cooldownUntilMs > 0 ? state.cooldownUntilMs : null,
    cooldownKind: state.cooldownKind,
    lastRateLimitAtMs: state.lastRateLimitAtMs,
  };
}

/** 只给测试用：清空进程内状态，避免用例之间互相影响。 */
export function resetRuntimeForTests(): void {
  accounts.clear();
  refreshFlights.clear();
}
