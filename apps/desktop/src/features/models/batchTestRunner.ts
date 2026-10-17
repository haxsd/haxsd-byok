/**
 * 批量连通性测试的派发器（18-frontend-features/F-02）。
 *
 * 一次只开 `concurrency` 条真实上游调用：几十个模型同时打出去会瞬间吃掉配额、触发限流，
 * 而且整批要一起等最慢的那条。
 *
 * **取消必须停住整批**。以前取消只 abort 了在跑的那几条，worker 的 `while` 没有任何检查，
 * 于是它们继续把剩下的模型一条条发出去——界面上写着"已取消"，后台还在烧真实上游调用。
 * 这里每轮派发**前**先问一次 `cancelled()`；取消之后不再启动任何新测试，剩下没轮到的目标
 * 通过 `skipped` 交给调用方标成已取消（用户看到的是"这批都取消了"，而不是永远"未测试"）。
 */

export type BatchTestOutcome = "success" | "failure" | "cancelled";

export type BatchTestResult<T> = {
  /** 真正跑过的目标各自的结论，顺序与派发顺序一致。 */
  results: BatchTestOutcome[];
  /** 取消后来不及开始的目标（保持原顺序）：调用方要把它们标成已取消。 */
  skipped: T[];
};

export const BATCH_TEST_CONCURRENCY = 4;

export async function runModelTestBatch<T>(options: {
  targets: readonly T[];
  /** 每轮派发前问一次：true 就不再启动新的测试。 */
  cancelled: () => boolean;
  run: (target: T) => Promise<BatchTestOutcome>;
  /** 每跑完一条报一次进度（只统计真正跑过的）。 */
  onProgress?: (done: number, total: number) => void;
  concurrency?: number;
}): Promise<BatchTestResult<T>> {
  const { targets, cancelled, run, onProgress, concurrency = BATCH_TEST_CONCURRENCY } = options;
  const results: BatchTestOutcome[] = [];
  let next = 0;
  const worker = async () => {
    while (next < targets.length) {
      // 检查必须在取目标之前：取完再检查，剩下的目标会被静默算进"从没发生"，调用方也就
      // 无从把它们标成已取消。
      if (cancelled()) return;
      const target = targets[next];
      next += 1;
      results.push(await run(target));
      onProgress?.(results.length, targets.length);
    }
  };
  await Promise.all(Array.from({ length: Math.max(1, Math.min(concurrency, targets.length)) }, () => worker()));
  return { results, skipped: targets.slice(next) };
}
