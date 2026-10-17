// 等 CA 装好的轮询：递归 setTimeout（不叠加），而且每拍只读一个接口。
//
// 旧实现是 window.setInterval(2s) + 每拍整批 appStore.refresh()（十二个接口）。复核把
// "两批并列打满 30 秒" 推翻了（refresh 会 abort 上一批），但"重复做无用功 + 单次超过 2s 的
// 查询被取消"是真的——这一组用例把两件事钉住：不叠加、只读一个。

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

function fakeTimers() {
  let nextId = 1;
  const pending = new Map();
  return {
    timers: {
      setTimeout: (handler, timeoutMs) => {
        const id = nextId;
        nextId += 1;
        pending.set(id, { handler, timeoutMs });
        return id;
      },
      clearTimeout: (id) => { pending.delete(id); },
    },
    pendingCount: () => pending.size,
    delays: () => [...pending.values()].map((entry) => entry.timeoutMs),
    async fire() {
      const due = [...pending.entries()];
      pending.clear();
      for (const [, entry] of due) entry.handler();
      await new Promise((resolve) => setImmediate(resolve));
    },
  };
}

function deferred() {
  let resolve;
  const promise = new Promise((resolvePromise) => { resolve = resolvePromise; });
  return { promise, resolve };
}

const caWait = () => loader.load("/src/features/models/caWaitPoll.ts");

test("a slow status read is not overtaken: the next tick is armed only after it settles", async () => {
  const { startCaWaitPoll } = await caWait();
  const clock = fakeTimers();
  const held = deferred();
  let reads = 0;
  let ready = false;

  startCaWaitPoll({
    refresh: () => { reads += 1; return held.promise; },
    isReady: () => ready,
    onDone: () => { ready = true; },
  }, { intervalMs: 2_000, maxAttempts: 15, timers: clock.timers });

  assert.deepEqual(clock.delays(), [2_000]);
  await clock.fire();
  assert.equal(reads, 1, "第一拍已经发出去了");
  assert.equal(clock.pendingCount(), 0, "在途期间不能排下一拍（setInterval 会，这正是要改掉的）");

  held.resolve();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(clock.pendingCount(), 1, "上一拍 settle 之后才排下一拍");
  assert.deepEqual(clock.delays(), [2_000]);
});

test("readiness ends the wait and the attempt cap ends it too", async () => {
  const { startCaWaitPoll } = await caWait();
  const readyClock = fakeTimers();
  let done = 0;
  let ready = false;
  startCaWaitPoll({
    refresh: async () => { ready = true; },
    isReady: () => ready,
    onDone: () => { done += 1; },
  }, { intervalMs: 10, maxAttempts: 15, timers: readyClock.timers });
  await readyClock.fire();
  assert.equal(done, 1, "装好即停");
  assert.equal(readyClock.pendingCount(), 0);

  const cappedClock = fakeTimers();
  let attempts = 0;
  let capped = 0;
  startCaWaitPoll({
    refresh: async () => { attempts += 1; },
    isReady: () => false,
    onDone: () => { capped += 1; },
  }, { intervalMs: 10, maxAttempts: 3, timers: cappedClock.timers });
  for (let round = 0; round < 3; round += 1) await cappedClock.fire();
  assert.equal(attempts, 3);
  assert.equal(capped, 1, "到达上限也要收尾（超时不是失败，只是停止自动等待）");
  assert.equal(cappedClock.pendingCount(), 0);
});

test("the default wiring reads the cursor status endpoint alone, not the whole batch", async () => {
  const { backend } = installAppBackend();
  loader.invalidateAll();
  const { appStore } = await loader.load("/src/shared/store/appStore.ts");
  const { startCursorCaWaitPoll } = await caWait();
  const clock = fakeTimers();

  let done = 0;
  startCursorCaWaitPoll(() => { done += 1; }, { intervalMs: 2_000, maxAttempts: 1, timers: clock.timers });
  await clock.fire();

  assert.equal(done, 1, "只等一拍就到了上限");
  assert.deepEqual(
    backend.requests.map((request) => `${request.method} ${request.path}`),
    [`GET ${API_ROOT}/harness/cursor/status`],
    "等 CA 时只读接管状态一个接口；整批 refresh 会打十二个",
  );
  assert.equal(appStore.getSnapshot().cursorHarness?.ca, "ready", "读到的状态要落进 store");
});
