// 缺字段的数据怎么显示。
//
// 「服务端少给一个字段」比「服务端返回 null 行」更常见（改名、旧版本、某条记录不完整）。
// 这一组断言的是：缺字段时显示的是占位符 "-" 或干脆不显示那一项，而不是把 undefined /
// Invalid Date 印在界面上——那看起来像应用坏了，而不是这一行缺数据。
//
// 实测背景：`/api/models` 返回一行只有 model_hash / model_id / display_name 的模型时，
// 模型库卡片上出现了「上下文 undefined」「输出上限 undefined」。

import assert from "node:assert/strict";
import test from "node:test";
import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
await loader.load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
const probe = { style: {}, dataset: {}, appendChild() {}, removeChild() {}, setAttribute() {}, addEventListener() {}, removeEventListener() {} };
globalThis.document.createElement = () => probe;
globalThis.document.addEventListener = () => {};
globalThis.document.removeEventListener = () => {};
test.after(() => loader.close());

/** 模型行：只有渲染卡片真正需要的三个字段，其余全部缺失。 */
function partialModel(overrides = {}) {
  return {
    model_hash: "hash-1",
    model_id: "mid",
    display_name: "只有名字的模型",
    type: "openai",
    ...overrides,
  };
}

async function renderCard(model) {
  const [{ CursorModelCards }, { TooltipProvider }] = await Promise.all([
    loader.load("/src/features/models/CursorModelCards.tsx"),
    loader.load("/src/shared/ui/Tooltip.tsx"),
  ]);
  const noop = () => {};
  return renderToStaticMarkup(h(TooltipProvider, null, h(CursorModelCards, {
    models: [model],
    pluginModels: [],
    grouping: "flat",
    disabled: false,
    allowReorder: true,
    testingModelHashes: new Set(),
    testResults: new Map(),
    onTest: noop,
    onEdit: noop,
    onDuplicate: noop,
    onDelete: noop,
    onTestPluginModel: noop,
    onPluginSettings: noop,
    onReorder: noop,
    onGroupSettings: noop,
  })));
}

test("a model row without its numeric fields renders no chip and no undefined", async () => {
  const markup = await renderCard(partialModel());
  assert.match(markup, /只有名字的模型/, "the card still renders");
  assert.doesNotMatch(markup, /undefined/, "missing numbers must not be printed as undefined");
  assert.doesNotMatch(markup, /上下文/, "an unknown context window shows no chip at all");
  assert.doesNotMatch(markup, /输出上限/, "an unknown output limit shows no chip at all");
});

test("a model row with a real context window still shows the chip", async () => {
  const markup = await renderCard(partialModel({ context_window_tokens: 200_000, max_completion_tokens: 32_000 }));
  assert.match(markup, /上下文 200K/);
  assert.match(markup, /输出上限 32K/);
});
