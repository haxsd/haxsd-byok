/**
 * 一批「逐条写」请求的收尾方式。
 *
 * 界面上有几处是"一次操作 = N 个模型的 PUT"（插件模型启用、模型分组设置）。原来写成
 * 串行 `await`：N 大时用户要盯着进度条等到最后一个，而且中间任意一条失败就中断，后面
 * 的项一个都不提交，界面只弹一条通用错误——用户既不知道改到第几个，也不知道重试会不会
 * 重复提交。
 *
 * 这里换成有限并发 + 全部尝试 + 汇总失败项：慢的项不再互相排队，一条失败不牵连其余，
 * 调用方拿到失败的条目名，能把"成功 N 个 / 失败 M 个（名字）"说清楚。
 */

/** 同时最多几条写请求。服务端是本机进程，四条的吞吐已经远超串行，又不会把写锁挤满。 */
export const WRITE_CONCURRENCY = 4;

export type BatchWriteFailure<T> = { item: T; reason: unknown };

/**
 * 有限并发地逐条执行 `write`，返回失败项（按输入顺序）。
 *
 * 本函数自己从不抛错：单条失败只记进结果，其余照常执行。
 */
export async function runBatchedWrites<T>(
  items: readonly T[],
  write: (item: T) => Promise<unknown>,
  concurrency: number = WRITE_CONCURRENCY,
): Promise<Array<BatchWriteFailure<T>>> {
  const failures: Array<BatchWriteFailure<T> | null> = new Array(items.length).fill(null);
  let next = 0;
  const worker = async () => {
    while (next < items.length) {
      const index = next;
      next += 1;
      try {
        await write(items[index]);
      } catch (reason) {
        failures[index] = { item: items[index], reason };
      }
    }
  };
  await Promise.all(Array.from(
    { length: Math.max(1, Math.min(concurrency, items.length)) },
    () => worker(),
  ));
  return failures.filter((failure): failure is BatchWriteFailure<T> => failure !== null);
}
