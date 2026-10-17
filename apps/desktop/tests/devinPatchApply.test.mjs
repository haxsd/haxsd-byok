// 「应用 Devin 宿主补丁」的两段式：写失败与「写成功但状态读不回来」必须分开。
//
// 旧实现把 apply 与 status 放在同一个 try 里：读状态失败会报成"应用失败"，而补丁其实已经
// 落盘了——用户被诱导再点一次，第二次会被服务端以"已经打过补丁"拒绝。这一组用例把三种
// 结局钉住。

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const patch = () => loader.load("/src/features/devin/devinPatchApply.ts");
const HOST = "C:\\Users\\me\\AppData\\Local\\Programs\\Devin\\resources\\app\\out\\main.js";

const statusFixture = (overrides = {}) => ({
  path: HOST,
  applied: true,
  patched_at_ms: 1,
  restore_receipt: { path: HOST, created_at_ms: 1 },
  ...overrides,
});

test("a refused write keeps the file untouched and reports why", async () => {
  const { backend } = installAppBackend();
  backend.route("POST", `${API_ROOT}/harness/devin/host/apply`, () => ({ status: 500, body: { message: "目标文件被占用" } }));
  const { applyDevinHostPatch } = await patch();

  const outcome = await applyDevinHostPatch(HOST);

  assert.equal(outcome.kind, "write-failed");
  assert.match(String(outcome.cause), /目标文件被占用/);
});

test("a written patch whose status cannot be read is not reported as a failure", async () => {
  const { backend } = installAppBackend();
  backend.route("POST", `${API_ROOT}/harness/devin/host/apply`, () => ({ status: 204 }));
  backend.route("GET", `${API_ROOT}/harness/devin/host/status`, () => ({ status: 500, body: { message: "状态读取超时" } }));
  const { applyDevinHostPatch } = await patch();

  const outcome = await applyDevinHostPatch(HOST);

  assert.equal(outcome.kind, "status-unreadable", "补丁已落盘，这一支不能说成写失败");
  assert.equal(backend.requestsFor("POST", `${API_ROOT}/harness/devin/host/apply`).length, 1, "补丁确实写下去了");
});

test("only a readable status ends as applied", async () => {
  const { backend } = installAppBackend();
  backend.route("POST", `${API_ROOT}/harness/devin/host/apply`, () => ({ status: 204 }));
  backend.route("GET", `${API_ROOT}/harness/devin/host/status`, () => ({ body: statusFixture() }));
  const { applyDevinHostPatch } = await patch();

  const outcome = await applyDevinHostPatch(HOST);

  assert.equal(outcome.kind, "applied");
  assert.equal(outcome.status.applied, true);
  assert.equal(outcome.status.path, HOST);
  assert.deepEqual(
    backend.requests.map((entry) => `${entry.method} ${entry.path}`),
    [`POST ${API_ROOT}/harness/devin/host/apply`, `GET ${API_ROOT}/harness/devin/host/status`],
    "先写、后读状态",
  );
});
