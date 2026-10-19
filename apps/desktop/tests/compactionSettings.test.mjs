// 上下文压缩设置卡：服务端校验规则的桌面侧镜像。
//
// 卡片自己的取值/校验/选项构造是纯函数，读模式与编辑模式也拆成了可静态渲染的
// 组件，所以这里不需要 DOM 就能钉住三件事：越界输入在发请求前就被拦下、读模式
// 真的把「摘要输出上限」画出来、已删除的摘要模型不会被悄悄从选项里丢掉。

import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
// Compile Sass before the minimal DOM stub, as in the other rendering tests.
await loader.load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
test.after(() => loader.close());

const card = await loader.load("/src/features/settings/CompactionSettingsCard.tsx");

const model = (hash, displayName, type = "anthropic", groupName = null, baseUrl = "https://api.example.com") => ({
  model_hash: hash,
  sort_order: 0,
  display_name: displayName,
  group_name: groupName,
  type,
  base_url: baseUrl,
  use_full_url: false,
  api_key_configured: true,
  tooltip_data: "",
  model_id: displayName.toLowerCase(),
  reasoning_effort: null,
  openai_endpoint: "/v1/chat/completions",
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
});

const models = [model("aaaa1111", "Summary Cheap", "anthropic", "Provider A"), model("bbbb2222", "Summary Big", "openai", null, "https://api.provider-b.com/v1")];

const draft = (overrides = {}) => ({ ...card.compactionDraft({ auto: true, summary_model_hash: "", summary_output_tokens: null, reserve_percent: null, retention_percent: null, max_retained_tokens: null }), ...overrides });
const settings = (overrides = {}) => ({ auto: true, summary_model_hash: "", summary_output_tokens: null, reserve_percent: null, retention_percent: null, max_retained_tokens: null, ...overrides });

test("an empty draft saves as nulls, so the server keeps its own defaults", () => {
  assert.deepEqual(card.compactionSettingsFromDraft(draft()), settings());
  assert.deepEqual(card.compactionSettingsFromDraft(draft({ auto: false })), settings({ auto: false }));
});

test("token caps and the reserve/retention sum are rejected before the request", () => {
  const max = card.COMPACTION_SUMMARY_OUTPUT_TOKENS_MAX;
  assert.equal(card.compactionSettingsFromDraft(draft({ summary_output_tokens: String(max) })).summary_output_tokens, max);
  assert.throws(() => card.compactionSettingsFromDraft(draft({ summary_output_tokens: String(max + 1) })), /不能超过 32768/);
  assert.equal(card.compactionSettingsFromDraft(draft({ max_retained_tokens: "1000000" })).max_retained_tokens, 1_000_000);
  assert.throws(() => card.compactionSettingsFromDraft(draft({ max_retained_tokens: "1000001" })), /不能超过 1000000/);

  // 50 + 30 = 80 is allowed; 50 + 31 = 81 is not; an empty field counts as its default.
  assert.deepEqual(card.compactionSettingsFromDraft(draft({ reserve_percent: "50", retention_percent: "30" })), settings({ reserve_percent: 50, retention_percent: 30 }));
  assert.throws(() => card.compactionSettingsFromDraft(draft({ reserve_percent: "50", retention_percent: "31" })), /之和不能超过 80/);
  assert.deepEqual(card.compactionSettingsFromDraft(draft({ retention_percent: "50" })), settings({ retention_percent: 50 }));
  assert.deepEqual(card.compactionSettingsFromDraft(draft({ reserve_percent: "45" })), settings({ reserve_percent: 45 }));
});

test("non-numeric and out-of-range percents are rejected", () => {
  for (const bad of ["0", "-5", "abc", "1.5", "51"]) {
    assert.throws(() => card.compactionSettingsFromDraft(draft({ retention_percent: bad })), /必须是正整数|1–50/, bad);
  }
});

test("a saved summary model that no longer exists stays visible as its own option", () => {
  const options = card.compactionModelOptions(models, "");
  assert.equal(options[0].value, "");
  assert.equal(options[0].label, "跟随会话模型");
  assert.deepEqual(options.slice(1).map((option) => option.value), ["aaaa1111", "bbbb2222"]);
  assert.deepEqual(options.slice(1).map((option) => option.group), ["Provider A", "api.provider-b.com"]);

  assert.equal(card.compactionModelOptions(models, "aaaa1111").length, 3);
  const missing = card.compactionModelOptions(models, "deadbeef");
  assert.equal(missing.length, 4);
  assert.equal(missing[3].value, "deadbeef");
  assert.match(missing[3].label, /已删除，将回退到会话模型/);
  assert.equal(missing[3].group, "已删除的摘要模型");
});

test("read mode shows the summary output cap that used to be hidden", () => {
  const markup = renderToStaticMarkup(createElement(card.CompactionSettingsRows, { settings: settings(), models }));
  assert.match(markup, /摘要输出上限/);
  assert.match(markup, /默认 8192/);
  assert.match(markup, /跟随会话模型/);
  assert.match(markup, /10% \/ 20%/);
  assert.match(markup, /未设置/);

  const custom = renderToStaticMarkup(createElement(card.CompactionSettingsRows, { settings: settings({ auto: false, summary_model_hash: "aaaa1111", summary_output_tokens: 2048, max_retained_tokens: 32768 }), models }));
  assert.match(custom, /2048 tokens/);
  assert.match(custom, /Summary Cheap/);
});

test("edit mode exposes the server bounds as input limits", async () => {
  const { TooltipProvider } = await loader.load("/src/shared/ui/Tooltip.tsx");
  const markup = renderToStaticMarkup(createElement(TooltipProvider, null,
    createElement(card.CompactionSettingsFields, { draft: draft(), models, disabled: false, onChange: () => {} })));
  assert.match(markup, /max="32768"/);
  assert.match(markup, /max="1000000"/);
  assert.match(markup, /max="50"/);
  assert.match(markup, /aria-label="筛选项：摘要模型/);
});
