// 「存储管理」的阈值提示：两个数字决定说哪句话，判定统一放在 storageHint.ts。
//
// 背景：真实库 1.07 GiB，但可清理的观测数据只有 18 MB——大头是对话内容与内容块，
// 这里的清理不删它们。提示必须分清这两种"大"，否则用户会以为点一下能腾出整个数据库。

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
  assert.equal(module.storageHint({ bytes: 18 * MB, database_bytes: 300 * MB }), null);
});

test("cleanable observability data over the threshold asks for a cleanup", () => {
  assert.equal(module.storageHint({ bytes: 300 * MB, database_bytes: 300 * MB }), "clean-observability");
  // 边界：正好等于阈值也算超过（提示与格式化出来的数字一致，不会出现"256.0 MB 却提示超过 256 MB"）。
  assert.equal(module.storageHint({ bytes: module.CLEANUP_HINT_BYTES, database_bytes: 0 }), "clean-observability");
});

test("a big file with little cleanable data explains where the space went", () => {
  assert.equal(module.storageHint({ bytes: 18 * MB, database_bytes: 1.07 * GB }), "conversation-data");
});

test("cleanable data wins when both are big, because that is the actionable one", () => {
  assert.equal(module.storageHint({ bytes: 300 * MB, database_bytes: 4 * GB }), "clean-observability");
});

test("the printed threshold is the one the check uses", () => {
  for (const hint of ["clean-observability", "conversation-data"]) {
    const threshold = module.storageHintThreshold(hint);
    assert.ok(threshold > 0);
    if (hint === "clean-observability") {
      assert.equal(module.storageHint({ bytes: threshold, database_bytes: 0 }), hint);
      assert.equal(module.storageHint({ bytes: threshold - 1, database_bytes: 0 }), null);
    } else {
      assert.equal(module.storageHint({ bytes: 0, database_bytes: threshold }), hint);
      assert.equal(module.storageHint({ bytes: 0, database_bytes: threshold - 1 }), null);
    }
  }
});

test("byte sizes read the way the card prints them", () => {
  assert.equal(module.formatBytes(512), "512 B");
  assert.equal(module.formatBytes(2048), "2.0 KB");
  assert.equal(module.formatBytes(18_167_299), "17.3 MB");
  assert.equal(module.formatBytes(1_212_334_080), "1.1 GB");
});

// 卡片里"服务端对话副本闲置 N 天后自动回收"的 N 直接印在句子中间：坏数字（缺失、0、
// 负数、NaN）会让句子变成"闲置 0 天"或"闲置 NaN 天"，比不写数字更糟。
test("the retention window the card prints is the one the server reported", () => {
  assert.equal(module.conversationRetentionDays(3), 3);
  assert.equal(module.conversationRetentionDays(20), 20);
  // 服务端不该发小数，真发了也不在句子里印小数天。
  assert.equal(module.conversationRetentionDays(3.9), 3);
});

test("a missing or nonsensical retention window reads as no number", () => {
  for (const value of [undefined, null, 0, -1, Number.NaN, Number.POSITIVE_INFINITY]) {
    assert.equal(module.conversationRetentionDays(value), 0, `value: ${String(value)}`);
  }
});
