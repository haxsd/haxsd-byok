// Behavior tests for the model library fact strip (features/models/modelLibraryFacts.ts).
//
// The contract: a number only earns a slot when it answers something. The strip used to
// render「插件模型 0 / 测试通过 0 / 测试失败 0」on a fresh install, which is noise; an earlier
// fix then hid the whole strip whenever there were no built-in models, taking the plugin
// count and any test result down with it.
//
// The rule is a pure function, so it is asserted directly through the project's module
// loader rather than by rendering the page.

import assert from "node:assert/strict";
import test from "node:test";
import { startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
test.after(() => loader.close());
const facts = async (input) => (await loader.load("/src/features/models/modelLibraryFacts.ts")).modelLibraryFacts(input);

const base = { modelCount: 0, providerCount: 0, pluginModelCount: 0, testedOk: 0, testedFailed: 0 };

test("an empty library shows no facts at all", async () => {
  assert.deepEqual(await facts(base), []);
});

test("built-in models bring their own count and the provider count together", async () => {
  assert.deepEqual(await facts({ ...base, modelCount: 16, providerCount: 8 }), ["models", "providers"]);
});

test("a library made only of plugin models still reports them", async () => {
  assert.deepEqual(await facts({ ...base, pluginModelCount: 3 }), ["pluginModels"]);
});

test("cancelled tests do not bring back the two zero counters", async () => {
  assert.deepEqual(
    await facts({ ...base, modelCount: 2, providerCount: 1, testedOk: 0, testedFailed: 0 }),
    ["models", "providers"],
  );
});

test("a measured test result adds both counters, whatever the outcome", async () => {
  assert.deepEqual(
    await facts({ ...base, modelCount: 2, providerCount: 1, testedOk: 2 }),
    ["models", "providers", "testsPassed", "testsFailed"],
  );
  assert.deepEqual(
    await facts({ ...base, modelCount: 2, providerCount: 1, testedFailed: 1 }),
    ["models", "providers", "testsPassed", "testsFailed"],
  );
});
