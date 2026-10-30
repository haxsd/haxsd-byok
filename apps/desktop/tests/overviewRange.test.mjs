// 概览页的区间口径：趋势与模型用量只看服务端按整段范围聚合的分桶数据。
//
// 这两项原来复用调用列表（服务端只返回最新一批，且调用页每 2 秒刷新），于是"近30天每个模型
// 用了多少"实际算的是"最新 200 条里每个模型用了多少"，数字还会跟着那个窗口自己变。

import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const stats = () => loader.load("/src/features/home/overview/overviewStats.ts");

function bucket(startMs, models) {
  return {
    bucket_start_ms: startMs,
    input_tokens: models.reduce((total, model) => total + model.input_tokens, 0),
    cache_read_tokens: models.reduce((total, model) => total + model.cache_read_tokens, 0),
    cache_write_tokens: models.reduce((total, model) => total + model.cache_write_tokens, 0),
    output_tokens: models.reduce((total, model) => total + model.output_tokens, 0),
    models,
  };
}

function model(overrides) {
  return {
    model_hash: "hash-1",
    model_id: "gpt-5",
    display_name: "主力模型",
    llm_calls: 1,
    input_tokens: 0,
    cache_read_tokens: 0,
    cache_write_tokens: 0,
    output_tokens: 0,
    ...overrides,
  };
}

test("the call trend counts the bucket's own call totals", async () => {
  const { callsPerBucket } = await stats();
  const series = [
    bucket(0, [model({ llm_calls: 3 }), model({ model_hash: "hash-2", model_id: "claude", llm_calls: 2 })]),
    bucket(86_400_000, [model({ llm_calls: 500 })]),
    bucket(172_800_000, []),
  ];
  assert.deepEqual(callsPerBucket(series), [5, 500, 0]);
});

test("model usage covers the whole range, not the newest page of calls", async () => {
  const { modelUsage } = await stats();
  const series = [
    bucket(0, [
      model({ llm_calls: 7, input_tokens: 100, cache_read_tokens: 10, output_tokens: 20 }),
      model({ model_hash: "hash-2", model_id: "claude", display_name: "Claude", llm_calls: 1, input_tokens: 30, output_tokens: 10 }),
    ]),
    bucket(86_400_000, [
      model({ llm_calls: 4, input_tokens: 40, output_tokens: 10 }),
      model({ model_hash: null, model_id: "unknown-model", display_name: "", llm_calls: 2, input_tokens: 5, output_tokens: 5 }),
    ]),
  ];
  const rows = modelUsage(series);
  assert.deepEqual(rows.map((row) => row.key), ["hash-1", "hash-2", "unknown-model"], "按用量降序");
  assert.deepEqual(rows.map((row) => row.calls), [11, 1, 2]);
  assert.deepEqual(rows.map((row) => row.tokens), [180, 40, 10]);
  assert.equal(rows[0].label, "主力模型");
  assert.equal(rows[2].label, "unknown-model", "没有显示名时回落到模型标识");
  assert.equal(Number(rows.reduce((total, row) => total + row.share, 0).toFixed(6)), 1, "占比之和为 1");
});

test("an empty series yields no rows and no division by zero", async () => {
  const { modelUsage, callsPerBucket } = await stats();
  assert.deepEqual(modelUsage([]), []);
  assert.deepEqual(callsPerBucket([]), []);
});

test("an unusable custom range says which input is wrong", async () => {
  const { timeRangeProblem } = await loader.load("/src/shared/utils/parseTimeInput.ts");
  const now = new Date("2026-10-16T12:00:00");
  assert.equal(timeRangeProblem("1小时前", "现在", now), null);
  assert.equal(timeRangeProblem("", "现在", now), "start-unparsed");
  assert.equal(timeRangeProblem("现在", "   ", now), "end-unparsed");
  assert.equal(timeRangeProblem("乱写的时间", "现在", now), "start-unparsed");
  assert.equal(timeRangeProblem("现在", "1小时前", now), "order");
  assert.equal(timeRangeProblem("2026-10-16 12:00", "2026-10-16 12:00", now), "order", "同一时刻也算非法区间");
});

test("switching from the custom popover to a preset keeps the chosen model filter", async () => {
  const source = await readFile(new URL("../src/features/home/HomePage.tsx", import.meta.url), "utf8");
  const start = source.indexOf("const selectPreset");
  const body = source.slice(start, source.indexOf("};", start));
  assert.match(body, /setAppliedModels\(selectedModels\)/, "预设切换必须把弹窗里的最新选择同步到请求口径");
});
