// Behavior tests for the call facts panel's client-source line (features/calls).
//
// 同一个字段以前有两套说法：调用页的筛选条写「内部调用」，展开行与详情页却把数据库
// 里的 `internal`／`cursor` 原样印在中文界面上。这里用真实渲染把说法钉住：
//
// 1. 客户端来源显示的是界面语言里的名字，不是数据库里的小写标记；
// 2. 内部调用与三个客户端各自说自己的名字；
// 3. 认不出来的来源保留原文——宁可露出英文，也不要安一个不准确的词。
//
// 断言只看渲染出的文字，不碰类名。
//
// Harness note: the SCSS module is warmed up *before* the browser doubles are
// installed — see cursorToolsSettings.test.mjs.

import assert from "node:assert/strict";
import test from "node:test";
import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";
import { desktopRoot, installBrowserDoubles } from "./harness.mjs";

/** `renderToStaticMarkup` 需要 useSyncExternalStore 的第三个参数：见 homeMetricsCopy.test.mjs。 */
const serverSnapshotPlugin = {
  name: "test-server-snapshot",
  enforce: "pre",
  transform(source, id) {
    const snapshotHooks = ["/src/shared/store/appStore.ts", "/src/i18n/store.ts"];
    const imported = 'import { useSyncExternalStore } from "react";';
    if (!snapshotHooks.some((path) => id.split("?")[0].endsWith(path)) || !source.includes(imported)) return null;
    return source.replace(
      imported,
      'import { useSyncExternalStore as __ssrUseSyncExternalStore } from "react";\n' +
      "const useSyncExternalStore = (subscribe, getSnapshot, getServerSnapshot = getSnapshot) =>\n" +
      "  __ssrUseSyncExternalStore(subscribe, getSnapshot, getServerSnapshot);",
    );
  },
};

const server = await createServer({
  configFile: false,
  root: desktopRoot,
  appType: "custom",
  logLevel: "error",
  plugins: [serverSnapshotPlugin],
  server: { middlewareMode: true, hmr: false, ws: false },
});
const load = (modulePath) => server.ssrLoadModule(modulePath);
await load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
test.after(() => server.close());

// 反馈里的界面是简体中文，这里把渲染固定成同一套。
const { setLocalePreference } = await load("/src/i18n/store.ts");
setLocalePreference("zh-CN");

/** 一条调用记录：只填这个面板会读到、并且参与断言的字段。 */
function call(source, overrides = {}) {
  return {
    call_kind: "provider_llm",
    client_source: source,
    route: "local_byok",
    call_id: "call-1",
    run_id: "run-1",
    conversation_id: "conversation-1",
    provider_call_index: 1,
    model_hash: "hash-1",
    provider_type: "openai",
    provider_url: "https://api.example.com",
    request_type: "responses",
    request_url: "https://api.example.com/v1/responses",
    model_id: "gpt-5",
    display_name: "主力模型",
    reasoning_effort: null,
    fast: null,
    status: "completed",
    finish_reason: "stop",
    created_at_ms: 1_700_000_000_000,
    ttfb_ms: 210,
    ttfr_ms: 290,
    ttft_ms: 370,
    duration_ms: 1_420,
    input_tokens: 4_800,
    output_tokens: 820,
    total_tokens: 5_620,
    cache_read_tokens: 3_100,
    cache_write_tokens: 320,
    reasoning_tokens: null,
    message_count: 14,
    tool_count: 3,
    http_status: 200,
    error_kind: null,
    error_message: null,
    detailed: false,
    ...overrides,
  };
}

/** 面板的可见文字：标签剥掉、空白压平。 */
function factsText(markup) {
  return markup.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();
}

async function renderFacts(loc) {
  const { CallFacts } = await load("/src/features/calls/CallFacts.tsx");
  return factsText(renderToStaticMarkup(h(CallFacts, { call: loc })));
}

test("内部调用显示为界面语言里的名字，不是数据库里的 internal", async () => {
  const text = await renderFacts(call("internal"));
  assert.match(text, /客户端来源 内部调用/, "客户端来源要与调用页筛选条的说法一致");
  assert.doesNotMatch(text, /\binternal\b/, "数据库标记不能出现在界面上");
});

for (const [source, label] of [["cursor", "Cursor"], ["devin", "Devin"], ["codex", "Codex"], ["claude", "Claude"]]) {
  test(`${source} 显示为产品名 ${label}`, async () => {
    const text = await renderFacts(call(source));
    assert.match(text, new RegExp(`客户端来源 ${label}`));
  });
}

test("认不出来的来源保留原文，不猜一个词", async () => {
  const text = await renderFacts(call("some-future-client"));
  assert.match(text, /客户端来源 some-future-client/);
});
