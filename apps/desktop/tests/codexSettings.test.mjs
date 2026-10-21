import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { API_ROOT, FakeBackend, installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
await loader.load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
test.after(() => loader.close());
const state = { enabled: true, listening: true, port: 43120, configured: false, config_conflict: false, restorable: false, conflicts: [], chatgpt_signed_in: true, conversation_detail_mode: "STEPS_COMMANDS", local_mode_guard: false, home: "test-home", calls: 0, last_error: null };

test("a listening Codex gateway is not presented as applied client configuration", async () => {
  const { codexConnectionView } = await loader.load("/src/features/codex/codexConnectionView.ts");
  assert.equal(codexConnectionView(state).state, "pending-apply");
  assert.equal(codexConnectionView({ ...state, configured: true }).state, "taken-over");
  assert.equal(codexConnectionView({ ...state, configured: true, listening: false }).state, "unavailable");
  assert.equal(codexConnectionView({ ...state, configured: true, config_conflict: true }).state, "conflict");
});

test("a managed field the client changed is not a routing conflict and keeps both actions usable", async () => {
  const { codexConnectionView, codexActionGate, codexConflictRows } = await loader.load("/src/features/codex/codexConnectionView.ts");
  // 真实场景：Codex 设置界面把 web_search 写成 "live"，路由本身还是本机网关的。
  const drifted = {
    ...state,
    configured: true,
    restorable: true,
    conflicts: [{ key: "web_search", applied: "\"disabled\"", current: "\"live\"" }],
  };

  assert.equal(codexConnectionView(drifted).state, "taken-over", "a field difference must not read as 配置冲突");
  assert.deepEqual(
    codexConflictRows(drifted.conflicts).map((row) => [row.field, row.applied, row.current]),
    [["联网搜索", "disabled", "live"]],
  );
  const gate = codexActionGate(drifted, { dirty: false, enabled: true });
  assert.equal(gate.restore, true, "with a receipt in place 撤销接管 must stay clickable");
  assert.equal(gate.apply, true, "a field difference does not block 应用到 Codex");
});

test("every managed field the server can report has its own Chinese label", async () => {
  const { codexConflictRows } = await loader.load("/src/features/codex/codexConnectionView.ts");
  const rows = codexConflictRows(
    ["model", "model_provider", "model_catalog_json", "model_context_window", "model_auto_compact_token_limit", "web_search", "model_providers.haxsd-byok"]
      .map((key) => ({ key, applied: "\"a\"", current: "\"b\"" })),
  );

  assert.deepEqual(
    rows.map((row) => row.field),
    ["模型", "模型供应商", "模型目录", "上下文窗口", "自动压缩阈值", "联网搜索", "网关配置"],
  );
});

test("conflict values lose their TOML quotes and an unset field reads as （未设置）", async () => {
  const { codexConflictRows } = await loader.load("/src/features/codex/codexConnectionView.ts");
  const rows = codexConflictRows([
    { key: "model_context_window", applied: "", current: "114000" },
    { key: "model_catalog_json", applied: "\"C:/Users/x/.codex/catalog.json\"", current: "" },
    { key: "some_future_key", applied: "\"live\"", current: "\"off\"" },
  ]);

  assert.deepEqual(rows[0], { key: "model_context_window", field: "上下文窗口", applied: "（未设置）", current: "114000" });
  assert.equal(rows[1].applied, "C:/Users/x/.codex/catalog.json", "the outer TOML quotes come off");
  assert.equal(rows[1].current, "（未设置）", "a field the snapshot never wrote shows as unset");
  assert.equal(rows[2].field, "some_future_key", "a key this build does not know keeps its raw name");
  assert.equal(rows[2].current, "off");
});

test("a route held by another provider blocks apply but not the restore", async () => {
  const { codexActionGate } = await loader.load("/src/features/codex/codexConnectionView.ts");
  const foreignRoute = {
    ...state,
    restorable: true,
    config_conflict: true,
    conflicts: [{ key: "model_provider", applied: "\"haxsd-byok\"", current: "\"openai\"" }],
  };

  const gate = codexActionGate(foreignRoute, { dirty: false, enabled: true });
  assert.equal(gate.apply, false, "writing into someone else's file must stay refused");
  assert.equal(gate.restore, true, "the receipt is still the user's way out");
});

test("the conflict card lists every changed field with both values", async () => {
  const { CodexConflictCard } = await loader.load("/src/features/codex/CodexConflictCard.tsx");
  const markup = renderToStaticMarkup(createElement(CodexConflictCard, {
    status: {
      ...state,
      restorable: true,
      conflicts: [
        { key: "web_search", applied: "\"disabled\"", current: "\"live\"" },
        { key: "model_context_window", applied: "", current: "114000" },
        { key: "model_catalog_json", applied: "\"C:/Users/x/.codex/catalog.json\"", current: "\"C:/Users/x/.codex/generated.json\"" },
      ],
    },
  }));

  assert.match(markup, /受管字段被改过/, "field drift gets its own card title");
  assert.doesNotMatch(markup, /配置冲突/, "field drift is not called a routing conflict");
  assert.match(markup, /联网搜索：接管时 disabled → 现在 live/);
  assert.match(markup, /上下文窗口：接管时 （未设置） → 现在 114000/);
  assert.match(markup, /模型目录：接管时 C:\/Users\/x\/\.codex\/catalog\.json → 现在 C:\/Users\/x\/\.codex\/generated\.json/);
  assert.match(markup, /撤销接管会还原这些字段；要继续使用本机网关，再应用一次。/);
});

test("the conflict card explains a route held by another provider", async () => {
  const { CodexConflictCard } = await loader.load("/src/features/codex/CodexConflictCard.tsx");
  const markup = renderToStaticMarkup(createElement(CodexConflictCard, {
    status: {
      ...state,
      restorable: true,
      config_conflict: true,
      conflicts: [{ key: "model_provider", applied: "\"haxsd-byok\"", current: "\"openai\"" }],
    },
  }));

  assert.match(markup, /配置冲突/);
  assert.match(markup, /Codex 现在使用别人写的 provider/);
  assert.match(markup, /模型供应商：接管时 haxsd-byok → 现在 openai/);
});

test("the conflict card stays out of the page when nothing was changed", async () => {
  const { CodexConflictCard } = await loader.load("/src/features/codex/CodexConflictCard.tsx");
  assert.equal(renderToStaticMarkup(createElement(CodexConflictCard, { status: state })), "");
  assert.equal(renderToStaticMarkup(createElement(CodexConflictCard, { status: { ...state, configured: true } })), "");
  assert.equal(renderToStaticMarkup(createElement(CodexConflictCard, { status: null })), "");
});

test("applied Codex configuration locks settings until an explicit restore", async () => {
  const { CodexSettingsForm } = await loader.load("/src/features/codex/CodexSettingsForm.tsx");
  const { TooltipProvider } = await loader.load("/src/shared/ui/Tooltip.tsx");
  const markup = renderToStaticMarkup(createElement(TooltipProvider, null, createElement(CodexSettingsForm, {
    draft: { enabled: true, port: 43120, home: "", model_hashes: ["hash"], default_model_hash: "hash" },
    models: [], pluginModels: [], locked: true, onChange: () => {},
  })));
  assert.match(markup, /aria-label="Codex 配置目录"[^>]*disabled/);
  assert.match(markup, /aria-label="Codex 网关端口"[^>]*disabled/);
  assert.match(markup, /撤销接管后才能修改/);
});

test("the reason Codex settings are locked belongs to the gateway switch row", async () => {
  const { CodexSettingsForm } = await loader.load("/src/features/codex/CodexSettingsForm.tsx");
  const { TooltipProvider } = await loader.load("/src/shared/ui/Tooltip.tsx");
  const render = (locked) => renderToStaticMarkup(createElement(TooltipProvider, null, createElement(CodexSettingsForm, {
    draft: { enabled: true, port: 43120, home: "", model_hashes: ["hash"], default_model_hash: "hash" },
    models: [], pluginModels: [], locked, onChange: () => {},
  })));
  // The reason used to be a paragraph floating above the form; it has to stay inside the
  // switch's own row, next to the control it explains.
  assert.match(render(true), /启用 Codex 网关<\/strong><small>撤销接管后才能修改<\/small>/);
  assert.match(render(false), /启用 Codex 网关<\/strong><small>关闭时不监听端口<\/small>/);
});

const libraryModel = (hash, name) => ({
  model_hash: hash,
  sort_order: 1,
  display_name: name,
  group_name: null,
  type: "openai",
  base_url: "https://api.example.com/v1",
  use_full_url: false,
  api_key_configured: true,
  tooltip_data: name,
  model_id: `${hash}-id`,
  reasoning_effort: null,
  openai_endpoint: "/v1/responses",
  openai_extra_params_enabled: false,
  openai_extra_params: {},
  custom_headers_enabled: false,
  custom_headers: {},
  anthropic_extra_params_enabled: false,
  anthropic_extra_params: {},
  context_window_tokens: 128_000,
  max_completion_tokens: null,
  anthropic_max_tokens: null,
  anthropic_thinking_effort: null,
  thinking_budget_tokens: null,
  created_at_ms: 0,
  updated_at_ms: 0,
});

const pluginModel = (id, displayName) => ({
  id,
  pluginId: "dev.cursorbyok.examples.grok-auth",
  pluginName: "Grok",
  providerId: "grok",
  modelId: id,
  displayName,
  description: null,
  icon: "",
  providerType: "plugin",
  maxOutputTokens: null,
  images: false,
  enabled: true,
});

test("the Codex picker only offers models the gateway can publish", async () => {
  const { codexModelAvailability } = await loader.load("/src/features/codex/codexModelOptions.ts");
  const grok = pluginModel("plugin:dev.cursorbyok.examples.grok-auth/grok/grok-4.6", "Grok 4.6");
  const availability = codexModelAvailability(
    // 插件模型不会出现在模型库列表里；这里仍然放一个同 id 的条目，锁住"即使出现在
    // 数据源里也不选它"这条规则。
    [libraryModel("aaaa1111", "GPT"), { ...libraryModel(grok.id, "Grok 4.6") }],
    [grok],
    ["aaaa1111", grok.id, "gone2222"],
  );

  // 可选的模型在前；已失效的历史选择排在后面，用户仍然能在列表里取消它们。
  assert.deepEqual(availability.options.map((option) => option.value), ["aaaa1111", "gone2222", grok.id]);
  assert.deepEqual(availability.missingSelections, ["gone2222"]);
  assert.deepEqual(availability.blockedPluginSelections, [grok.id]);
  assert.equal(availability.options.at(-2).group, "模型库里已不存在");
  assert.equal(availability.options.at(-1).group, "插件模型（Codex 不能用）");
  // 插件模型已被选进设置时，"为什么不能选它们"那句说明就多余了：真正要处理的是
  // 已选的那一条。
  assert.deepEqual(availability.blockedPluginModels, []);

  // 没有选中插件模型时，列表本身就该说明它们为什么不在可选范围内。
  const unselected = codexModelAvailability([libraryModel("aaaa1111", "GPT")], [grok], ["aaaa1111"]);
  assert.deepEqual(unselected.blockedPluginModels.map((model) => model.id), [grok.id]);
  assert.deepEqual(unselected.blockedPluginSelections, []);
  assert.deepEqual(unselected.missingSelections, []);
});

test("the Codex form says why plugin models are not offered", async () => {
  const { CodexSettingsForm } = await loader.load("/src/features/codex/CodexSettingsForm.tsx");
  const { TooltipProvider } = await loader.load("/src/shared/ui/Tooltip.tsx");
  const markup = renderToStaticMarkup(createElement(TooltipProvider, null, createElement(CodexSettingsForm, {
    draft: { enabled: true, port: 43120, home: "", model_hashes: ["hash"], default_model_hash: "hash" },
    models: [libraryModel("hash", "GPT")],
    pluginModels: [pluginModel("plugin:dev.cursorbyok.examples.grok-auth/grok/grok-4.6", "Grok 4.6")],
    locked: false,
    onChange: () => {},
  })));
  assert.match(markup, /插件模型的 1 个模型不可用/);
});

test("a selected model that left the library is called out instead of silently kept", async () => {
  const { CodexSettingsForm } = await loader.load("/src/features/codex/CodexSettingsForm.tsx");
  const { TooltipProvider } = await loader.load("/src/shared/ui/Tooltip.tsx");
  const markup = renderToStaticMarkup(createElement(TooltipProvider, null, createElement(CodexSettingsForm, {
    draft: { enabled: true, port: 43120, home: "", model_hashes: ["gone2222"], default_model_hash: "gone2222" },
    models: [libraryModel("hash", "GPT")],
    pluginModels: [],
    locked: false,
    onChange: () => {},
  })));
  assert.match(markup, /1 个已选模型不在模型库/);
});

test("Codex settings and explicit apply/restore stay on the shared management HTTP boundary", async () => {
  const settings = { enabled: true, port: 43120, home: "", model_hashes: ["hash"], default_model_hash: "hash" };
  const backend = new FakeBackend()
    .route("GET", `${API_ROOT}/codex/settings`, () => ({ body: settings }))
    .route("PUT", `${API_ROOT}/codex/settings`, ({ body }) => ({ body }))
    .route("GET", `${API_ROOT}/codex/status`, () => ({ body: state }))
    .route("POST", `${API_ROOT}/codex/apply`, () => ({ body: { ...state, configured: true } }))
    .route("POST", `${API_ROOT}/codex/restore`, () => ({ body: state }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");
  assert.deepEqual(await api.codexSettings(), settings);
  assert.deepEqual(await api.setCodexSettings(settings), settings);
  assert.equal((await api.codexStatus()).configured, false);
  assert.equal((await api.applyCodex()).configured, true);
  assert.equal((await api.restoreCodex()).configured, false);
  assert.equal(backend.requestsFor("PUT", `${API_ROOT}/codex/settings`).length, 1);
  assert.equal(backend.requestsFor("POST", `${API_ROOT}/codex/apply`).length, 1);
  assert.equal(backend.requestsFor("POST", `${API_ROOT}/codex/restore`).length, 1);
});

test("ChatGPT login is a hint that never changes the routing state", async () => {
  const { codexLoginState, codexApplyNeedsSignInConfirm, codexConnectionView } = await loader.load("/src/features/codex/codexConnectionView.ts");
  assert.equal(codexLoginState(null), "checking");
  assert.equal(codexLoginState({ ...state, chatgpt_signed_in: true }), "signed-in");
  assert.equal(codexLoginState({ ...state, chatgpt_signed_in: false }), "signed-out");
  assert.equal(codexApplyNeedsSignInConfirm({ ...state, chatgpt_signed_in: false }), true);
  assert.equal(codexApplyNeedsSignInConfirm({ ...state, chatgpt_signed_in: true }), false);
  // 未登录只影响客户端账号能力，接管状态本身仍是"已接管"。
  assert.equal(codexConnectionView({ ...state, configured: true, chatgpt_signed_in: false }).state, "taken-over");
});

test("login and client mode are compact pills, not paragraphs", async () => {
  const { codexLoginView, codexClientModeView } = await loader.load("/src/features/codex/codexConnectionView.ts");
  assert.equal(codexLoginView(null).label, "登录检测中…");
  assert.equal(codexLoginView({ ...state, chatgpt_signed_in: true }).label, "ChatGPT 已登录");
  assert.equal(codexLoginView({ ...state, chatgpt_signed_in: false }).label, "ChatGPT 未登录");

  // 未登录时把客户端实际模式摆在旁边；已登录时 Work 模式是正常的，不再提示。
  assert.equal(codexClientModeView({ ...state, chatgpt_signed_in: false, conversation_detail_mode: "STEPS_COMMANDS" }).label, "本地模式");
  assert.equal(codexClientModeView({ ...state, chatgpt_signed_in: false, conversation_detail_mode: "STEPS_PROSE" }).label, "Work 模式");
  assert.equal(codexClientModeView({ ...state, chatgpt_signed_in: true, conversation_detail_mode: "STEPS_PROSE" }), null);
});

test("the status card lists at most two next actions and shorter descriptions", async () => {
  const { codexOutstandingSteps, codexVerdictDescription } = await loader.load("/src/features/codex/codexConnectionView.ts");
  const pending = { ...state, configured: false, listening: false, enabled: false, chatgpt_signed_in: false };
  const steps = codexOutstandingSteps(pending, 0);
  assert.equal(steps.length, 2);
  assert.deepEqual(steps.map((step) => step.label), ["添加模型", "启用网关"]);
  assert.equal(codexOutstandingSteps({ ...pending, configured: true }, 0).length, 0, "已接管后不再列步骤");

  for (const stateName of ["checking", "disabled", "pending-apply", "taken-over", "unavailable", "conflict"]) {
    assert.ok(codexVerdictDescription(stateName).length <= 30, `${stateName} 的状态说明要短`);
  }
});
