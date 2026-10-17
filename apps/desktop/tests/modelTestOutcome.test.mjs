// 连通性测试超时/取消的界面映射（11/P3-2）。
//
// 后端裁定：超时后立即收尾，结论是"已取消"（`service.rs` 的
// `connectivity_test_timeout_error`：45 秒上限的 Provider 错误 + 取消这次测试）。
// 界面因此必须显示"已取消"，把超时上限作为原因放进详情——说"超时"会像"还在跑"，
// 而且与服务端记下的结论不一致。

import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

/** 服务端超时原句（server/src/control/service.rs:319-324）。 */
const TIMEOUT_MESSAGE = "model connectivity test timed out after 45 seconds";

async function loadOutcome() {
  loader.invalidateAll();
  return loader.load("/src/features/models/modelTestOutcome.ts");
}

test("a timed-out test is reported as cancelled, not as a failure", async () => {
  const { classifyModelTestFailure, isConnectivityTimeout } = await loadOutcome();

  const end = classifyModelTestFailure(new Error(TIMEOUT_MESSAGE), (cause) => `失败：${String(cause)}`);

  assert.equal(isConnectivityTimeout(new Error(TIMEOUT_MESSAGE)), true);
  assert.equal(end.status, "cancelled", "超时算取消，不算失败");
  assert.doesNotMatch(end.reason ?? "", /^失败：/, "不该走失败格式化");
});

test("the timeout limit is the readable reason, taken from the server's own sentence", async () => {
  const { connectivityTimeoutReason, connectivityTimeoutSeconds } = await loadOutcome();

  assert.equal(connectivityTimeoutSeconds(new Error(TIMEOUT_MESSAGE)), 45);
  assert.match(connectivityTimeoutReason(new Error(TIMEOUT_MESSAGE)), /45/);
  assert.match(connectivityTimeoutReason(new Error(TIMEOUT_MESSAGE)), /取消/);
});

test("a reason without a number still says cancelled without inventing a limit", async () => {
  const { connectivityTimeoutReason, connectivityTimeoutSeconds } = await loadOutcome();

  assert.equal(connectivityTimeoutSeconds(new Error("model connectivity test timed out")), null);
  const reason = connectivityTimeoutReason(new Error("model connectivity test timed out"));
  assert.match(reason, /取消/);
  assert.doesNotMatch(reason, /\d/, "认不出上限就不印数字，别编一个 45");
});

test("real failures keep their own formatting", async () => {
  const { classifyModelTestFailure, isConnectivityTimeout } = await loadOutcome();

  const offline = new Error("管理服务连不上");
  assert.equal(isConnectivityTimeout(offline), false);
  assert.deepEqual(classifyModelTestFailure(offline, () => "离线：管理服务未启动"), { status: "failed", reason: "离线：管理服务未启动" });

  // 客户端读超时（shared/api.ts 的 RequestTimeoutError）不是"服务端收尾的取消"：它仍然是失败。
  const readTimeout = new Error("管理请求超时（15 秒）");
  assert.equal(classifyModelTestFailure(readTimeout, () => "读超时").status, "failed");
});

test("the result view shows cancelled in the summary and the reason in the detail", async () => {
  const source = await readFile(new URL("../src/features/models/CursorModelTestResult.tsx", import.meta.url), "utf8");

  assert.match(source, /status: "cancelled"; reason\?: string \| null/, "取消状态要能带上原因");
  assert.match(source, /t\("已取消：\{reason\}"[^)]*\)/, "原因放在详情/提示里");
  // 只查真正的文案（注释里提"超时"是在解释为什么不说超时）。
  assert.doesNotMatch(source, /t\("[^"]*超时/, "摘要与详情都不该把这次收尾说成超时");
});

test("the model library maps a timed-out connectivity test to cancelled", async () => {
  const source = await readFile(new URL("../src/features/models/ModelLibraryPage.tsx", import.meta.url), "utf8");

  assert.match(source, /classifyModelTestFailure\(/, "要用分类器，而不是直接当失败");
  assert.match(source, /\{ status: "cancelled", reason: end\.reason \}/, "取消状态带上原因");
});
