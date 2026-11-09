// Desktop clipboard path for the call diagnostics copy action.
//
// The Tauri window must keep using the bundled clipboard capability. This file
// intentionally keeps `window.__TAURI_INTERNALS__` installed; the browser path
// lives in clipboardCopyBrowser.test.mjs so the two hosts never share module state.

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const browser = installBrowserDoubles();
const loader = await startModuleLoader();
const { api } = await loader.load("/src/shared/api.ts");
test.after(() => loader.close());

test("desktop copy keeps using the Tauri clipboard capability", async () => {
  const before = browser.tauriInvocations.length;
  await api.copyCursorText("desktop diagnostic");
  const invocation = browser.tauriInvocations.slice(before).at(-1);
  assert.equal(invocation.command, "plugin:clipboard-manager|write_text");
  assert.equal(invocation.args.text, "desktop diagnostic");
});
