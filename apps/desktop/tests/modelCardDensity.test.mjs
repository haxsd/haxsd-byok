// Behavior tests for the model library card grid (features/models).
//
// The contract: a card only carries a test row when there is something to say.
// Rendering "未测试" on all 16 cards was the noise that made the grid unreadable,
// so the row must appear for a running or finished test and stay absent otherwise.
//
// Harness note: same as modelEditorLayout.test.mjs — compile one SCSS module before
// the browser doubles so sass's Dart runtime never sees the minimal `document` stub.

import assert from "node:assert/strict";
import test from "node:test";
import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
await loader.load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
// The shared doubles carry a minimal document; sortablejs probes the DOM while it
// loads (feature detection only), so give it one throwaway element to probe plus
// listener no-ops. Nothing here is exercised at render time — Sortable.create only
// runs from useEffect, which server rendering never reaches.
const probe = { style: {}, dataset: {}, appendChild() {}, removeChild() {}, setAttribute() {}, addEventListener() {}, removeEventListener() {} };
globalThis.document.createElement = () => probe;
globalThis.document.addEventListener = () => {};
globalThis.document.removeEventListener = () => {};
test.after(() => loader.close());

function model(overrides = {}) {
  return {
    model_hash: "hash-1",
    sort_order: 1,
    display_name: "主力模型",
    group_name: null,
    type: "openai",
    base_url: "https://api.example.com",
    use_full_url: false,
    api_key_configured: true,
    tooltip_data: "",
    model_id: "gpt-5",
    reasoning_effort: null,
    openai_endpoint: "/v1/responses",
    openai_extra_params_enabled: false,
    openai_extra_params: {},
    custom_headers_enabled: false,
    custom_headers: {},
    anthropic_extra_params_enabled: false,
    anthropic_extra_params: {},
    context_window_tokens: null,
    max_completion_tokens: null,
    anthropic_max_tokens: null,
    anthropic_thinking_effort: null,
    thinking_budget_tokens: null,
    created_at_ms: 0,
    updated_at_ms: 0,
    ...overrides,
  };
}

/** Render the flat grid with the given test results. */
async function renderCards(testResults = new Map()) {
  const [{ CursorModelCards }, { TooltipProvider }] = await Promise.all([
    loader.load("/src/features/models/CursorModelCards.tsx"),
    loader.load("/src/shared/ui/Tooltip.tsx"),
  ]);
  const noop = () => {};
  return renderToStaticMarkup(h(TooltipProvider, null, h(CursorModelCards, {
    models: [model()],
    pluginModels: [],
    grouping: "flat",
    disabled: false,
    allowReorder: true,
    testingModelHashes: new Set(),
    testResults,
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

test("an untested model card does not reserve a test row", async () => {
  const markup = await renderCards();
  assert.match(markup, /主力模型/, "the card itself must render");
  assert.match(markup, /gpt-5/, "the model id stays on the card");
  assert.doesNotMatch(markup, /未测试/, "no placeholder row when nothing was tested");
});

test("a finished test puts the measured speed back on the card", async () => {
  const markup = await renderCards(new Map([["hash-1", {
    status: "success",
    result: { duration_ms: 1200, first_valid_response_ms: 300, output_tokens: 40, tokens_per_second: 12.5, tokens_estimated: false, output: "ok" },
  }]]));
  assert.match(markup, /12\.5 tokens\/s/, "the test result must be visible on the card");
  assert.doesNotMatch(markup, /未测试/);
});
