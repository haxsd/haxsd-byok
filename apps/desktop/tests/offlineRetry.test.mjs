// 离线自动重试：什么时候发、多久发一次。
//
// 旧实现是 setInterval(3s) 且不看窗口状态：窗口最小化、用户切到别的应用时，后台每 3 秒
// 仍然发一整批请求（refresh 打十二个接口）。这一组用例把判据与退避钉住。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const retry = () => loader.load("/src/shell/offlineRetry.ts");

test("the retry interval backs off from three seconds to thirty and stays there", async () => {
  const { RETRY_DELAYS_MS, retryDelayMs } = await retry();

  assert.deepEqual([...RETRY_DELAYS_MS], [3_000, 10_000, 30_000], "节奏只在这里定义");
  assert.equal(retryDelayMs(0), 3_000);
  assert.equal(retryDelayMs(1), 10_000);
  assert.equal(retryDelayMs(2), 30_000);
  assert.equal(retryDelayMs(9), 30_000, "退避有底：不会一直翻倍");
  assert.equal(retryDelayMs(-3), 3_000, "非法次数退回第一档");
});

test("a hidden or unfocused window does not fire a retry", async () => {
  const { shouldAutoRetry } = await retry();

  assert.equal(shouldAutoRetry({ hidden: false, hasFocus: true }), true, "用户正看着这个窗口");
  assert.equal(shouldAutoRetry({ hidden: true, hasFocus: true }), false, "最小化：看不见就不发");
  assert.equal(shouldAutoRetry({ hidden: false, hasFocus: false }), false, "用户在别的窗口里干活");
  assert.equal(shouldAutoRetry({ hidden: true, hasFocus: false }), false);
});

test("window visibility reads the document and defaults to retrying when focus is unknown", async () => {
  const { windowVisibility } = await retry();

  assert.deepEqual(windowVisibility({ hidden: false, hasFocus: () => true }), { hidden: false, hasFocus: true });
  assert.deepEqual(windowVisibility({ hidden: true, hasFocus: () => false }), { hidden: true, hasFocus: false });
  assert.equal(windowVisibility({}).hasFocus, true, "取不到焦点状态时按有焦点处理：不重试比多试一次坏得多");
  assert.deepEqual(windowVisibility({}), { hidden: false, hasFocus: true });
});
