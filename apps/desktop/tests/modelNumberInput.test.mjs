// 模型高级字段的数字输入（18/F-14）。
//
// `Math.trunc(Number("abc"))` 是 NaN：NaN 进草稿后输入框看着是空的，保存却报
// "必须是大于 0 的整数"，用户找不到是哪个字段。非有限值一律当作"没填"。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

test("an empty field means unset, not zero", async () => {
  const { numberFieldValue } = await loader.load("/src/features/models/modelNumberInput.ts");

  assert.equal(numberFieldValue(""), null);
  assert.equal(numberFieldValue("   "), null, "只输空格也算没填（Number('') 是 0，会写成 0）");
});

test("a non-numeric entry never becomes NaN in the draft", async () => {
  const { numberFieldValue } = await loader.load("/src/features/models/modelNumberInput.ts");

  for (const raw of ["abc", "1e", "-", "1.2.3", "Infinity", "NaN", "１２３abc"]) {
    const value = numberFieldValue(raw);
    assert.equal(value, null, `${JSON.stringify(raw)} 应当当作没填`);
    assert.equal(Number.isNaN(value), false, "草稿里绝不能出现 NaN");
  }
});

test("real numbers are truncated to integers", async () => {
  const { numberFieldValue } = await loader.load("/src/features/models/modelNumberInput.ts");

  assert.equal(numberFieldValue("128000"), 128000);
  assert.equal(numberFieldValue(" 64000 "), 64000, "两边空格不算内容");
  assert.equal(numberFieldValue("12.7"), 12);
  assert.equal(numberFieldValue("1e3"), 1000, "科学计数法是合法数字");
  assert.equal(numberFieldValue("-3"), -3, "负数留给校验去拒绝，读取端不偷偷改成 0");
});
