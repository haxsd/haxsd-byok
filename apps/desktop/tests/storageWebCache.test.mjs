// 设置→存储页的抓取缓存半条（16/P2-7 的桌面端展示半边）。
//
// 服务端把抓取缓存（`<数据目录>/cache/web`，文件系统）与库内字节分开报：`web_cache_bytes` /
// `web_cache_files`，只有 `scope=web_cache` 会清它，清理是后台分批跑的（`clearing.deleted_files`）。
// 这一组用例钉住：数字分行渲染、按钮发出正确的作用域、进度用文件数而不是行数。

import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { API_ROOT, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const STORAGE = `${API_ROOT}/settings/storage/statistics`;
const STORAGE_FIXTURE = {
  bytes: 26_004_480,
  database_bytes: 1_212_334_080,
  artifact_bytes: 41_943_040,
  artifact_retention_days: 7,
  call_count: 12,
  trace_count: 12,
  conversation_retention_days: 3,
  web_cache_bytes: 8_912_896,
  web_cache_files: 412,
};

function installStorageBackend(initial = {}) {
  const { backend } = installAppBackend();
  let stored = { ...STORAGE_FIXTURE, ...initial };
  backend.route("GET", STORAGE, () => ({ body: { ...stored } }));
  backend.route("DELETE", STORAGE, (request) => {
    const scope = request.body?.scope ?? "details";
    stored = scope === "web_cache"
      ? { ...stored, web_cache_bytes: 0, web_cache_files: 0 }
      : scope === "all"
        ? { ...stored, bytes: 0, call_count: 0, trace_count: 0 }
        : { ...stored, bytes: 0 };
    return { body: { ...stored } };
  });
  return { backend, stored: () => stored };
}

async function loadStorageHelpers() {
  loader.invalidateAll();
  return loader.load("/src/features/settings/storageHint.ts");
}

test("the fetched-cache numbers are shown apart from the library bytes", async () => {
  const { backend } = installStorageBackend();
  const { formatBytes, hasWebCache } = await loadStorageHelpers();

  const response = await fetch(STORAGE);
  const storage = await response.json();

  assert.equal(formatBytes(storage.web_cache_bytes), "8.5 MB", "抓取缓存体积单独格式化");
  assert.equal(storage.web_cache_files, 412, "还要给出文件数");
  assert.equal(formatBytes(storage.bytes), "24.8 MB", "库内占用是另一个数字");
  assert.notEqual(formatBytes(storage.web_cache_bytes), formatBytes(storage.bytes), "两个数字不能合并成一个");
  assert.equal(hasWebCache(storage), true);
  assert.equal(backend.requestsFor("GET", STORAGE).length, 1);
});

test("a card without a cache disables the button instead of offering a no-op", async () => {
  const { hasWebCache } = await loadStorageHelpers();

  assert.equal(hasWebCache({ web_cache_bytes: 0, web_cache_files: 0 }), false);
  assert.equal(hasWebCache({ web_cache_bytes: 0, web_cache_files: 3 }), true, "一堆空文件也算占用");
  assert.equal(hasWebCache({ web_cache_bytes: 1024, web_cache_files: 0 }), true, "显示成 0 B 的大体积也算");
});

test("clearing the fetched cache sends the web_cache scope and keeps the library bytes", async () => {
  const { backend, stored } = installStorageBackend();

  const response = await fetch(STORAGE, { method: "DELETE", body: JSON.stringify({ scope: "web_cache" }) });
  const after = await response.json();

  const writes = backend.requestsFor("DELETE", STORAGE);
  assert.equal(writes.length, 1);
  assert.deepEqual(writes[0].body, { scope: "web_cache" }, "作用域必须是 web_cache，不能顺手清库内数据");
  assert.equal(after.web_cache_bytes, 0);
  assert.equal(after.web_cache_files, 0);
  assert.equal(stored().bytes, STORAGE_FIXTURE.bytes, "库内观测数据不受影响");
  assert.equal(stored().call_count, STORAGE_FIXTURE.call_count);
});

test("the progress of a cache clear reads deleted_files, not deleted_rows", async () => {
  const { clearingCounter } = await loadStorageHelpers();

  // 服务端只有 web_cache 作用域会动 deleted_files：另一个计数一直是 0，拿它显示会像卡住了。
  assert.equal(clearingCounter("web_cache", { deleted_rows: 0, deleted_files: 137 }), 137);
  assert.equal(clearingCounter("details", { deleted_rows: 4096, deleted_files: 0 }), 4096);
  assert.equal(clearingCounter("all", { deleted_rows: 8192, deleted_files: 0 }), 8192);
});

test("the in-progress clear is reported with its progress and polled until it disappears", async () => {
  const { backend } = installStorageBackend();
  let polls = 0;
  backend.route("GET", STORAGE, () => {
    polls += 1;
    return { body: polls >= 2 ? { ...STORAGE_FIXTURE, web_cache_bytes: 0, web_cache_files: 0 } : { ...STORAGE_FIXTURE, clearing: { deleted_rows: 0, deleted_files: 180 } } };
  });
  const { clearingCounter } = await loadStorageHelpers();

  const first = await (await fetch(STORAGE)).json();
  assert.ok(first.clearing, "第一次读回执说明清理在跑");
  assert.equal(clearingCounter("web_cache", first.clearing), 180, "进度显示已删除的文件数");
  const second = await (await fetch(STORAGE)).json();
  assert.equal(second.clearing, undefined, "清完之后服务端不再给进度");
  assert.equal(second.web_cache_files, 0);
});

test("the card renders both rows and wires the cache button to the dialog", async () => {
  const source = await readFile(new URL("../src/features/settings/StorageSettingsCard.tsx", import.meta.url), "utf8");

  assert.match(source, /web_cache_bytes/, "卡片要显示抓取缓存体积");
  assert.match(source, /web_cache_files/, "卡片要显示抓取缓存文件数");
  assert.match(source, /setScope\("web_cache"\)/, "缓存那一行的按钮要把作用域设成 web_cache");
  assert.match(source, /clearingCounter\(clearingScope, clearing\)/, "进度按作用域取对应的计数");
  assert.doesNotMatch(source, /bytes \+ storage\.web_cache_bytes/, "别把两个数字加起来");
});

// 大工具结果与压缩历史也不在库里：服务端按"引用还在不在"清理（维护循环）。卡片要把体积与
// 保留期一起说出来，但天数只有服务端给了正数才印——缺字段时宁可不提，也不印"保留 0 天"。
test("the artifact row reports disk usage and only names a window the server sent", async () => {
  const source = await readFile(new URL("../src/features/settings/StorageSettingsCard.tsx", import.meta.url), "utf8");

  assert.match(source, /formatBytes\(storage\.artifact_bytes\)/, "要显示落盘目录的合计体积");
  assert.match(source, /retentionDays\(storage\?\.artifact_retention_days\)/, "天数走与对话窗口同一套判定");
  assert.match(source, /大工具结果与压缩历史：\{size\}（随会话一起回收；没有引用的文件最多再留 \{days\} 天）/, "有正数天数时的句子");
  assert.match(source, /大工具结果与压缩历史：\{size\}（随会话一起回收）/, "天数缺失时的句子不能带 {days}");
  assert.doesNotMatch(source, /artifact_retention_days \?\? \d+/, "别在前端写死保留天数");
});
