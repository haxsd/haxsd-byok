// 设备码授权轮询链：串行、可停、停了之后在途的那一次不会再续排。
//
// 旧实现（effect 内联）只清定时器：正在飞的 pluginOAuthPoll 返回后仍会 setTimeout 再排
// 一个，于是 effect 每次重启都留下一条并行轮询链。这一组用例把「取消位」钉住。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

/** 把时间握在手里的定时器替身。 */
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
    /** 触发所有待发定时器并让它们的微任务跑完。 */
    async fire() {
      const due = [...pending.entries()];
      pending.clear();
      for (const [, entry] of due) entry.handler();
      await settle();
    },
  };
}

const settle = () => new Promise((resolve) => setImmediate(resolve));

function deferred() {
  let resolve;
  const promise = new Promise((resolvePromise) => { resolve = resolvePromise; });
  return { promise, resolve };
}

function recorder() {
  const events = [];
  return {
    events,
    handlers: {
      poll: null,
      onPending: () => events.push("pending"),
      onCompleted: (modelSyncError) => events.push(`completed:${modelSyncError ?? "-"}`),
      onFailed: (message) => events.push(`failed:${message ?? "-"}`),
      onTransportError: (cause) => events.push(`transport:${cause.message}`),
    },
  };
}

test("a pending answer re-arms the chain at the interval the server asked for, never below one second", async () => {
  const { startOAuthPoll } = await loader.load("/src/features/plugins/oauthPoll.ts");
  const clock = fakeTimers();
  const log = recorder();
  let polls = 0;
  log.handlers.poll = async () => { polls += 1; return { status: "pending", pollIntervalMs: 250 }; };

  startOAuthPoll(log.handlers, 1_500, clock.timers);
  assert.deepEqual(clock.delays(), [1_500], "第一拍用服务端给的间隔");
  await clock.fire();

  assert.equal(polls, 1);
  assert.deepEqual(log.events, ["pending"]);
  assert.deepEqual(clock.delays(), [1_000], "低于 1 秒的间隔被夹到 1 秒，避免把本机服务问成压力测试");
});

test("stopping the chain keeps an in-flight answer from arming another timer", async () => {
  const { startOAuthPoll } = await loader.load("/src/features/plugins/oauthPoll.ts");
  const clock = fakeTimers();
  const log = recorder();
  const answer = deferred();
  let polls = 0;
  log.handlers.poll = () => { polls += 1; return answer.promise; };

  const stop = startOAuthPoll(log.handlers, 1_000, clock.timers);
  await clock.fire();
  assert.equal(polls, 1, "第一拍已经发出去了");

  stop();
  answer.resolve({ status: "pending", pollIntervalMs: 1_000 });
  await settle();

  assert.equal(clock.pendingCount(), 0, "停了之后在途的那一次不能再排下一拍");
  assert.equal(polls, 1, "也不能再发起新的轮询");
  assert.deepEqual(log.events, [], "已经停掉的链不该再往界面上写状态");
});

test("stopping before the first tick clears the timer outright", async () => {
  const { startOAuthPoll } = await loader.load("/src/features/plugins/oauthPoll.ts");
  const clock = fakeTimers();
  const log = recorder();
  log.handlers.poll = async () => ({ status: "pending", pollIntervalMs: 1_000 });

  const stop = startOAuthPoll(log.handlers, 1_000, clock.timers);
  assert.equal(clock.pendingCount(), 1);
  stop();
  assert.equal(clock.pendingCount(), 0, "卸载时定时器必须清零");
});

test("a completed answer ends the chain while a denial reports its reason", async () => {
  const { startOAuthPoll } = await loader.load("/src/features/plugins/oauthPoll.ts");
  const completedClock = fakeTimers();
  const completed = recorder();
  completed.handlers.poll = async () => ({ status: "completed", added: 1, updated: 0, modelSyncError: null });
  startOAuthPoll(completed.handlers, 1_000, completedClock.timers);
  await completedClock.fire();
  assert.deepEqual(completed.events, ["completed:-"]);
  assert.equal(completedClock.pendingCount(), 0, "授权完成后不再轮询");

  const deniedClock = fakeTimers();
  const denied = recorder();
  denied.handlers.poll = async () => ({ status: "denied", message: "用户在网页上点了拒绝" });
  startOAuthPoll(denied.handlers, 1_000, deniedClock.timers);
  await deniedClock.fire();
  assert.deepEqual(denied.events, ["failed:用户在网页上点了拒绝"]);
  assert.equal(deniedClock.pendingCount(), 0, "被拒绝也不该继续轮询");
});

test("a transport error is reported and the same interval is retried", async () => {
  const { startOAuthPoll } = await loader.load("/src/features/plugins/oauthPoll.ts");
  const clock = fakeTimers();
  const log = recorder();
  let attempts = 0;
  log.handlers.poll = async () => {
    attempts += 1;
    throw new Error("服务不可达");
  };

  startOAuthPoll(log.handlers, 2_000, clock.timers);
  await clock.fire();
  assert.equal(attempts, 1);
  assert.deepEqual(log.events, ["transport:服务不可达"]);
  assert.deepEqual(clock.delays(), [2_000], "一次失败不该改变重试节奏");
});
