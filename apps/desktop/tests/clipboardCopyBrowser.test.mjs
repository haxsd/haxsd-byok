// Browser clipboard path for the call diagnostics copy action.
//
// A detail page opened from the local management origin has no Tauri internals;
// copying must use navigator.clipboard and must fail with an explicit message
// when the browser does not expose it.

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
delete globalThis.window.__TAURI_INTERNALS__;

let clipboardWrite = null;
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: { clipboard: { writeText: async (text) => { clipboardWrite = text; } } },
});

const loader = await startModuleLoader();
const { api } = await loader.load("/src/shared/api.ts");
test.after(() => loader.close());

test("browser detail page copy uses navigator.clipboard", async () => {
  await api.copyCursorText("browser diagnostic");
  assert.equal(clipboardWrite, "browser diagnostic");
});

test("browser copy without a clipboard fails explicitly", async () => {
  Object.defineProperty(globalThis, "navigator", { configurable: true, value: {} });
  await assert.rejects(api.copyCursorText("x"), /当前环境不支持写入剪贴板/);
});
