import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

test("developer settings stay out of normal rendering until forced by an error", async () => {
  const { DeveloperSettings } = await loader.load("/src/features/harness/SettingsLayers.tsx");

  const normal = renderToStaticMarkup(createElement(DeveloperSettings, null,
    createElement("span", null, "内部诊断"),
  ));
  assert.equal(normal, "", "normal users must not get developer diagnostics");

  const forced = renderToStaticMarkup(createElement(DeveloperSettings, { forceVisible: true },
    createElement("span", null, "内部诊断"),
  ));
  assert.match(forced, /内部诊断/, "an error can expose the diagnostic entry");
});
