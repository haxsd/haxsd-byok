// Behavior tests for the Cursor page's 功能设置 section (features/cursor).
//
// Two contracts are pinned here:
//
// 1. `?section=tools` is the only value that opens the tools section — the system
//    settings page links "Tab 补全" and "提交信息模型" straight to it.
// 2. The TAB card shows its edit form, and shows a failed read inline with a retry,
//    instead of folding the body away.
//
// Harness note: the SCSS module is warmed up *before* the browser doubles are
// installed. sass's Dart runtime sniffs for a global `document` while it loads and
// throws on the minimal stub; one compile first keeps the runtime loaded, and later
// SCSS compiles then work with the doubles in place.

import assert from "node:assert/strict";
import test from "node:test";
import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
await loader.load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
test.after(() => loader.close());

async function cursorSettingsSection(query) {
  const { cursorSettingsSection: parse } = await loader.load("/src/features/cursor/toolsSection.ts");
  return parse(new URLSearchParams(query));
}

/** Render the TAB card with the given overrides; every callback is a no-op. */
async function renderTabCard(overrides) {
  const { TabSettingsCard } = await loader.load("/src/features/cursor/TabSettingsCard.tsx");
  const noop = () => {};
  return renderToStaticMarkup(h(TabSettingsCard, {
    settings: null,
    draft: { mode: "public", address: "" },
    editing: false,
    saving: false,
    error: null,
    onDraftChange: noop,
    onEdit: noop,
    onCancel: noop,
    onSave: noop,
    onRetry: noop,
    ...overrides,
  }));
}

test("section=tools opens the tools section, every other value stays on access", async () => {
  assert.equal(await cursorSettingsSection("section=tools"), "tools");
  assert.equal(await cursorSettingsSection("section=tools&q=commit"), "tools");
  assert.equal(await cursorSettingsSection(""), "access");
  assert.equal(await cursorSettingsSection("section=import"), "access");
  assert.equal(await cursorSettingsSection("?section=tools"), "tools");
});

test("TAB drafts are trimmed and an empty custom address is refused before the request", async () => {
  const { normalizeTabDraft } = await loader.load("/src/features/cursor/CursorToolsSettings.tsx");
  assert.deepEqual(
    normalizeTabDraft({ mode: "custom", address: "  https://tab.example  " }),
    { mode: "custom", address: "https://tab.example" },
  );
  assert.deepEqual(normalizeTabDraft({ mode: "public", address: "" }), { mode: "public", address: "" });
  assert.deepEqual(
    normalizeTabDraft({ mode: "direct", address: " https://tab.example " }),
    { mode: "direct", address: "https://tab.example" },
  );
  assert.throws(
    () => normalizeTabDraft({ mode: "custom", address: "   " }),
    /TAB 服务地址不能为空/,
  );
});

test("editing the TAB card renders the form instead of a folded card", async () => {
  const markup = await renderTabCard({
    settings: { mode: "custom", address: "https://tab.example" },
    draft: { mode: "custom", address: "https://tab.example" },
    editing: true,
  });
  assert.match(markup, /aria-label="TAB 服务地址"/, "the address field must be visible while editing");
  assert.match(markup, /aria-label="TAB 选择"/, "the mode selector must be visible while editing");
  assert.match(markup, /value="https:\/\/tab\.example"/, "the draft keeps the saved address");
  assert.match(markup, /保存/, "the edit state offers save");
  assert.match(markup, /取消/, "the edit state offers cancel");
});

test("reading the TAB card shows the saved value without any edit field", async () => {
  const markup = await renderTabCard({ settings: { mode: "custom", address: "https://tab.example" } });
  assert.match(markup, /自定义/);
  assert.match(markup, /https:\/\/tab\.example/);
  assert.doesNotMatch(markup, /aria-label="TAB 服务地址"/);
});

test("a failed TAB read is reported inline with a retry beside it", async () => {
  const markup = await renderTabCard({ error: "连接本地管理服务失败" });
  assert.match(markup, /role="alert"/);
  assert.match(markup, /读取 TAB 设置失败：连接本地管理服务失败/);
  assert.match(markup, /重试/);
  assert.match(markup, /读取失败/);
  assert.doesNotMatch(markup, /加载中…/, "a finished read must not keep saying it is loading");
});
