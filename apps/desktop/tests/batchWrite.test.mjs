// 「一次操作 = N 个 PUT」的收尾方式：并发有上限、单条失败不牵连其余、失败项按顺序报回来。
//
// 原来这两处（插件模型启用、模型分组设置）是串行 await：N 大时用户等到最后一条，
// 中间任意一条失败就中断，剩下的一个都不提交，界面只弹一条通用错误。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const batch = () => loader.load("/src/shared/utils/batchWrite.ts");

test("writes stay within the concurrency limit while every item is attempted", async () => {
  const { runBatchedWrites } = await batch();
  const items = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
  const written = [];
  let inFlight = 0;
  let peak = 0;

  const failures = await runBatchedWrites(items, async (item) => {
    inFlight += 1;
    peak = Math.max(peak, inFlight);
    await new Promise((resolve) => setTimeout(resolve, 1));
    inFlight -= 1;
    written.push(item);
    if (item % 4 === 0) throw new Error(`模型 ${item} 保存失败`);
  }, 3);

  assert.equal(peak, 3, "同时最多只开 3 条写请求");
  assert.deepEqual([...written].sort((left, right) => left - right), items, "一条失败不能中断其余的写入");
  assert.deepEqual(failures.map((failure) => failure.item), [4, 8], "失败项按输入顺序报回来");
  assert.match(failures[0].reason.message, /模型 4 保存失败/, "失败原因要跟着条目一起返回");
});

test("an empty batch resolves without starting a worker", async () => {
  const { runBatchedWrites } = await batch();
  let calls = 0;
  assert.deepEqual(await runBatchedWrites([], async () => { calls += 1; }), []);
  assert.equal(calls, 0);
});

test("the default concurrency is the shared constant", async () => {
  const { runBatchedWrites, WRITE_CONCURRENCY } = await batch();
  const items = Array.from({ length: WRITE_CONCURRENCY * 3 }, (_, index) => index);
  let inFlight = 0;
  let peak = 0;
  await runBatchedWrites(items, async () => {
    inFlight += 1;
    peak = Math.max(peak, inFlight);
    await new Promise((resolve) => setTimeout(resolve, 1));
    inFlight -= 1;
  });
  assert.equal(peak, WRITE_CONCURRENCY);
});
