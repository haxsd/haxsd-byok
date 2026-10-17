// 提交信息设置的读取路径必须**只读**。
//
// 旧实现把一次 PUT 藏在 GET 后面（prompt 为空且 prompt_locale ≠ 界面语言就自动改语言）：
// 每次进这一页、每次切语言都会静默写一次库，用户根本没点过保存。写现在只有显式动作。

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const view = (overrides = {}) => ({
  model_id: "",
  prompt: "",
  prompt_locale: "en-US",
  default_prompt: "Write a commit message.",
  ...overrides,
});

/** 装一个只认 /settings/commit 的假服务端，并把它收到的写入记下来。 */
function installCommitBackend(initial) {
  const requests = [];
  let stored = { ...initial };
  globalThis.fetch = async (input, init = {}) => {
    const url = new URL(String(input), "http://127.0.0.1");
    const method = (init.method ?? "GET").toUpperCase();
    requests.push({ method, path: url.pathname, body: typeof init.body === "string" ? JSON.parse(init.body) : null });
    if (method === "PUT") stored = { ...stored, ...JSON.parse(init.body) };
    return new Response(JSON.stringify({ ...stored, default_prompt: initial.default_prompt }), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  };
  return { requests, writes: () => requests.filter((request) => request.method === "PUT") };
}

async function loadHelpers() {
  loader.invalidateAll();
  return loader.load("/src/features/cursor/commitSettings.ts");
}

test("reading the settings writes nothing, even when the stored prompt locale differs", async () => {
  const backend = installCommitBackend(view({ prompt_locale: "en-US" }));
  const { loadCommitSettings } = await loadHelpers();

  const loaded = await loadCommitSettings("zh-CN");

  assert.equal(loaded.view.prompt_locale, "en-US", "读回来的仍是服务端存的那一份");
  assert.equal(loaded.localeMismatch, true, "不一致要被报告出来，界面才知道该提示");
  assert.equal(backend.writes().length, 0, "读取路径不得产生任何写入");
  assert.equal(backend.requests.filter((request) => request.method === "GET").length, 1);
});

test("a custom prompt is not treated as a locale mismatch", async () => {
  const install = installCommitBackend(view({ prompt: "按我的格式写", prompt_locale: "en-US" }));
  void install;
  const { loadCommitSettings } = await loadHelpers();

  const loaded = await loadCommitSettings("zh-CN");
  assert.equal(loaded.localeMismatch, false, "用户自己写过提示词时，语言标记不再影响生成内容");
});

test("adopting the interface language is one explicit write", async () => {
  const backend = installCommitBackend(view({ prompt_locale: "en-US" }));
  const { adoptInterfacePromptLocale, commitLocaleMismatch } = await loadHelpers();

  const saved = await adoptInterfacePromptLocale(view({ prompt_locale: "en-US" }), "zh-CN");

  assert.equal(backend.writes().length, 1, "只有显式动作才写");
  assert.deepEqual(backend.writes()[0].body, { model_id: "", prompt: "", prompt_locale: "zh-CN" });
  assert.equal(saved.prompt_locale, "zh-CN");
  assert.equal(commitLocaleMismatch(saved, "zh-CN"), false, "写完不再是不一致");
});
