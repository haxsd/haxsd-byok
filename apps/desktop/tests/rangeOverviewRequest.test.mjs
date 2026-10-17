// 首页区间请求的代际管理：取消要既 abort 在途的 HTTP、又让旧结果不再落地。
//
// 旧实现只有 effect 的 `active` 布尔：请求照发不误（快速切尺度时服务端要把每一发都算完），
// 且 abort 与响应到达之间的窗口里旧结果仍可能盖掉新尺度。这一组用例把两件事都钉住。

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, deferred, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const request = () => loader.load("/src/features/home/overview/rangeOverviewRequest.ts");
const RANGE_A = { startMs: 1_000, endMs: 2_000 };
const RANGE_B = { startMs: 3_000, endMs: 4_000 };

function overviewFixture(calls) {
  return {
    metrics: { llm_calls: calls, successful_calls: calls, failed_calls: 0, token_usage: 0, prompt_tokens: 0, input_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0, output_tokens: 0 },
    token_usage_granularity: "day",
    token_usage_series: [],
  };
}

function recorder() {
  const events = [];
  return {
    events,
    handlers: {
      onLoaded: (overview, range) => events.push({ kind: "loaded", calls: overview.metrics.llm_calls, range }),
      onFailed: (message) => events.push({ kind: "failed", message }),
      onSettled: () => events.push({ kind: "settled" }),
    },
  };
}

test("cancelling a range request aborts the wire call and keeps its answer out of the UI", async () => {
  const { backend } = installAppBackend();
  const held = deferred();
  backend.route("GET", `${API_ROOT}/overview`, async () => held.promise);
  const { requestRangeOverview } = await request();
  const log = recorder();

  const cancel = requestRangeOverview({ range: RANGE_A, modelHashes: [] }, log.handlers);
  assert.equal(backend.requestsFor("GET", `${API_ROOT}/overview`).length, 1);
  cancel();

  const [sent] = backend.requestsFor("GET", `${API_ROOT}/overview`);
  assert.equal(sent.signal?.aborted, true, "取消必须 abort 在途的请求，服务端不必再算完");

  held.resolve({ body: overviewFixture(7) });
  await new Promise((resolve) => setImmediate(resolve));

  assert.deepEqual(log.events, [], "取消之后这一次的结果与收尾都不该再落到界面上");
});

test("switching ranges applies only the newest answer", async () => {
  const { backend } = installAppBackend();
  const first = deferred();
  const second = deferred();
  let call = 0;
  backend.route("GET", `${API_ROOT}/overview`, () => {
    call += 1;
    return call === 1 ? first.promise : second.promise;
  });
  const { requestRangeOverview } = await request();
  const log = recorder();

  const cancelFirst = requestRangeOverview({ range: RANGE_A, modelHashes: [] }, log.handlers);
  cancelFirst();
  requestRangeOverview({ range: RANGE_B, modelHashes: [] }, log.handlers);

  const [sentFirst, sentSecond] = backend.requestsFor("GET", `${API_ROOT}/overview`);
  assert.equal(sentFirst.signal?.aborted, true, "被换掉的那一发要 abort");
  assert.equal(sentSecond.signal?.aborted, false, "最新的一发仍在飞");

  // 旧的那一发晚回：不能覆盖新尺度。
  first.resolve({ body: overviewFixture(1) });
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(log.events, [], "旧发的结果不能落地");

  second.resolve({ body: overviewFixture(2) });
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(log.events, [
    { kind: "loaded", calls: 2, range: RANGE_B },
    { kind: "settled" },
  ], "只有最新一发落地，并且收尾一次");
});

test("a failed range read reports the reason and still settles the busy state", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/overview`, () => ({ status: 500, body: { message: "统计不可用" } }));
  const { requestRangeOverview } = await request();
  const log = recorder();

  requestRangeOverview({ range: RANGE_A, modelHashes: [] }, log.handlers);
  await new Promise((resolve) => setTimeout(resolve, 20));

  assert.deepEqual(log.events, [
    { kind: "failed", message: "统计不可用" },
    { kind: "settled" },
  ]);
});
