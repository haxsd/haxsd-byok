/**
 * 离线自动重试的判据与节奏。
 *
 * 原来的实现是 `setInterval(3s)`：不看窗口状态。窗口最小化或用户切到别的应用时，
 * 每 3 秒仍然发一整批请求（`appStore.refresh()` 打十二个接口）——后台的机器在等一个
 * 用户根本没在看的界面。重试要"在用户回来看的时候"发生，所以这里把两件事分开：
 * 窗口可见/有焦点才发请求，间隔按 3s → 10s → 30s 退避（服务挂久了就别再高频打）。
 */

/** 退避节奏：前几次快（服务重启通常几秒），之后放慢并停在 30 秒。 */
export const RETRY_DELAYS_MS = [3_000, 10_000, 30_000] as const;

export function retryDelayMs(attempt: number): number {
  if (!Number.isFinite(attempt) || attempt <= 0) return RETRY_DELAYS_MS[0];
  return RETRY_DELAYS_MS[Math.min(Math.floor(attempt), RETRY_DELAYS_MS.length - 1)];
}

/**
 * 是否该发这一拍。
 *
 * 最小化（`hidden`）或整个窗口没有焦点时都不发：前者用户看不见，后者用户正在别的窗口里
 * 干活。两种情况下都保持等待——用户切回来时下一拍立刻补上，不丢重试。
 */
export function shouldAutoRetry(view: { hidden: boolean; hasFocus: boolean }): boolean {
  return !view.hidden && view.hasFocus;
}

/**
 * 从 document 上取窗口状态（单独一层是为了测试能塞替身）。
 *
 * `hasFocus` 缺失时按"有焦点"处理：这个能力在老环境或替身里可能没有，而"永远不重试"
 * 比"多试一次"坏得多。
 */
export function windowVisibility(view: { hidden?: boolean; hasFocus?: () => boolean }): { hidden: boolean; hasFocus: boolean } {
  return {
    hidden: Boolean(view.hidden),
    hasFocus: typeof view.hasFocus === "function" ? Boolean(view.hasFocus()) : true,
  };
}
