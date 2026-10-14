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

test("host restore uses a freshly inspected path instead of a page-owned receipt", async () => {
  const path = "D:\\Devin files\\extension.js";
  const backend = new FakeBackend();
  backend.route("POST", `${API_ROOT}/harness/devin/host/restore`, () => ({ body: { restored: true } }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");
  const result = await api.restoreDevinHostPatch(path);
  assert.deepEqual(result, { restored: true });
  assert.deepEqual(backend.requestsFor("POST", `${API_ROOT}/harness/devin/host/restore`)[0].body, { path });
});
