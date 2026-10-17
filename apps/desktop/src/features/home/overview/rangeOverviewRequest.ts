import { api, type Overview } from "../../../shared/api";

export type TimeRange = { startMs: number; endMs: number };

export type RangeOverviewHandlers = {
  onLoaded: (overview: Overview, range: TimeRange) => void;
  onFailed: (message: string) => void;
  onSettled: () => void;
};

/**
 * 发一次区间请求，返回取消函数。
 *
 * 取消必须同时做两件事：
 * 1. `abort()` 掉在途的 HTTP——快速切换尺度时，旧的那一发不必再让服务端算完；
 * 2. 把这次请求标记为过期——只 abort、不设防的话，晚回来的旧结果仍会把新尺度的数据盖掉
 *    （`abort()` 与 fetch 真正落地之间有窗口，取消的那一发也可能已经拿到响应）。
 *
 * 从 HomePage 的 effect 里抽出来就是为了能在 Node 里把它钉住：组件本身没有 DOM 跑不了。
 */
export function requestRangeOverview(
  input: { range: TimeRange; modelHashes: string[] },
  handlers: RangeOverviewHandlers,
): () => void {
  const controller = new AbortController();
  let current = true;
  void api.overview({
    startMs: input.range.startMs,
    endMs: input.range.endMs,
    modelHashes: input.modelHashes,
  }, controller.signal).then((overview) => {
    if (current) handlers.onLoaded(overview, input.range);
  }).catch((cause) => {
    if (current) handlers.onFailed(cause instanceof Error ? cause.message : String(cause));
  }).finally(() => {
    if (current) handlers.onSettled();
  });
  return () => {
    current = false;
    controller.abort();
  };
}
