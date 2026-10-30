// Behavior tests for the tutorial's Devin configuration step.
//
// 设置状态是单独读的（`api.devinHostStatus()`），读不到时清单以前直接把它当成
// 「设置没写入」，于是用户会重复写一次 Devin 配置。这里把三种情形钉住：
//
// 1. 读取失败：状态说读取失败，不说「还差写入 Devin 设置」；
// 2. 正在读取：状态说读取中，不说「还差写入 Devin 设置」；
// 3. 读到结果：才按真实状态说「已接通 / 还差写入 Devin 设置」。
//
// 网关本身的状态优先于设置状态：网关没启用时，下一步是先启网关。

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
    devin: { enabled: true, listening: true, ready: true },
    codex: null,
    claude: null,
    hostConfigured: false,
    callCounts: {},
    ...overrides,
  };
}

async function devinStep(overrides) {
  const buildOnboarding = await loadOnboarding();
  return buildOnboarding(input(overrides)).steps.find((step) => step.key === "devin");
}

test("读不到设置状态时说读取失败，不说还差写入设置", async () => {
  const step = await devinStep({ hostConfigRead: "failed" });
  assert.equal(step.status, "读取失败");
  assert.notEqual(step.status, "网关已就绪，还差写入 Devin 设置");
  assert.equal(step.done, false, "读不到就不能算完成");
});

test("正在读取时说读取中，不说还差写入设置", async () => {
  const step = await devinStep({ hostConfigRead: "reading" });
  assert.equal(step.status, "读取中…");
  assert.equal(step.done, false);
});

test("读到结果后才按设置的真实状态说", async () => {
  assert.equal((await devinStep({ hostConfigRead: null, hostConfigured: true })).status, "已接通");
  assert.equal((await devinStep({ hostConfigRead: null, hostConfigured: false })).status, "网关已就绪，还差写入 Devin 设置");
  // 未传读取状态的调用方按已有结果判断。
  assert.equal((await devinStep({ hostConfigured: false })).status, "网关已就绪，还差写入 Devin 设置");
  assert.equal((await devinStep({ hostConfigured: true })).status, "已接通");
});

test("网关本身没就绪时，设置状态读到什么都先说要先做的事", async () => {
  assert.equal((await devinStep({ devin: { enabled: false, listening: false, ready: false }, hostConfigRead: "failed" })).status, "网关未启用");
  assert.equal((await devinStep({ devin: { enabled: true, listening: false, ready: false }, hostConfigRead: "failed" })).status, "网关已启用，等待端口就绪");
  assert.equal((await devinStep({ devin: { enabled: true, listening: true, ready: false }, hostConfigRead: "failed" })).status, "端口已更改，重启后生效");
});
