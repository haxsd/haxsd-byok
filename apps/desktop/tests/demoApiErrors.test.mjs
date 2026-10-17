// 演示面具的错误响应形状（17/P3-7）。
//
// 演示里以前回的是 `{error:{message}}`，而 shared/api.ts 读的是 `parsed.message`：
// 读不到就把整段正文当成提示条内容，于是演示里的错误提示是一串原始 JSON。真实服务端
// （server/src/error.rs）的形状是 `{code,message}`，演示要能当替身就得一模一样。
//
// 另外两句 codex 文案以前是自造的（"Enable Codex and select a model first"），与真实服务端
// 原句不同，`features/harness/harnessErrorText.ts` 的匹配表自然也命不中——演示里就露出一句
// 英文。这里连原句、前缀（`configuration error: `）与状态码一起对齐真实服务端，并断言映射命中。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const demo = () => loader.load("/src/demo/api.ts");

/** 浏览器里 `window.fetch` 与全局 `fetch` 是同一个对象；替身里要手动对上。 */
const nativeFetch = globalThis.fetch;
async function installDemo() {
  const { installDemoApi } = await demo();
  globalThis.window.fetch = nativeFetch;
  installDemoApi();
  globalThis.fetch = globalThis.window.fetch;
  return { installDemoApi };
}

test("the status code maps to the same code strings the server uses", async () => {
  const { errorCode, errorBody } = await demo();

  assert.equal(errorCode(400), "invalid_argument");
  assert.equal(errorCode(404), "not_found");
  assert.equal(errorCode(409), "aborted");
  assert.equal(errorCode(502), "unavailable");
  assert.equal(errorCode(500), "internal");
  assert.deepEqual(errorBody(409, "boo"), { code: "aborted", message: "boo" });
});

test("a failing demo endpoint answers {code,message} with the server's own sentence", async () => {
  await installDemo();

  const response = await fetch("/__byok-api__/api/codex/apply", { method: "POST" });
  assert.equal(response.status, 400);
  const body = await response.json();
  assert.deepEqual(Object.keys(body).sort(), ["code", "message"], "形状必须与服务端一致");
  assert.equal(body.code, "invalid_argument");
  assert.equal(
    body.message,
    "configuration error: enable the Codex gateway before applying configuration",
    "原句与前缀都照抄服务端（error.rs:12 的 Display + gateway.rs:260）",
  );

  // 走真实 api 客户端时，提示条拿到的应当就是那句话，而不是整段 JSON。
  const { api } = await loader.load("/src/shared/api.ts");
  await assert.rejects(
    () => api.applyCodex(),
    (cause) => {
      assert.match(cause.message, /enable the Codex gateway before applying configuration/);
      assert.doesNotMatch(cause.message, /[{}\[\]]/, "提示条里不该出现 JSON 的括号");
      return true;
    },
  );
});

test("the demo's codex sentences hit the harness error mapping", async () => {
  await installDemo();
  const { api } = await loader.load("/src/shared/api.ts");
  const { clientErrorText } = await loader.load("/src/features/harness/harnessErrorText.ts");

  // 未启用网关 → 先启用，而不是先选模型。
  const disabled = await api.applyCodex().then(() => null, (cause) => cause);
  assert.equal(clientErrorText(disabled), "先启用 Codex 网关，再应用配置。");

  // 启用后还没选默认模型 → "先选择默认模型。"
  await api.setCodexSettings({ enabled: true, port: 43120, home: "", model_hashes: [], default_model_hash: null });
  const noModel = await api.applyCodex().then(() => null, (cause) => cause);
  assert.equal(clientErrorText(noModel), "先选择默认模型。");

  // 应用成功后再改设置 → 先撤销接管。
  await api.setCodexSettings({ enabled: true, port: 43120, home: "", model_hashes: ["m1"], default_model_hash: "m1" });
  await api.applyCodex();
  const configured = await api.setCodexSettings({ enabled: true, port: 43120, home: "", model_hashes: ["m1"], default_model_hash: "m1" })
    .then(() => null, (cause) => cause);
  assert.equal(clientErrorText(configured), "先撤销接管，再修改 Codex 网关设置。");
});

test("an unknown demo endpoint is a 404 in the same shape", async () => {
  await installDemo();

  const response = await fetch("/__byok-api__/api/definitely-not-a-route");
  assert.equal(response.status, 404);
  const body = await response.json();
  assert.equal(body.code, "not_found");
  assert.match(body.message, /Unhandled demo endpoint/);
});
