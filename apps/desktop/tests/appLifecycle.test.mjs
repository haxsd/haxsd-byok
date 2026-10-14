// Behavior tests for the desktop lifecycle preferences (silent start, Dock icon).
//
// These run the real module against a fake management server and a fake Tauri
// window: the assertions are the requests it sends and the state the server keeps.

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, deferred, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend } from "./appBackend.mjs";

const doubles = installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

async function loadAppLifecycle() {
  loader.invalidateAll();
  return loader.load("/src/shared/native/appLifecycle.ts");
}

test("silent start and Dock icon saved at the same time keep both settings", async () => {
  const { backend, state } = installAppBackend({
    desktop: { silent_start: false, show_dock_icon: true, developer_mode: false },
  });
  // Two saves racing the way the settings card can produce them: each one used to
  // read the whole object first, so both wrote the same pre-change snapshot back.
  // The reads are held open to make that ordering deterministic.
  const firstRead = deferred();
  const secondRead = deferred();
  let reads = 0;
  const initial = { silent_start: false, show_dock_icon: true, developer_mode: false };
  backend.route("GET", `${API_ROOT}/settings/desktop`, async () => {
    reads += 1;
    return reads === 1 ? firstRead.promise : secondRead.promise;
  });

  const { writeSilentStart, writeDockIconVisibility } = await loadAppLifecycle();
  const silent = writeSilentStart(true);
  const dock = writeDockIconVisibility(false);
  firstRead.resolve({ body: initial });
  secondRead.resolve({ body: initial });
  await Promise.all([silent, dock]);

  assert.deepEqual(
    state.desktop,
    { silent_start: true, show_dock_icon: false, developer_mode: false },
    "saving one preference must not undo the other",
  );
  assert.equal(
    backend.requestsFor("GET", `${API_ROOT}/settings/desktop`).length,
    0,
    "a preference save must not read the whole settings object first",
  );
});

test("a silent start save carries only its own field", async () => {
  const { backend, state } = installAppBackend({
    desktop: { silent_start: false, show_dock_icon: false, developer_mode: true },
  });

  const { writeSilentStart } = await loadAppLifecycle();
  await writeSilentStart(true);

  const writes = backend.requestsFor("PUT", `${API_ROOT}/settings/desktop`);
  assert.equal(writes.length, 1);
  assert.deepEqual(writes[0].body, { silent_start: true });
  assert.deepEqual(
    state.desktop,
    { silent_start: true, show_dock_icon: false, developer_mode: true },
    "the fields this save did not mention must stay as they were",
  );
});

test("a failed Dock icon save restores the visibility the server still stores", async () => {
  const { backend, state } = installAppBackend({
    desktop: { silent_start: false, show_dock_icon: true, developer_mode: false },
  });
  backend.route("PUT", `${API_ROOT}/settings/desktop`, () => ({ status: 500, body: { message: "settings rejected" } }));
  const before = doubles.tauriInvocations.length;

  const { writeDockIconVisibility } = await loadAppLifecycle();
  await assert.rejects(() => writeDockIconVisibility(false), /settings rejected/);

  const touched = backend.requests.filter((request) => request.path === `${API_ROOT}/settings/desktop`);
  assert.equal(touched[0]?.method, "PUT", "the native change must not start with reading the settings object");
  assert.deepEqual(
    state.desktop,
    { silent_start: false, show_dock_icon: true, developer_mode: false },
    "a rejected save must not change what the server stores",
  );
  assert.deepEqual(
    doubles.tauriInvocations.slice(before).map((invocation) => invocation.args),
    [{ visible: false }, { visible: true }],
    "the Dock icon must go back to the stored value after the save fails",
  );
});
