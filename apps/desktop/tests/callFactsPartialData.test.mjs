// 调用明细在缺字段时的显示。
//
// CallFacts 只判断 `=== null`：服务端少给一个字段（改名、旧记录不完整）时值是 undefined，
// 面板会印出 "undefined" 或 "Invalid Date"——看起来像应用坏了。这里用真实渲染把占位符钉住。
//
// Harness note：`CallFacts` 读应用 store（useSyncExternalStore），服务端渲染需要
// getServerSnapshot，用与 callFactsCopy.test.mjs 相同的注入方式补上。

import assert from "node:assert/strict";
import test from "node:test";
import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";
import { desktopRoot, installBrowserDoubles } from "./harness.mjs";

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
const { setLocalePreference } = await load("/src/i18n/store.ts");
setLocalePreference("zh-CN");

test("a call row without any optional field shows dashes, not undefined", async () => {
  const { CallFacts } = await load("/src/features/calls/CallFacts.tsx");
  const markup = renderToStaticMarkup(h(CallFacts, {
    call: { call_id: "call-1", client_source: "cursor", route: "local_byok", status: "completed", display_name: "主力模型", model_id: "gpt-5" },
  }));
  const text = markup.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();

  assert.doesNotMatch(text, /undefined/, "missing counts must not be printed as undefined");
  assert.doesNotMatch(text, /Invalid Date/);
  assert.match(text, /输入 Token -/);
  assert.match(text, /输出 Token -/);
  assert.match(text, /总耗时 -/);
});

test("a complete call row still shows its numbers", async () => {
  const { CallFacts } = await load("/src/features/calls/CallFacts.tsx");
  const markup = renderToStaticMarkup(h(CallFacts, {
    call: {
      call_id: "call-1",
      client_source: "cursor",
      route: "local_byok",
      status: "completed",
      display_name: "主力模型",
      model_id: "gpt-5",
      input_tokens: 4_800,
      output_tokens: 820,
      cache_read_tokens: 3_100,
      cache_write_tokens: 320,
      reasoning_tokens: 0,
      duration_ms: 1_420,
    },
  }));
  const text = markup.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();

  assert.match(text, /输入 Token 4800/);
  assert.match(text, /输出 Token 820/);
  assert.match(text, /推理 Token 0/, "a real zero stays a zero, not a dash");
});
