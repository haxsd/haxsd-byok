import { appStore } from "../../shared/store/appStore";
import { browserTimers, type PollTimers } from "../../shared/utils/pollTimers";

export type CaWaitHandlers = {
  /** 只读一次 CA 状态。 */
  refresh: () => Promise<void>;
  isReady: () => boolean;
  onDone: () => void;
};

export type CaWaitOptions = {
  /** 触发终端后的等待节奏：最长 30 秒，够一次 UAC 授权或一次密码输入。 */
  intervalMs: number;
  maxAttempts: number;
  timers?: PollTimers;
};

/**
 * 等本地 CA 装好：递归 setTimeout，上一拍 settle 之后才排下一拍。
 *
 * 用 setInterval 的话，一次慢于间隔的查询会被下一拍作废重来（旧实现走的是整批
 * `appStore.refresh()`，12 个接口各拉一遍，慢下来是常态）。超时不是失败——授权可能还在
 * 等待，所以到点只是停止自动等待，手动刷新照旧。
 */
export function startCaWaitPoll(handlers: CaWaitHandlers, options: CaWaitOptions): () => void {
  const timers = options.timers ?? browserTimers;
  let cancelled = false;
  let timer = 0;
  let attempts = 0;

  const schedule = () => {
    if (cancelled) return;
    timer = timers.setTimeout(() => void tick(), options.intervalMs);
  };

  const tick = async () => {
    if (cancelled) return;
    attempts += 1;
    await handlers.refresh();
    if (cancelled) return;
    if (handlers.isReady() || attempts >= options.maxAttempts) {
      handlers.onDone();
      return;
    }
    schedule();
  };

  schedule();
  return () => {
    cancelled = true;
    timers.clearTimeout(timer);
  };
}

/**
 * 默认接线：等待期间只读 Cursor 接管状态这一个接口。
 *
 * 旧实现每 2 秒走一次 `appStore.refresh()`（十二个接口），而等待要看的就是 `ca` 一个字段。
 */
export function startCursorCaWaitPoll(onDone: () => void, options: CaWaitOptions): () => void {
  return startCaWaitPoll({
    refresh: () => appStore.refreshCursorHarness(),
    isReady: () => appStore.getSnapshot().cursorHarness?.ca === "ready",
    onDone,
  }, options);
}
