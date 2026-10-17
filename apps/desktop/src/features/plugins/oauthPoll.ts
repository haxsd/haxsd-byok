import type { PluginOAuthPoll } from "../../shared/api";
import { browserTimers, type PollTimers } from "../../shared/utils/pollTimers";

export type OAuthPollHandlers = {
  poll: () => Promise<PluginOAuthPoll>;
  /** 还在等用户确认：界面借这一拍把上一次失败的红字撤掉。 */
  onPending: () => void;
  onCompleted: (modelSyncError: string | null) => void;
  onFailed: (message: string | null) => void;
  onTransportError: (cause: unknown) => void;
};

/**
 * 设备码授权轮询：一条**串行**的定时器链。
 *
 * 用递归 setTimeout 而不是 setInterval：上一拍 settle 之后才排下一拍。更关键的是取消位——
 * 只清定时器是不够的：正在飞的那一次 `poll()` 返回后仍会再排一个定时器，于是 effect 每次
 * 重启都会多留一条并行轮询链（同一个 session 被两条链反复问）。返回的停止函数同时做两件事：
 * 清掉待发的定时器、把取消位置位，在途的那一次回来也续不上。
 *
 * 这个循环从 `OAuthMethodCard` 的 effect 里抽出来，就是为了能把它钉住：组件本身在 Node 里
 * 跑不起来（没有 DOM），但这条链的行为可以。
 */
export function startOAuthPoll(
  handlers: OAuthPollHandlers,
  initialIntervalMs: number,
  timers: PollTimers = browserTimers,
): () => void {
  let cancelled = false;
  let timer = 0;

  const schedule = (nextIntervalMs: number) => {
    if (cancelled) return;
    // 服务端可能给一个很小的间隔；下限 1 秒，别把本机服务问成压力测试。
    timer = timers.setTimeout(() => void tick(nextIntervalMs), Math.max(1000, nextIntervalMs));
  };

  const tick = async (retryIntervalMs: number) => {
    if (cancelled) return;
    let result: PluginOAuthPoll;
    try {
      result = await handlers.poll();
    } catch (cause) {
      if (cancelled) return;
      handlers.onTransportError(cause);
      schedule(retryIntervalMs);
      return;
    }
    if (cancelled) return;
    if (result.status === "pending") {
      handlers.onPending();
      schedule(result.pollIntervalMs);
      return;
    }
    if (result.status === "completed") {
      handlers.onCompleted(result.modelSyncError);
      return;
    }
    handlers.onFailed(result.message);
  };

  schedule(initialIntervalMs);
  return () => {
    cancelled = true;
    timers.clearTimeout(timer);
  };
}
