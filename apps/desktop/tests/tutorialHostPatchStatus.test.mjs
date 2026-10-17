// Behavior tests for the tutorial's Devin host-patch step (features/tutorial).
//
// 宿主补丁的状态是单独读的（`api.devinHostStatus()`），读不到时清单以前直接把它当成
// 「补丁没打」，于是用户会去重打一次补丁——一个会改写 Devin 宿主文件的动作。这里把
// 三种情形钉住：
//
// 1. 读取失败：状态说读取失败，不说「还差宿主补丁」；
// 2. 正在读取：状态说读取中，不说「还差宿主补丁」；
// 3. 读到结果：才按补丁的真实状态说「已接通 / 还差宿主补丁」。
//
// 网关本身的状态优先于补丁状态：网关没启用时，下一步是先启网关，补丁读到没读到都不
// 改变这一步的说法。断言只看清单渲染用的字段，不碰样式。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

async function loadOnboarding() {
  loader.invalidateAll();
  const module = await loader.load("/src/features/tutorial/onboarding.ts");
  return module.buildOnboarding;
}

function input(overrides = {}) {
  return {
    client: "devin",
    modelCount: 1,
    cursor: null,
    devin: { enabled: true, listening: true },
    codex: null,
    hostPatched: false,
    callCount: 0,
    ...overrides,
  };
}

async function devinStep(overrides) {
  const buildOnboarding = await loadOnboarding();
  return buildOnboarding(input(overrides)).steps.find((step) => step.key === "devin");
}

test("读不到宿主补丁状态时说读取失败，不说还差宿主补丁", async () => {
  const step = await devinStep({ hostPatchRead: "failed" });
  assert.equal(step.status, "读取失败");
  assert.notEqual(step.status, "网关在运行，还差宿主补丁");
  assert.equal(step.done, false, "读不到就不能算完成");
});

test("正在读取时说读取中，不说还差宿主补丁", async () => {
  const step = await devinStep({ hostPatchRead: "reading" });
  assert.equal(step.status, "读取中…");
  assert.equal(step.done, false);
});

test("读到结果后才按补丁的真实状态说", async () => {
  assert.equal((await devinStep({ hostPatchRead: null, hostPatched: true })).status, "已接通");
  assert.equal((await devinStep({ hostPatchRead: null, hostPatched: false })).status, "网关在运行，还差宿主补丁");
  // 旧调用方不传这个字段时行为不变。
  assert.equal((await devinStep({ hostPatched: false })).status, "网关在运行，还差宿主补丁");
  assert.equal((await devinStep({ hostPatched: true })).status, "已接通");
});

test("网关本身没就绪时，补丁状态读到什么都先说要先做的事", async () => {
  assert.equal((await devinStep({ devin: { enabled: false, listening: false }, hostPatchRead: "failed" })).status, "网关未启用");
  assert.equal((await devinStep({ devin: { enabled: true, listening: false }, hostPatchRead: "failed" })).status, "网关已启用，等待端口就绪");
});
