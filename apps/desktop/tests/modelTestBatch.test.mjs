// 「一键测试」的取消必须停住整批（18-frontend-features/F-02）。
//
// 复核结论：取消只 abort 了在跑的那几条，worker 的 while 没有取消检查，于是它继续把剩下的
// 模型一条条发出去——界面上写着"已取消"，后台还在烧真实上游调用。这一组用例钉住：取消后不再
// 启动新的测试、未开始的项全部标成已取消；不取消时仍然按并发上限跑完全部。

import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const targets = (count) => Array.from({ length: count }, (_, index) => ({ model_hash: `m${index}`, display_name: `模型 ${index}` }));

async function loadRunner() {
  loader.invalidateAll();
  return loader.load("/src/features/models/batchTestRunner.ts");
}

test("cancelling the batch stops it: at most one more test starts and everything else is skipped", async () => {
  const { runModelTestBatch } = await loadRunner();
  const controls = { cancelled: false };
  let started = 0;

  const { results, skipped } = await runModelTestBatch({
    targets: targets(10),
    cancelled: () => controls.cancelled,
    run: async () => {
      started += 1;
      // 第 2 条一开始就取消（用户在界面上点了"取消全部"）。
      if (started === 2) controls.cancelled = true;
      await Promise.resolve();
      return "cancelled";
    },
  });

  assert.ok(started <= 3, `取消之后最多再多开 1 条，实际开始了 ${started} 条`);
  assert.equal(results.length, started, "结果只对应真正跑过的目标");
  assert.equal(skipped.length, 10 - started, "剩下的全部进 skipped，由调用方标成已取消");
  assert.deepEqual(skipped.map((target) => target.model_hash), targets(10).slice(started).map((target) => target.model_hash), "skipped 保持原顺序");
  assert.ok(skipped.length >= 7, "10 条里至少 7 条不该被测过");
});

test("a cancel before the first dispatch starts nothing at all", async () => {
  const { runModelTestBatch } = await loadRunner();
  let started = 0;

  const { results, skipped } = await runModelTestBatch({
    targets: targets(6),
    cancelled: () => true,
    run: async () => { started += 1; return "success"; },
  });

  assert.equal(started, 0, "取消已生效时一条都不许发");
  assert.deepEqual(results, []);
  assert.equal(skipped.length, 6);
});

test("without a cancel the whole batch still runs, no faster than the concurrency limit", async () => {
  const { runModelTestBatch, BATCH_TEST_CONCURRENCY } = await loadRunner();
  let inFlight = 0;
  let peak = 0;
  let finished = 0;
  const progress = [];

  const { results, skipped } = await runModelTestBatch({
    targets: targets(10),
    cancelled: () => false,
    run: async () => {
      inFlight += 1;
      peak = Math.max(peak, inFlight);
      await Promise.resolve();
      inFlight -= 1;
      finished += 1;
      return finished % 3 === 0 ? "failure" : "success";
    },
    onProgress: (done, total) => progress.push([done, total]),
  });

  assert.equal(results.length, 10, "没有取消就该跑完");
  assert.deepEqual(skipped, [], "没有跳过的目标");
  assert.ok(peak <= BATCH_TEST_CONCURRENCY, `同时在跑的不超过 ${BATCH_TEST_CONCURRENCY} 条，实际 ${peak}`);
  assert.deepEqual(progress.at(0), [1, 10]);
  assert.deepEqual(progress.at(-1), [10, 10], "进度只统计真正跑过的");
});

test("a single target works and an empty batch is a no-op", async () => {
  const { runModelTestBatch } = await loadRunner();

  const one = await runModelTestBatch({ targets: targets(1), cancelled: () => false, run: async () => "success" });
  assert.deepEqual(one.results, ["success"]);
  assert.deepEqual(one.skipped, []);

  const empty = await runModelTestBatch({ targets: [], cancelled: () => false, run: async () => "success" });
  assert.deepEqual(empty.results, []);
  assert.deepEqual(empty.skipped, []);
});

test("the page stops dispatching before it aborts the in-flight tests", async () => {
  const source = await readFile(new URL("../src/features/models/ModelLibraryPage.tsx", import.meta.url), "utf8");

  assert.match(source, /runModelTestBatch\(/, "批量测试要走能取消的派发器");
  assert.match(source, /batchRun\.current\.cancelled = true/, "取消时先置标志");
  assert.match(source, /cancelled: \(\) => controls\.cancelled/, "派发器每轮检查这次批次的取消标志");
  assert.match(source, /\{ status: "cancelled" \}/, "没轮到的目标要标成已取消");
  // 先置标志再 abort：反过来的话在跑的几条结束后 worker 还会再派发。
  const cancelBody = source.slice(source.indexOf("const cancelAllModelTests"), source.indexOf("const testModel = async"));
  assert.ok(cancelBody.indexOf("cancelled = true") < cancelBody.indexOf("cancelModelTest("), "置标志必须在 abort 之前");
});
