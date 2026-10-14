// Behavior tests for the application store: which value wins when a save and a
// refresh overlap, and what a settings save sends to the server.
//
// The store is a module singleton, so every test re-loads the real module through
// the Vite loader and talks to a fresh fake management server.

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, deferred, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { defaultPricing, installAppBackend, llmCallFixture } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

async function loadAppStore() {
  loader.invalidateAll();
  const module = await loader.load("/src/shared/store/appStore.ts");
  return module.appStore;
}

test("old refresh (started before the saves) must not overwrite any freshly saved setting", async () => {
  const { backend, state } = installAppBackend();
  state.calls = [llmCallFixture()];
  // Hold one read open: the whole batch is still pending while the three saves
  // below complete, and it answers with the values it read beforehand.
  const heldRead = deferred();
  const stalePricing = structuredClone(state.pricing);
  backend.route("GET", `${API_ROOT}/settings/pricing`, async () => heldRead.promise);

  const appStore = await loadAppStore();
  const refresh = appStore.refresh();

  assert.equal(await appStore.updatePorts({ proxy_port: 8090, service_port: 4200 }), true);
  await appStore.updateDetailed(true);
  assert.equal(await appStore.updateDeveloperMode(true), true);
  const customPricing = structuredClone(defaultPricing());
  customPricing.models["hash-1"].mode = "fixed";
  assert.equal(await appStore.updatePricingSettings("hash-1", customPricing.models["hash-1"]), true);

  heldRead.resolve({ body: stalePricing });
  await refresh;

  const snapshot = appStore.getSnapshot();
  assert.deepEqual(snapshot.ports, { proxy_port: 8090, service_port: 4200 }, "the saved ports must survive the stale refresh");
  assert.equal(snapshot.detailed, true, "the saved detailed switch must survive the stale refresh");
  assert.equal(snapshot.developerMode, true, "the saved developer mode must survive the stale refresh");
  assert.deepEqual(snapshot.pricing, customPricing, "the saved pricing must survive the stale refresh");
  assert.equal(snapshot.calls.length, 1, "the rest of the same batch must still be applied");
  assert.equal(snapshot.busy, false, "the discarded refresh must not leave the UI spinning");
});

test("refresh started while a save is in flight must not roll the saved value back", async () => {
  const { backend, state } = installAppBackend();
  const heldSave = deferred();
  // The save is accepted by the network but its answer arrives late.
  backend.route("PUT", `${API_ROOT}/settings/ports`, async (request) => {
    await heldSave.promise;
    state.ports = { ...request.body };
    return { body: { ...state.ports } };
  });
  // The refresh reads the old ports and waits on another endpoint before committing.
  const heldBatch = deferred();
  const stalePorts = { ...state.ports };
  backend.route("GET", `${API_ROOT}/settings/ports`, async () => heldBatch.promise);
  backend.route("GET", `${API_ROOT}/settings/pricing`, () => ({ body: state.pricing }));

  const appStore = await loadAppStore();
  const save = appStore.updatePorts({ proxy_port: 8090, service_port: 4200 });
  const refresh = appStore.refresh();

  heldSave.resolve();
  assert.equal(await save, true);
  heldBatch.resolve({ body: stalePorts });
  await refresh;

  assert.deepEqual(
    appStore.getSnapshot().ports,
    { proxy_port: 8090, service_port: 4200 },
    "a refresh that read before the save must not win after it",
  );
  assert.equal(appStore.getSnapshot().busy, false);
});

test("developer mode is saved by field alone and never reads the settings first", async () => {
  const { backend, state } = installAppBackend({
    desktop: { silent_start: true, show_dock_icon: false, developer_mode: false },
  });
  const appStore = await loadAppStore();

  assert.equal(await appStore.updateDeveloperMode(true), true);

  assert.deepEqual(
    state.desktop,
    { silent_start: true, show_dock_icon: false, developer_mode: true },
    "saving one field must keep the fields saved by other writers",
  );
  assert.equal(appStore.getSnapshot().developerMode, true);
  const writes = backend.requestsFor("PUT", `${API_ROOT}/settings/desktop`);
  assert.equal(writes.length, 1);
  assert.deepEqual(writes[0].body, { developer_mode: true }, "the save must carry only the changed field");
  assert.equal(
    backend.requestsFor("GET", `${API_ROOT}/settings/desktop`).length,
    0,
    "the save must not read the settings and write them back",
  );
});

test("per-model pricing saves send one rule and retain other models despite reordered responses", async () => {
  const { backend, state } = installAppBackend();
  const appStore = await loadAppStore();
  await appStore.refresh();
  const held = deferred();
  const ds = structuredClone(defaultPricing().models["hash-1"]);
  ds.mode = "fixed";
  const gpt = structuredClone(ds);
  gpt.cny.fixed.input_per_million = 30;
  backend.route("PUT", `${API_ROOT}/settings/pricing/hash-1`, async request => {
    state.pricing = { models: { ...state.pricing.models, "hash-1": request.body } };
    const early = structuredClone(state.pricing);
    await held.promise;
    return { body: early };
  });
  const first = appStore.updatePricingSettings("hash-1", ds);
  assert.equal(await appStore.updatePricingSettings("hash-2", gpt), true);
  held.resolve();
  assert.equal(await first, true);
  assert.deepEqual(appStore.getSnapshot().pricing, state.pricing);
  assert.deepEqual(backend.requestsFor("PUT", `${API_ROOT}/settings/pricing/hash-2`)[0].body, gpt);
  assert.equal(backend.requestsFor("PUT", `${API_ROOT}/settings/pricing`).length, 0);
});

test("a delayed price save cannot resurrect another model's deleted rule", async () => {
  const base = defaultPricing();
  base.models["hash-2"] = structuredClone(base.models["hash-1"]);
  const { backend, state } = installAppBackend({ pricing: base });
  const appStore = await loadAppStore();
  await appStore.refresh();
  const held = deferred();
  backend.route("PUT", `${API_ROOT}/settings/pricing/hash-1`, async request => {
    state.pricing.models["hash-1"] = request.body;
    const early = structuredClone(state.pricing);
    await held.promise;
    return { body: early };
  });
  const save = appStore.updatePricingSettings("hash-1", base.models["hash-1"]);
  assert.equal(await appStore.removePricingSettings("hash-2"), true);
  held.resolve();
  assert.equal(await save, true);
  assert.equal(Object.hasOwn(appStore.getSnapshot().pricing.models, "hash-2"), false);
  assert.deepEqual(appStore.getSnapshot().pricing, state.pricing);
});

test("a failed save keeps the refresh that was already reading and clears its busy flag", async () => {
  const { backend, state } = installAppBackend();
  backend.route("PUT", `${API_ROOT}/settings/desktop`, () => ({ status: 500, body: { message: "disk full" } }));
  const heldBatch = deferred();
  const freshPorts = { proxy_port: 7000, service_port: 4200 };
  backend.route("GET", `${API_ROOT}/settings/ports`, async () => heldBatch.promise);

  const appStore = await loadAppStore();
  const refresh = appStore.refresh();
  assert.equal(await appStore.updateDeveloperMode(true), false);

  heldBatch.resolve({ body: freshPorts });
  await refresh;

  const snapshot = appStore.getSnapshot();
  assert.equal(snapshot.error, "disk full", "the failure must be visible");
  assert.equal(snapshot.developerMode, false, "a failed save must not claim the new value");
  assert.deepEqual(snapshot.ports, freshPorts, "a failed save must not discard data a concurrent refresh did read");
  assert.equal(snapshot.busy, false);
  assert.deepEqual(state.desktop, { silent_start: false, show_dock_icon: true, developer_mode: false });
});
