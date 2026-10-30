// Behavior tests for the tutorial onboarding checklist.
//
// The checklist follows the client the user picked: only Cursor, Devin, Codex or
// Claude is counted, and the progress denominator contains the displayed steps
// only. A client the user did not select must never appear as an unfinished step.

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
    client: null,
    modelCount: 0,
    cursor: null,
    devin: null,
    codex: null,
    claude: null,
    hostConfigured: false,
    callCounts: {},
    ...overrides,
  };
}

function cursorHarness(overrides = {}) {
  return {
    caReady: true,
    settingsApplied: true,
    mode: "mixed",
    ...overrides,
  };
}

function devinStatus(overrides = {}) {
  return { enabled: true, listening: true, ready: true, ...overrides };
}

function codexStatus(overrides = {}) {
  return { enabled: true, listening: true, configured: true, ...overrides };
}

function claudeStatus(overrides = {}) {
  return { enabled: true, listening: true, cli: "ready", desktop: "ready", ...overrides };
}

test("before a client is picked the checklist holds only the shared first step", async () => {
  const buildOnboarding = await loadOnboarding();

  const view = buildOnboarding(input());

  assert.deepEqual(view.steps.map((step) => step.key), ["model"]);
  assert.equal(view.total, 1, "the client steps are not part of the checklist yet");
  assert.equal(view.completed, 0);
});

test("picking Cursor adds the Cursor step and never counts Devin", async () => {
  const buildOnboarding = await loadOnboarding();

  const view = buildOnboarding(input({
    client: "cursor",
    modelCount: 2,
    cursor: cursorHarness(),
    devin: devinStatus({ enabled: false, listening: false, ready: false }),
    hostConfigured: false,
    callCounts: { cursor: 4 },
  }));

  assert.deepEqual(view.steps.map((step) => step.key), ["model", "cursor", "verify"]);
  assert.equal(view.total, 3, "Devin's unfinished steps are not in the denominator");
  assert.equal(view.completed, 3, "the untouched Devin module does not hold the progress back");
});

test("picking Devin adds the Devin step and tolerates a missing Cursor status", async () => {
  const buildOnboarding = await loadOnboarding();

  const view = buildOnboarding(input({
    client: "devin",
    modelCount: 1,
    cursor: null,
    devin: devinStatus(),
    hostConfigured: true,
    callCounts: { devin: 1 },
  }));

  assert.deepEqual(view.steps.map((step) => step.key), ["model", "devin", "verify"]);
  assert.equal(view.total, 3);
  assert.equal(view.completed, 3, "an unknown Cursor harness must not fail the Devin path");
});

test("the Cursor step is done only when the mode, the settings and the CA are ready", async () => {
  const buildOnboarding = await loadOnboarding();

  const rows = [
    { harness: cursorHarness({ mode: "official" }), done: false, why: "official mode keeps the app out of the path" },
    { harness: cursorHarness({ settingsApplied: false }), done: false, why: "the proxy settings were not applied" },
    { harness: cursorHarness({ caReady: false }), done: false, why: "requests cannot be read without the local CA" },
    { harness: cursorHarness(), done: true, why: "everything is in place" },
    { harness: null, done: false, why: "no status has been read yet" },
  ];

  for (const row of rows) {
    const view = buildOnboarding(input({ client: "cursor", modelCount: 1, cursor: row.harness }));
    const step = view.steps.find((item) => item.key === "cursor");
    assert.equal(step.done, row.done, row.why);
  }
});

test("the Devin step is done only when the gateway is ready and Devin is configured", async () => {
  const buildOnboarding = await loadOnboarding();

  const rows = [
    { status: devinStatus({ enabled: false, listening: false, ready: false }), hostConfigured: true, done: false, why: "the gateway is off" },
    { status: devinStatus({ listening: false, ready: false }), hostConfigured: true, done: false, why: "the saved gateway is waiting for a restart" },
    { status: devinStatus({ listening: true, ready: false }), hostConfigured: true, done: false, why: "the process still holds the previous ports" },
    { status: devinStatus(), hostConfigured: false, done: false, why: "Devin still points at the vendor" },
    { status: devinStatus(), hostConfigured: true, done: true, why: "gateway and Devin settings are both in place" },
    { status: null, hostConfigured: true, done: false, why: "no status has been read yet" },
  ];

  for (const row of rows) {
    const view = buildOnboarding(input({ client: "devin", modelCount: 1, devin: row.status, hostConfigured: row.hostConfigured }));
    const step = view.steps.find((item) => item.key === "devin");
    assert.equal(step.done, row.done, row.why);
  }
});

test("picking Codex adds the Codex step and never counts the other clients", async () => {
  const buildOnboarding = await loadOnboarding();

  const view = buildOnboarding(input({
    client: "codex",
    modelCount: 1,
    cursor: cursorHarness({ mode: "official" }),
    devin: devinStatus({ enabled: false, listening: false, ready: false }),
    codex: codexStatus(),
    callCounts: { codex: 2 },
  }));

  assert.deepEqual(view.steps.map((step) => step.key), ["model", "codex", "verify"]);
  assert.equal(view.total, 3, "the untouched Cursor and Devin modules are not in the denominator");
  assert.equal(view.completed, 3);
});

test("the Codex step is done only when the gateway runs and the config is applied", async () => {
  const buildOnboarding = await loadOnboarding();

  const rows = [
    { status: codexStatus({ enabled: false, listening: false, configured: false }), done: false, why: "the gateway is off" },
    { status: codexStatus({ listening: false }), done: false, why: "the saved gateway is waiting for a restart" },
    { status: codexStatus({ configured: false }), done: false, why: "the client config has not been applied yet" },
    { status: codexStatus(), done: true, why: "gateway and client config are both in place" },
    { status: null, done: false, why: "no status has been read yet" },
  ];

  for (const row of rows) {
    const view = buildOnboarding(input({ client: "codex", modelCount: 1, codex: row.status }));
    const step = view.steps.find((item) => item.key === "codex");
    assert.equal(step.done, row.done, row.why);
  }
});

test("picking Claude adds the Claude step and never counts the other clients", async () => {
  const buildOnboarding = await loadOnboarding();

  const view = buildOnboarding(input({
    client: "claude",
    modelCount: 1,
    cursor: cursorHarness({ mode: "official" }),
    devin: devinStatus({ enabled: false, listening: false, ready: false }),
    codex: codexStatus({ enabled: false, listening: false, configured: false }),
    claude: claudeStatus({ cli: "applied" }),
    callCounts: { claude: 1 },
  }));

  assert.deepEqual(view.steps.map((step) => step.key), ["model", "claude", "verify"]);
  assert.equal(view.total, 3, "the untouched Cursor, Devin and Codex modules are not in the denominator");
  assert.equal(view.completed, 3);
});

test("the Claude step accepts either client but surfaces conflicts and stale takeovers", async () => {
  const buildOnboarding = await loadOnboarding();

  const rows = [
    { value: null, done: false, status: "读取中…", why: "no status has been read yet" },
    { value: claudeStatus({ enabled: false, listening: false }), done: false, status: "网关未启用" },
    { value: claudeStatus({ listening: false }), done: false, status: "网关已启用，等待端口就绪" },
    { value: claudeStatus({ cli: "applied", desktop: "missing" }), done: true, status: "已接管" },
    { value: claudeStatus({ cli: "conflict", desktop: "applied" }), done: false, status: "配置冲突" },
    { value: claudeStatus({ cli: "stale", desktop: "applied" }), done: false, status: "配置已过期" },
    { value: claudeStatus({ cli: "ready", desktop: "running" }), done: false, status: "先完全退出 Claude Desktop" },
    { value: claudeStatus({ cli: "managed", desktop: "missing" }), done: false, status: "受管配置，按页面指引处理" },
    { value: claudeStatus(), done: false, status: "网关在运行，还差应用接管" },
  ];

  for (const row of rows) {
    const view = buildOnboarding(input({ client: "claude", modelCount: 1, claude: row.value }));
    const step = view.steps.find((item) => item.key === "claude");
    assert.equal(step.done, row.done, row.why);
    assert.equal(step.status, row.status, row.why);
  }
});

test("every displayed step carries the fields the checklist renders", async () => {
  const buildOnboarding = await loadOnboarding();

  for (const client of ["cursor", "devin", "codex", "claude"]) {
    const view = buildOnboarding(input({ client }));
    const keys = view.steps.map((step) => step.key);
    assert.equal(new Set(keys).size, keys.length, "step keys are unique");
    for (const step of view.steps) {
      assert.ok(step.title.length > 0, `${step.key} has a title`);
      assert.ok(step.status.length > 0, `${step.key} has a status line`);
      assert.ok(step.path.startsWith("/"), `${step.key} links to a page`);
      assert.ok(step.action.length > 0, `${step.key} has an action label`);
    }
  }
});

test("a call from another client never completes the selected client's verification", async () => {
  const buildOnboarding = await loadOnboarding();

  const view = buildOnboarding(input({
    client: "cursor",
    modelCount: 1,
    cursor: cursorHarness(),
    callCounts: { devin: 3, codex: 2 },
  }));

  const verify = view.steps.find((step) => step.key === "verify");
  assert.equal(verify.done, false);
  assert.equal(verify.status, "还没有调用记录");
});
