// Behavior tests for the request boundary in src/shared/api.ts.

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend } from "./appBackend.mjs";

const PAGE_ORIGIN = "http://127.0.0.1:3000";
installBrowserDoubles({ origin: PAGE_ORIGIN });
const loader = await startModuleLoader();
test.after(() => loader.close());

test("the plugin export URL stays on the origin the page is actually served from", async () => {
  // The stored service port is a wish for the next start, not the port this window
  // is talking to; it was 43120 while the page runs on 3000.
  installAppBackend({ ports: { proxy_port: 8090, service_port: 43120 } });
  loader.invalidateAll();
  const { appStore } = await loader.load("/src/shared/store/appStore.ts");
  await appStore.refresh();
  assert.equal(appStore.getSnapshot().ports.service_port, 43120);

  const { api } = await loader.load("/src/shared/api.ts");
  const url = api.pluginResourceExportUrl("example-plugin", "account");

  assert.equal(
    url,
    `${PAGE_ORIGIN}${API_ROOT}/plugins/example-plugin/resources/account/export`,
  );
});

test("the plugin export URL escapes the plugin and resource names", async () => {
  const { api } = await loader.load("/src/shared/api.ts");
  const url = api.pluginResourceExportUrl("plug in", "acc/ount");

  assert.equal(
    url,
    `${PAGE_ORIGIN}${API_ROOT}/plugins/plug%20in/resources/acc%2Fount/export`,
  );
});

test("a plugin model key is escaped into the pricing route, never split into segments", async () => {
  const { backend } = installAppBackend();
  // 插件模型的稳定标识带 `:` 与 `/`：它们必须留在同一个路由段里，
  // 否则保存会落到别的键上（axum 会把捕获段整体百分号解码，不会按 `/` 拆段）。
  const encoded = `${API_ROOT}/settings/pricing/plugin%3Adev.example%2Fcodex%2Fgpt-test`;
  backend.route("PUT", encoded, () => ({ body: { models: {} } }));

  const { api } = await loader.load("/src/shared/api.ts");
  await api.setModelPricing("plugin:dev.example/codex/gpt-test", {
    mode: "fixed",
    cny: null,
    usd: null,
  });

  assert.deepEqual(
    backend.requestsFor("PUT", encoded).map(request => request.path),
    [encoded],
  );
});
