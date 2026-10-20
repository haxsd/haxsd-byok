// 「响应流」预览：几十 KB 直接渲染，几 MB 只渲染前缀，用户点「显示全部」才展开。
//
// 起因：一次 3 MB 的响应正文一次性交给 Monaco，打开详情要等 2.4 秒，窗口像卡死。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const module = await loader.load("/src/features/calls/responsePreview.ts");
const { responsePreview, RESPONSE_PREVIEW_BYTES } = module;

const chunk = (data, byteCount = data.length) => ({ seq: 0, received_offset_ms: 0, data, byte_count: byteCount });

test("a small response is rendered in full", () => {
  const preview = responsePreview([chunk("hello "), chunk("world")]);
  assert.equal(preview.text, "hello world");
  assert.equal(preview.truncated, false);
  assert.equal(preview.totalBytes, 11);
  assert.equal(preview.totalChunks, 2);
});

test("a large response stops after the preview budget", () => {
  const big = "x".repeat(200 * 1024);
  const chunks = [chunk(big), chunk(big), chunk(big)];
  const preview = responsePreview(chunks);
  assert.equal(preview.truncated, true);
  assert.ok(preview.text.length <= big.length * 2, "must not join every chunk");
  assert.ok(preview.shownBytes >= RESPONSE_PREVIEW_BYTES, "the preview must fill the budget");
  assert.equal(preview.totalBytes, 600 * 1024, "the total still counts every chunk");
  assert.equal(preview.totalChunks, 3);
});

test("show all returns the whole body", () => {
  const chunks = [chunk("a".repeat(1000)), chunk("b".repeat(1000))];
  const preview = responsePreview(chunks, { showAll: true });
  assert.equal(preview.truncated, false);
  assert.equal(preview.text.length, 2000);
  assert.equal(preview.shownBytes, 2000);
});

test("the first chunk is always shown even when it alone exceeds the budget", () => {
  const preview = responsePreview([chunk("z".repeat(RESPONSE_PREVIEW_BYTES + 10)), chunk("tail")], { limitBytes: 16 });
  assert.equal(preview.text.length, RESPONSE_PREVIEW_BYTES + 10);
  assert.equal(preview.truncated, true, "later chunks are still left out");
});

test("a malformed chunk does not throw and does not inflate the byte count", () => {
  const preview = responsePreview([{ data: null, byte_count: "nope" }, chunk("ok")]);
  assert.equal(preview.text, "ok");
  assert.equal(preview.totalBytes, 2);
  assert.equal(preview.truncated, false);
});
