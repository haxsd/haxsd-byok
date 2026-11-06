// 「存储管理」的两类提示：已删除数据的空闲页要压缩，观测数据要清理；两者不能混成
// "点清理就能腾出整个数据库"。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const module = await loader.load("/src/features/settings/storageHint.ts");

const MB = 1024 * 1024;
const GB = 1024 * MB;

test("small cleanable data and a small file stay quiet", () => {
  assert.equal(module.storageHint({ observability_bytes: 18 * MB, database_free_bytes: 20 * MB }), null);
});

test("cleanable observability data over the threshold asks for a cleanup", () => {
  assert.equal(module.storageHint({ observability_bytes: 300 * MB, database_free_bytes: 0 }), "clean-observability");
  assert.equal(module.storageHint({ observability_bytes: module.CLEANUP_HINT_BYTES, database_free_bytes: 0 }), "clean-observability");
});

test("deleted rows left on disk ask for compaction, not another cleanup", () => {
  assert.equal(module.storageHint({ observability_bytes: 18 * MB, database_free_bytes: 1.07 * GB }), "reclaim-space");
  assert.equal(module.storageHint({ observability_bytes: 0, database_free_bytes: module.RECLAIM_HINT_BYTES }), "reclaim-space");
});

test("a large freelist wins because the user already deleted those rows", () => {
  assert.equal(module.storageHint({ observability_bytes: 300 * MB, database_free_bytes: 4 * GB }), "reclaim-space");
});

test("the printed threshold is the one the check uses", () => {
  for (const hint of ["reclaim-space", "clean-observability"]) {
    const threshold = module.storageHintThreshold(hint);
    assert.ok(threshold > 0);
    if (hint === "clean-observability") {
      assert.equal(module.storageHint({ observability_bytes: threshold, database_free_bytes: 0 }), hint);
      assert.equal(module.storageHint({ observability_bytes: threshold - 1, database_free_bytes: 0 }), null);
    } else {
      assert.equal(module.storageHint({ observability_bytes: 0, database_free_bytes: threshold }), hint);
      assert.equal(module.storageHint({ observability_bytes: 0, database_free_bytes: threshold - 1 }), null);
    }
  }
});

test("byte sizes read the way the card prints them", () => {
  assert.equal(module.formatBytes(512), "512 B");
  assert.equal(module.formatBytes(2048), "2.0 KB");
  assert.equal(module.formatBytes(18_167_299), "17.3 MB");
  assert.equal(module.formatBytes(1_212_334_080), "1.1 GB");
});

test("missing or nonsensical byte counts read as zero", () => {
  for (const value of [undefined, null, Number.NaN, Number.POSITIVE_INFINITY, -1]) {
    assert.equal(module.formatBytes(value), "0 B", `value: ${String(value)}`);
  }
});

// 卡片里"服务端对话副本闲置 N 天后自动回收"的 N 直接印在句子中间：坏数字（缺失、0、
// 负数、NaN）会让句子变成"闲置 0 天"或"闲置 NaN 天"，比不写数字更糟。
test("the retention window the card prints is the one the server reported", () => {
  assert.equal(module.retentionDays(3), 3);
  assert.equal(module.retentionDays(20), 20);
  // 服务端不该发小数，真发了也不在句子里印小数天。
  assert.equal(module.retentionDays(3.9), 3);
});

test("a missing or nonsensical retention window reads as no number", () => {
  for (const value of [undefined, null, 0, -1, Number.NaN, Number.POSITIVE_INFINITY]) {
    assert.equal(module.retentionDays(value), 0, `value: ${String(value)}`);
  }
});
