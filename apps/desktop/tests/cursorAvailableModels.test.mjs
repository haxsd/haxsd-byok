import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const plugin = (configured, enabled = true) => ({
  id: "plugin-a",
  name: "Account plugin",
  providers: [{
    configured,
    models: [{
      id: "plugin:provider:model",
      displayName: "Shared name",
      pluginName: "Account plugin",
      enabled,
    }],
  }],
});

test("plugin-only configuration supplies usable Cursor models", async () => {
  const { cursorAvailableModels } = await loader.load("/src/features/models/cursorAvailableModels.ts");
  assert.deepEqual(cursorAvailableModels([], [plugin(true)]), [{
    id: "plugin:provider:model",
    name: "Shared name",
    source: "Account plugin",
  }]);
});

test("unconfigured or disabled plugin models are excluded", async () => {
  const { cursorAvailableModels } = await loader.load("/src/features/models/cursorAvailableModels.ts");
  assert.deepEqual(cursorAvailableModels([], [plugin(false), plugin(true, false)]), []);
});

test("manual and plugin models keep distinct identities even with the same name", async () => {
  const { cursorAvailableModels } = await loader.load("/src/features/models/cursorAvailableModels.ts");
  const result = cursorAvailableModels([{
    model_hash: "manual-hash",
    display_name: "Shared name",
    group_name: "My provider",
    base_url: "https://example.com/v1",
  }], [plugin(true)]);
  assert.equal(result.length, 2);
  assert.deepEqual(result.map(model => model.id), ["manual-hash", "plugin:provider:model"]);
  assert.deepEqual(result.map(model => model.source), ["My provider", "Account plugin"]);
});
