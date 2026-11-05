// Behavior tests for the top bar status derivations.
//
// The global status answers one question: is the local management service
// reachable. A client module that is switched off belongs to that client's own
// entry in the navigation, never to the global pill.

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

async function loadStatus() {
  loader.invalidateAll();
  return loader.load("/src/shell/connectionStatus.ts");
}

test("the global status follows offline only", async () => {
  const { localServiceStatus } = await loadStatus();

  const online = localServiceStatus(false);
  assert.equal(online.state, "connected");
  assert.equal(online.tone, "ok");

  const offline = localServiceStatus(true);
  assert.equal(offline.state, "disconnected");
  assert.equal(offline.tone, "bad");
});

test("a client entry without a status row reports nothing", async () => {
  const { cursorEntryStatus, devinEntryStatus } = await loadStatus();

  assert.equal(cursorEntryStatus(null), null);
  assert.equal(devinEntryStatus(null, null), null);
});

test("the Cursor entry describes Cursor's own mode", async () => {
  const { cursorEntryStatus } = await loadStatus();

  const rows = [
    { harness: { mode: "official", settings_applied: true }, state: "official", tone: "idle" },
    { harness: { mode: "official", settings_applied: false }, state: "official", tone: "idle", why: "official mode is not a failure" },
    { harness: { mode: "local", settings_applied: true }, state: "taken-over", tone: "ok" },
    { harness: { mode: "mixed", settings_applied: true }, state: "taken-over", tone: "ok" },
    { harness: { mode: "local", settings_applied: false }, state: "not-taken-over", tone: "idle" },
    { harness: { mode: "mixed", settings_applied: false }, state: "not-taken-over", tone: "idle" },
  ];

  for (const row of rows) {
    const result = cursorEntryStatus(row.harness);
    assert.equal(result.state, row.state, row.why ?? JSON.stringify(row.harness));
    assert.equal(result.tone, row.tone, row.why ?? JSON.stringify(row.harness));
  }
});

test("the Devin entry describes the gateway's own lifecycle", async () => {
  const { devinEntryStatus } = await loadStatus();

  const rows = [
    { status: { enabled: false, listening: false, ready: false }, state: "disabled", tone: "idle" },
    { status: { enabled: false, listening: true, ready: false }, state: "disabled-listening", tone: "warn", why: "saved off but the process still answers on its ports" },
    { status: { enabled: true, listening: false, ready: false }, state: "pending-restart", tone: "warn" },
    { status: { enabled: true, listening: true, ready: false }, state: "pending-restart", tone: "warn", why: "the process still holds the previous ports" },
    { status: { enabled: true, listening: true, ready: true }, state: "checking", tone: "warn" },
  ];

  for (const row of rows) {
    const result = devinEntryStatus(row.status, null);
    assert.equal(result.state, row.state, row.why ?? JSON.stringify(row.status));
    assert.equal(result.tone, row.tone, row.why ?? JSON.stringify(row.status));
  }
});

test("a running Devin gateway is not taken over until its host points here with the patch", async () => {
  const { devinEntryStatus } = await loadStatus();
  const status = {
    enabled: true, listening: true, ready: true,
    bound_ports: { api: 44110, inference: 44111, local_api: 44112 },
  };
  const connectedHost = {
    configured: true,
    ports: { api_port: 44110, inference_port: 44111 },
    extension: { patched: true },
  };

  assert.equal(devinEntryStatus(status, null).state, "checking");
  for (const host of [
    { ...connectedHost, configured: false, ports: null },
    { ...connectedHost, extension: { patched: false } },
    { ...connectedHost, ports: { api_port: 45110, inference_port: 45111 } },
  ]) {
    const result = devinEntryStatus(status, host);
    assert.equal(result.state, "pending-configuration");
    assert.equal(result.tone, "warn");
    assert.equal(result.label, "待完成");
  }
  const result = devinEntryStatus(status, connectedHost);
  assert.equal(result.state, "taken-over");
  assert.equal(result.label, "已接管");
  assert.equal(result.tone, "ok");
});
