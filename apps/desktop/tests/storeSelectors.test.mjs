// store 的订阅粒度（17/P2-3）。
//
// 复核结论：`update()` 广播给全部订阅者是真的，「9 个常驻页面整棵重渲染」是假的（隐藏页的
// 订阅会被 React 19 的 <Activity mode="hidden"> 卸掉）。剩下的真问题只有「外壳 + 当前页」
// 会被无关写入惊动，修法是选择器 + memo。
//
// 关键性质：选择器必须返回**快照里的稳定引用/原始值**。返回新对象的话 Object.is 每次都判
// 「变了」，等于没省，还会让 React 报 getSnapshot 未被缓存。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

async function loadStore() {
  loader.invalidateAll();
  const store = await loader.load("/src/shared/store/appStore.ts");
  const selectors = await loader.load("/src/shared/store/selectors.ts");
  return { ...store, ...selectors };
}

test("a selector keeps the identity of an untouched field", async () => {
  installAppBackend();
  const { appStore, selectCalls, selectModels, selectOffline, selectTheme } = await loadStore();

  const before = appStore.getSnapshot();
  const callsBefore = selectCalls(before);
  const modelsBefore = selectModels(before);

  // 一次"别的字段"的写入：主题变了，调用与模型都没动。
  appStore.selectTheme("default-light");
  const after = appStore.getSnapshot();

  assert.notEqual(after, before, "写入确实换了快照");
  assert.equal(selectTheme(after), "default-light", "这次写入动的是主题");
  assert.equal(selectCalls(after), callsBefore, "调用列表的引用必须原样保留，Object.is 才不会判成变了");
  assert.equal(selectModels(after), modelsBefore);
  assert.equal(selectOffline(after), false);
  globalThis.localStorage.removeItem("haxsd-byok.theme");
});

test("a selector never builds a new container per call", async () => {
  installAppBackend();
  const { appStore, selectCalls, selectModels, selectOffline, selectTheme } = await loadStore();
  const snapshot = appStore.getSnapshot();

  assert.equal(selectCalls(snapshot), selectCalls(snapshot), "同一次读两次必须同一个引用");
  assert.equal(selectModels(snapshot), selectModels(snapshot));
  assert.equal(selectOffline(snapshot), selectOffline(snapshot));
  assert.equal(selectTheme(snapshot), selectTheme(snapshot));
});

test("the offline flag flips when the service goes away", async () => {
  installAppBackend();
  const { appStore, selectOffline } = await loadStore();

  assert.equal(selectOffline(appStore.getSnapshot()), false);
  // 服务进程没了：fetch 直接拒绝，而不是回一个错误状态码。
  const reachable = globalThis.fetch;
  globalThis.fetch = () => Promise.reject(new TypeError("fetch failed"));
  try {
    await appStore.refresh();
  } finally {
    globalThis.fetch = reachable;
  }
  assert.equal(selectOffline(appStore.getSnapshot()), true, "服务连不上要翻成 true（横幅据此出现）");
});
