// Behavior tests for the model editor's advanced-field grouping (features/models).
//
// The contract pinned here is about what the user can *see* when the editor opens:
//
// 1. A brand-new draft (every field at its default) shows the everyday fields and
//    keeps the advanced ones folded — the form is 14 inputs deep otherwise.
// 2. A draft that already carries advanced values (context window, custom headers,
//    a note, thinking budget, …) opens with the advanced group expanded, so an
//    existing model never looks like its configuration was dropped.
//
// The assertions only look at rendered fields and the disclosure's expanded state,
// never at class names or style constants, so a re-styled editor keeps passing.
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

/** The everyday fields that must stay in the first view. */
const COMMON_FIELDS = [
  /aria-label="模型类型"/,
  /aria-label="请求协议"/,
  /服务器地址/,
  /模型名称/,
  /显示名称/,
  // 备注是保存时的必填项，必须在第一屏：折进「高级」会让不展开的用户永远存不上。
  /请输入模型备注/,
];

/** Fields that only appear once the advanced group is open. */
const ADVANCED_FIELDS = [
  /上下文窗口 Token/,
  /思考预算 Token/,
  /自定义 Headers/,
];

/** Which probes matched, reported as the probe list itself so failures stay short. */
function probesFound(markup, probes) {
  return probes.filter((probe) => probe.test(markup));
}

/** Render the real editor inside the tooltip provider its form fields require. */
async function renderEditor(overrides = {}) {
  const [{ CursorModelEditor, emptyCursorModelDraft }, { TooltipProvider }] = await Promise.all([
    loader.load("/src/features/models/CursorModelEditor.tsx"),
    loader.load("/src/shared/ui/Tooltip.tsx"),
  ]);
  const draft = overrides.draft ?? emptyCursorModelDraft();
  return renderToStaticMarkup(h(TooltipProvider, null, h(CursorModelEditor, {
    modelOptions: [],
    discovering: false,
    hasStoredKey: false,
    onChange: () => {},
    onDiscover: async () => false,
    ...overrides,
    draft,
  })));
}

async function draftWith(model) {
  const { emptyCursorModelDraft } = await loader.load("/src/features/models/CursorModelEditor.tsx");
  const draft = emptyCursorModelDraft();
  return { ...draft, model: { ...draft.model, ...model } };
}

test("a brand-new draft shows the everyday fields with the advanced group folded", async () => {
  const markup = await renderEditor();
  assert.deepEqual(probesFound(markup, COMMON_FIELDS), COMMON_FIELDS, "the everyday fields must all be rendered");
  assert.match(markup, /高级/, "the folded group still announces itself");
  assert.deepEqual(probesFound(markup, ADVANCED_FIELDS), [], "advanced fields must stay out of the first view");
});

test("a stored context window opens the advanced group without a click", async () => {
  const draft = await draftWith({ context_window_tokens: 200_000 });
  const markup = await renderEditor({ draft });
  assert.match(markup, /aria-expanded="true"/, "the advanced disclosure must start expanded");
  assert.match(markup, /上下文窗口 Token/, "the stored context window must be visible");
  assert.match(markup, /自定义 Headers/, "the rest of the advanced fields come with it");
});

test("enabled custom headers open the advanced group and show their JSON editor", async () => {
  const draft = await draftWith({ custom_headers_enabled: true });
  const markup = await renderEditor({ draft, hasStoredKey: true });
  assert.match(markup, /aria-label="自定义 Headers"/, "the headers switch must be visible");
  assert.match(markup, /格式化/, "an enabled headers editor must render its editor body");
});

test("a saved note stays visible while the advanced group is folded", async () => {
  const draft = await draftWith({ tooltip_data: "主力模型" });
  const markup = await renderEditor({ draft, hasStoredKey: true });
  assert.match(markup, /value="主力模型"/, "the saved note must stay visible");
  assert.deepEqual(probesFound(markup, ADVANCED_FIELDS), [], "a note alone must not unfold the advanced group");
});

test("advancedValueCount counts only the fields the user actually set", async () => {
  const { advancedValueCount } = await loader.load("/src/features/models/CursorModelEditor.tsx");
  const { emptyCursorModelDraft } = await loader.load("/src/features/models/CursorModelEditor.tsx");
  const fresh = emptyCursorModelDraft();
  assert.equal(advancedValueCount(fresh), 0, "a brand-new draft has nothing set");
  assert.equal(advancedValueCount({ ...fresh, model: { ...fresh.model, tooltip_data: "备注" } }), 0, "the note is not an advanced field");
  assert.equal(advancedValueCount({ ...fresh, model: { ...fresh.model, context_window_tokens: 200_000 } }), 1);
  assert.equal(
    advancedValueCount({ ...fresh, model: { ...fresh.model, context_window_tokens: 200_000, custom_headers_enabled: true } }),
    2,
  );
});

test("editing a model whose advanced fields are all default keeps the group folded", async () => {
  const draft = await draftWith({});
  const markup = await renderEditor({ draft, hasStoredKey: true });
  assert.deepEqual(probesFound(markup, ADVANCED_FIELDS), [], "an all-default draft must not render advanced fields");
});
