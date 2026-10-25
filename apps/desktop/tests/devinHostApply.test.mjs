// 「写入 Devin 设置」的两段式：写失败与「写成功但状态读不回来」必须分开。
//
// 服务端先落盘、再读回状态。读状态失败时设置已经写下去了，不能报成写入失败，
// 否则用户重复写入一次。这一组用例把三种结局钉住。

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const moduleUnderTest = () => loader.load("/src/features/devin/devinHostApply.ts");
const HOST = "C:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json";
const ROOT = "D:\\Devin";

const statusFixture = (overrides = {}) => ({
  path: HOST,
  compatible: true,
  unconfigured: false,
  configured: true,
  parts: { api: true, inference: true },
  ports: { api_port: 43_110, inference_port: 43_111 },
  backup_path: `${HOST}.devin-router.backup`,
  backup_available: true,
  current_sha256: "patched",
  backup_sha256: "original",
  restore_receipt: {
    path: HOST,
    backup_path: `${HOST}.devin-router.backup`,
    original_sha256: "original",
    patched_sha256: "patched",
    ports: { api_port: 43_110, inference_port: 43_111 },
  },
  extension: {
    path: "D:\\devin\\Devin\\resources\\app\\extensions\\windsurf\\dist\\extension.js",
    version: "3.11.216",
    supported_sha256: "A063DAE82C91328E17AE16121E90780087674B65839B5AD39915C9C59C652FF0",
    compatible: true,
    clean: false,
    patched: true,
    backup_path: "D:\\devin\\Devin\\resources\\app\\extensions\\windsurf\\dist\\extension.js.devin-router-gate.backup",
    backup_available: true,
    current_sha256: "patched-extension",
    backup_sha256: "original-extension",
    restore_receipt: {
      path: "D:\\devin\\Devin\\resources\\app\\extensions\\windsurf\\dist\\extension.js",
      backup_path: "D:\\devin\\Devin\\resources\\app\\extensions\\windsurf\\dist\\extension.js.devin-router-gate.backup",
      version: "3.11.216",
      original_sha256: "original-extension",
      patched_sha256: "patched-extension",
    },
    message: "",
  },
  message: "",
  ...overrides,
});

test("a refused write keeps the settings file untouched and reports why", async () => {
  const { backend } = installAppBackend();
  backend.route("POST", `${API_ROOT}/harness/devin/host/apply`, () => ({ status: 500, body: { message: "目标文件被占用" } }));
  const { applyDevinHostConfiguration } = await moduleUnderTest();

  const outcome = await applyDevinHostConfiguration(HOST);

  assert.equal(outcome.kind, "write-failed");
  assert.match(String(outcome.cause), /目标文件被占用/);
});

test("a written configuration whose status cannot be read is not reported as a failure", async () => {
  installAppBackend();
  const { backend } = installAppBackend();
  backend.route("POST", `${API_ROOT}/harness/devin/host/apply`, () => ({ status: 204 }));
  backend.route("GET", `${API_ROOT}/harness/devin/host/status`, () => ({ status: 500, body: { message: "状态读取超时" } }));
  const { applyDevinHostConfiguration } = await moduleUnderTest();

  const outcome = await applyDevinHostConfiguration(HOST);

  assert.equal(outcome.kind, "status-unreadable", "设置已落盘，这一支不能说成写失败");
  assert.equal(backend.requestsFor("POST", `${API_ROOT}/harness/devin/host/apply`).length, 1, "设置确实写下去了");
});

test("only a readable status ends as applied", async () => {
  installAppBackend();
  const { backend } = installAppBackend();
  backend.route("POST", `${API_ROOT}/harness/devin/host/apply`, () => ({ status: 204 }));
  backend.route("GET", `${API_ROOT}/harness/devin/host/status`, () => ({ body: statusFixture() }));
  const { applyDevinHostConfiguration } = await moduleUnderTest();

  const outcome = await applyDevinHostConfiguration(HOST);

  assert.equal(outcome.kind, "applied");
  assert.equal(outcome.status.configured, true);
  assert.equal(outcome.status.path, HOST);
  assert.deepEqual(
    backend.requests.map((entry) => `${entry.method} ${entry.path}`),
    [`POST ${API_ROOT}/harness/devin/host/apply`, `GET ${API_ROOT}/harness/devin/host/status`],
    "先写、后读状态",
  );
});

test("an extension-only receipt still makes restore available", async () => {
  const { canRestoreDevinHost } = await moduleUnderTest();
  const status = statusFixture({ restore_receipt: null });

  assert.equal(canRestoreDevinHost(status), true);
  assert.equal(canRestoreDevinHost({ ...status, extension: { ...status.extension, restore_receipt: null } }), false);
});

test("the installation root reaches both the write and the status read", async () => {
  const { applyDevinHostConfiguration } = await moduleUnderTest();
  const calls = [];
  const outcome = await applyDevinHostConfiguration(HOST, ROOT, {
    apply: async (path, root) => {
      calls.push(["apply", path, root]);
    },
    readStatus: async (path, root) => {
      calls.push(["status", path, root]);
      return statusFixture();
    },
  });

  assert.equal(outcome.kind, "applied");
  assert.deepEqual(calls, [
    ["apply", HOST, ROOT],
    ["status", HOST, ROOT],
  ]);
});
