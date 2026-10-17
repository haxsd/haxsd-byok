// 批量导入的本地准入检查：体积上限在读文件之前判定，条数多时按固定大小分批。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const limits = () => loader.load("/src/features/plugins/importLimits.ts");

test("a single oversized file is rejected by name before anything is read", async () => {
  const { importSizeProblem, MAX_IMPORT_FILE_BYTES } = await limits();
  const problem = importSizeProblem([
    { name: "accounts.json", size: 1024 },
    { name: "huge.json", size: MAX_IMPORT_FILE_BYTES + 1 },
  ]);
  assert.deepEqual(problem, { kind: "file-too-large", name: "huge.json" });
  // 正好等于上限是允许的：边界本身不是错误。
  assert.equal(importSizeProblem([{ name: "edge.json", size: MAX_IMPORT_FILE_BYTES }]), null);
});

test("a selection whose total exceeds the budget is rejected even when each file fits", async () => {
  const { importSizeProblem, MAX_IMPORT_FILE_BYTES, MAX_IMPORT_TOTAL_BYTES } = await limits();
  const files = Array.from({ length: 6 }, (_, index) => ({ name: `part-${index}.json`, size: MAX_IMPORT_FILE_BYTES }));
  assert.equal(MAX_IMPORT_FILE_BYTES * files.length > MAX_IMPORT_TOTAL_BYTES, true);
  assert.deepEqual(importSizeProblem(files), { kind: "total-too-large" });
  assert.equal(importSizeProblem([{ name: "small.json", size: 64 * 1024 }]), null);
});

test("large selections are split into bounded batches that keep their order", async () => {
  const { importBatches, IMPORT_FILES_PER_REQUEST } = await limits();
  const files = Array.from({ length: IMPORT_FILES_PER_REQUEST * 2 + 3 }, (_, index) => `file-${index}`);
  const batches = importBatches(files);
  assert.deepEqual(batches.map((batch) => batch.length), [IMPORT_FILES_PER_REQUEST, IMPORT_FILES_PER_REQUEST, 3]);
  assert.deepEqual(batches.flat(), files, "分批不能改变顺序，也不能丢文件");
  assert.deepEqual(importBatches([]), []);
});
