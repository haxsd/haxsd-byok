import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
// Load Sass before the minimal DOM boundary, as in cursorToolsSettings.test.mjs.
await loader.load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
test.after(() => loader.close());

async function renderProxy(overrides = {}) {
  const { ProxySettingsCard } = await loader.load("/src/features/settings/ProxySettingsCard.tsx");
  const noop = () => {};
  return renderToStaticMarkup(createElement(ProxySettingsCard, {
    settings: null,
    outbound: null,
    cursorMode: "official",
    draft: { mode: "default", address: "", auth_enabled: false, username: "", password: "" },
    editing: false,
    saving: false,
    error: null,
    onDraftChange: noop,
    onEdit: noop,
    onCancel: noop,
    onSave: noop,
    ...overrides,
  }));
}

test("a failed proxy read stops reporting an in-progress load", async () => {
  const markup = await renderProxy({ error: "Proxy settings request failed" });
  assert.match(markup, /读取失败/);
  assert.doesNotMatch(markup, /加载中…/);
});

test("a pending proxy read remains distinguishable from a failed read", async () => {
  const markup = await renderProxy();
  assert.match(markup, /加载中…/);
  assert.doesNotMatch(markup, /读取失败/);
});

test("editing an authenticated proxy immediately renders named address and credential inputs", async () => {
  const markup = await renderProxy({
    settings: { mode: "custom", address: "http://127.0.0.1:9009", auth_enabled: true, username: "sample", has_password: true },
    draft: { mode: "custom", address: "http://127.0.0.1:9009", auth_enabled: true, username: "sample", password: "" },
    editing: true,
  });
  assert.match(markup, /aria-label="代理地址"/);
  assert.match(markup, /aria-label="用户名"/);
  assert.match(markup, /aria-label="密码"/);
  assert.match(markup, /value="http:\/\/127\.0\.0\.1:9009"/);
  assert.match(markup, /type="password"/);
});
