import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { API_ROOT, FakeBackend, installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
await loader.load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
test.after(() => loader.close());
const state = { enabled: true, listening: true, port: 43120, configured: false, config_conflict: false, home: "test-home", calls: 0, last_error: null };

test("a listening Codex gateway is not presented as applied client configuration", async () => {
  const { codexConnectionView } = await loader.load("/src/features/codex/codexConnectionView.ts");
  assert.equal(codexConnectionView(state).state, "pending-apply");
  assert.equal(codexConnectionView({ ...state, configured: true }).state, "taken-over");
  assert.equal(codexConnectionView({ ...state, configured: true, listening: false }).state, "unavailable");
  assert.equal(codexConnectionView({ ...state, configured: true, config_conflict: true }).state, "conflict");
});

test("applied Codex configuration locks settings until an explicit restore", async () => {
  const { CodexSettingsForm } = await loader.load("/src/features/codex/CodexSettingsForm.tsx");
  const { TooltipProvider } = await loader.load("/src/shared/ui/Tooltip.tsx");
  const markup = renderToStaticMarkup(createElement(TooltipProvider, null, createElement(CodexSettingsForm, {
    draft: { enabled: true, port: 43120, home: "", model_hashes: ["hash"], default_model_hash: "hash" },
    models: [], locked: true, onChange: () => {},
  })));
  assert.match(markup, /aria-label="Codex 配置目录"[^>]*disabled/);
  assert.match(markup, /aria-label="Codex 网关端口"[^>]*disabled/);
  assert.match(markup, /撤销接管后才能修改设置/);
});

test("Codex settings and explicit apply/restore stay on the shared management HTTP boundary", async () => {
  const settings = { enabled: true, port: 43120, home: "", model_hashes: ["hash"], default_model_hash: "hash" };
  const backend = new FakeBackend()
    .route("GET", `${API_ROOT}/codex/settings`, () => ({ body: settings }))
    .route("PUT", `${API_ROOT}/codex/settings`, ({ body }) => ({ body }))
    .route("GET", `${API_ROOT}/codex/status`, () => ({ body: state }))
    .route("POST", `${API_ROOT}/codex/apply`, () => ({ body: { ...state, configured: true } }))
    .route("POST", `${API_ROOT}/codex/restore`, () => ({ body: state }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");
  assert.deepEqual(await api.codexSettings(), settings);
  assert.deepEqual(await api.setCodexSettings(settings), settings);
  assert.equal((await api.codexStatus()).configured, false);
  assert.equal((await api.applyCodex()).configured, true);
  assert.equal((await api.restoreCodex()).configured, false);
  assert.equal(backend.requestsFor("PUT", `${API_ROOT}/codex/settings`).length, 1);
  assert.equal(backend.requestsFor("POST", `${API_ROOT}/codex/apply`).length, 1);
  assert.equal(backend.requestsFor("POST", `${API_ROOT}/codex/restore`).length, 1);
});
