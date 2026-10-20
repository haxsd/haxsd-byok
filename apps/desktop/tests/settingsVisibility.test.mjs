// 设置页的取数门：不可见的分区不发请求。
//
// 卡片全部挂在树上是对的（切分区、搜索时草稿不丢），但 `hidden` 挡不住副作用——进设置页
// 就替五个分区里所有卡片各发一轮请求是纯浪费。这一组用例把两件事钉住：显示集合怎么算、
// 门有没有真的拦住请求。

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const navigation = () => loader.load("/src/features/settings/settingsNavigation.ts");

test("only the selected section's settings are treated as visible, search widens it precisely", async () => {
  const { settingsSections, visibleSettingsItems } = await navigation();

  assert.deepEqual(
    [...visibleSettingsItems(settingsSections(false), "general", "")].sort(),
    ["app", "appearance"],
    "默认落在「常规」：只有这一分区的两项可见",
  );
  assert.deepEqual([...visibleSettingsItems(settingsSections(false), "network", "")], ["proxy"]);
  assert.deepEqual(
    [...visibleSettingsItems(settingsSections(false), "general", "存储")].sort(),
    ["storage"],
    "搜索命中另一个分区时，只有命中的那一项可见",
  );
  assert.deepEqual(
    [...visibleSettingsItems(settingsSections(true), "advanced", "")].sort(),
    ["developer", "diagnostics", "ports"],
    "开发者模式打开后端口设置才进集合",
  );
});

test("the fetch gate keeps invisible cards from issuing any request", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/settings/proxy`, () => ({ body: proxyFixture() }));
  backend.route("GET", `${API_ROOT}/settings/outbound`, () => ({ body: { mode: "direct", proxy: null, checked_at_ms: 0 } }));
  backend.route("GET", `${API_ROOT}/settings/storage/statistics`, () => ({ body: storageFixture() }));
  backend.route("GET", `${API_ROOT}/app-info`, () => ({ body: { version: "0.0.0", executable_path: "x", data_dir: "C:\\data" } }));

  loader.invalidateAll();
  const { loadWhenVisible } = await loader.load("/src/shared/utils/visibleLoad.ts");
  const { loadNetworkSettings, loadStatisticsStorage, loadAppInfo } = await loader.load("/src/features/settings/settingsLoaders.ts");

  const hidden = await Promise.all([
    loadWhenVisible(false, loadNetworkSettings),
    loadWhenVisible(false, loadStatisticsStorage),
    loadWhenVisible(false, loadAppInfo),
  ]);
  assert.deepEqual(hidden, [null, null, null], "不可见时门后什么都不做");
  assert.deepEqual(backend.requests, [], "不可见时一条请求都不该发出去");

  const [network, storage, info] = await Promise.all([
    loadWhenVisible(true, loadNetworkSettings),
    loadWhenVisible(true, loadStatisticsStorage),
    loadWhenVisible(true, loadAppInfo),
  ]);
  assert.equal(network.settings.address, "127.0.0.1:7890", "可见时代理设置要真的读回来");
  assert.equal(network.outbound.mode, "direct");
  assert.equal(storage.call_count, 3);
  assert.equal(info.version, "0.0.0");
  assert.deepEqual(
    backend.requests.map((request) => request.path).sort(),
    [
      `${API_ROOT}/app-info`,
      `${API_ROOT}/settings/outbound`,
      `${API_ROOT}/settings/proxy`,
      `${API_ROOT}/settings/storage/statistics`,
    ],
    "可见时每张卡各发自己的那几条",
  );
});

test("one failing read does not swallow the other on the same card", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/settings/proxy`, () => ({ status: 500, body: { message: "代理设置读不到" } }));
  backend.route("GET", `${API_ROOT}/settings/outbound`, () => ({ body: { mode: "direct", proxy: null, checked_at_ms: 0 } }));

  loader.invalidateAll();
  const { loadWhenVisible } = await loader.load("/src/shared/utils/visibleLoad.ts");
  const { loadNetworkSettings } = await loader.load("/src/features/settings/settingsLoaders.ts");

  const loaded = await loadWhenVisible(true, loadNetworkSettings);
  assert.equal(loaded.settings, null);
  assert.match(loaded.settingsError, /代理设置读不到/);
  assert.equal(loaded.outbound.mode, "direct", "一个接口失败不该让另一个也空着");
  assert.equal(loaded.outboundError, null);
});

function proxyFixture() {
  return { mode: "manual", address: "127.0.0.1:7890", auth_enabled: false, username: "", password: "" };
}

function storageFixture() {
  return {
    bytes: 1024,
    call_count: 3,
    trace_count: 0,
    database_bytes: 2048,
    artifact_bytes: 4096,
    artifact_retention_days: 7,
    conversation_retention_days: 30,
  };
}
