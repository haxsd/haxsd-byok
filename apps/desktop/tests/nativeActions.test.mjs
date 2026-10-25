import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, FakeBackend, installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const browser = installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

test("CA installation invokes only the application's fixed native capability", async () => {
  const { api } = await loader.load("/src/shared/api.ts");
  const before = browser.tauriInvocations.length;
  await api.openCursorCaInstallTerminal();
  assert.deepEqual(browser.tauriInvocations.slice(before), [
    { command: "open_cursor_ca_install_terminal", args: {} },
  ]);
});

test("host restore uses the settings path instead of a page-owned receipt", async () => {
  const path = "D:\\Devin files\\User\\settings.json";
  const backend = new FakeBackend();
  backend.route("POST", `${API_ROOT}/harness/devin/host/restore`, () => ({ body: { restored: true } }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");
  const result = await api.restoreDevinHostConfiguration(path);
  assert.deepEqual(result, { restored: true });
  assert.deepEqual(backend.requestsFor("POST", `${API_ROOT}/harness/devin/host/restore`)[0].body, { path });
});

test("Devin host requests carry a custom installation root", async () => {
  const path = "D:\\Devin files\\User\\settings.json";
  const installRoot = "D:\\devin\\Devin";
  const backend = new FakeBackend();
  backend.route("GET", `${API_ROOT}/harness/devin/host/status`, () => ({ body: { path } }));
  backend.route("GET", `${API_ROOT}/harness/devin/model-uids`, () => ({ body: { path, choices: [] } }));
  backend.route("POST", `${API_ROOT}/harness/devin/host/apply`, () => ({ status: 204 }));
  backend.route("POST", `${API_ROOT}/harness/devin/host/restore`, () => ({ body: { restored: true } }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");

  await api.devinHostStatus(path, ` ${installRoot} `);
  await api.devinModelUids(installRoot);
  await api.applyDevinHostConfiguration(path, installRoot);
  await api.restoreDevinHostConfiguration(path, installRoot);

  const [status] = backend.requestsFor("GET", `${API_ROOT}/harness/devin/host/status`);
  const [uids] = backend.requestsFor("GET", `${API_ROOT}/harness/devin/model-uids`);
  const [apply] = backend.requestsFor("POST", `${API_ROOT}/harness/devin/host/apply`);
  const [restore] = backend.requestsFor("POST", `${API_ROOT}/harness/devin/host/restore`);
  assert.deepEqual(Object.fromEntries(new URLSearchParams(status.search)), { path, install_root: installRoot });
  assert.deepEqual(Object.fromEntries(new URLSearchParams(uids.search)), { install_root: installRoot });
  assert.deepEqual(apply.body, { path, install_root: installRoot });
  assert.deepEqual(restore.body, { path, install_root: installRoot });
});
