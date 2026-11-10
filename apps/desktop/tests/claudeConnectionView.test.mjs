// Claude 接入页的状态推导、Desktop 接管门槛与渲染。
//
// 这些规则决定"按钮能不能点、点了会写什么"，所以用真实模块做行为测试；页面渲染靠
// 这些纯函数，不必为了它们启动整个 WebView。

import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { API_ROOT, FakeBackend, installBrowserDoubles, startModuleLoader } from "./harness.mjs";

/** 与服务端 Rust 测试共用的真实响应：两边都用它，字段漂移会同时失败。 */
const serverTakeover = JSON.parse(
  fs.readFileSync(new URL("./fixtures/claude-takeover-status.json", import.meta.url), "utf8"),
);

const loader = await startModuleLoader();
const loadClaude = async () => ({
  ...(await loader.load("/src/features/claude/claudeConnectionView.ts")),
  ...(await loader.load("/src/features/claude/claudeTakeoverPlan.ts")),
});
await loader.load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
test.after(() => loader.close());

const status = {
  enabled: true,
  listening: true,
  port: 43_121,
  base_url: "http://127.0.0.1:43121",
  token: "haxsd-test-token",
  models: 3,
  default_model_hash: "aaaa1111",
  calls: 2,
  last_error: null,
};

const settings = {
  enabled: true,
  port: 43_121,
  default_model_hash: "aaaa1111",
  sonnet_model_hash: "aaaa1111",
  opus_model_hash: "bbbb2222",
  haiku_model_hash: null,
};

const takeover = {
  desktop: {
    state: "ready",
    config_source: "local",
    profile_path: "/home/u/Library/Application Support/Claude",
    backup_path: "",
    restorable: false,
    conflicts: [],
    guided: false,
  },
};

const gateInput = (overrides = {}) => ({
  settings,
  status,
  takeover,
  dirty: false,
  desktopModels: ["aaaa1111"],
  ...overrides,
});

const libraryModel = (hash, name) => ({
  model_hash: hash,
  sort_order: 1,
  display_name: name,
  group_name: "Example",
  type: "anthropic",
  base_url: "https://api.example.com",
  use_full_url: false,
  api_key_configured: true,
  tooltip_data: name,
  model_id: `${hash}-id`,
  reasoning_effort: null,
  openai_endpoint: "",
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

test("the entry counts Claude only when Desktop is actually applied", async () => {
  const { claudeConnectionView } = await loadClaude();

  assert.equal(claudeConnectionView(null, null).state, "checking");
  assert.equal(claudeConnectionView(status, null).state, "checking", "takeover must be read before claiming a state");

  const applied = { desktop: { ...takeover.desktop, state: "applied", backup_path: "/tmp/backup.json", restorable: true } };
  assert.equal(claudeConnectionView(status, applied).state, "taken-over");
  assert.equal(claudeConnectionView(status, applied).tone, "ok");
  assert.equal(claudeConnectionView(status, applied).label, "已接管");

  const conflict = { desktop: { ...takeover.desktop, state: "conflict" } };
  assert.equal(claudeConnectionView(status, conflict).state, "conflict");
  assert.equal(claudeConnectionView(status, conflict).tone, "bad");

  assert.equal(claudeConnectionView({ ...status, enabled: false, listening: false }, takeover).state, "disabled");
  assert.equal(claudeConnectionView({ ...status, listening: false }, takeover).state, "unavailable");
  assert.equal(
    claudeConnectionView(status, { desktop: { ...takeover.desktop, state: "managed" } }).state,
    "managed",
  );
  assert.equal(claudeConnectionView(status, takeover).state, "pending-apply");
  assert.equal(
    claudeConnectionView(status, { desktop: { ...takeover.desktop, state: "missing" } }).state,
    "idle",
  );
});

test("a stale Desktop is amber and outranks applied, but never hides a conflict", async () => {
  const { claudeConnectionView, claudeTakeoverStateLabel, claudeTakeoverStateTone } = await loadClaude();

  const stale = claudeConnectionView(status, { desktop: { ...takeover.desktop, state: "stale" } });
  assert.equal(stale.state, "stale");
  assert.equal(stale.tone, "warn", "stale is a warning, not a healthy green");
  assert.equal(stale.label, "配置已过期");
  assert.equal(
    claudeConnectionView(status, { desktop: { ...takeover.desktop, state: "conflict" } }).state,
    "conflict",
    "conflict still wins over stale",
  );
  assert.equal(
    claudeConnectionView({ ...status, enabled: false, listening: false }, { desktop: { ...takeover.desktop, state: "stale" } }).state,
    "stale",
    "a disabled gateway does not turn a stale client back into 'disabled'",
  );

  // 网关关着或没监听时不能继续显示绿色「已接管」：那样会让用户以为请求正在走网关。
  const applied = { desktop: { ...takeover.desktop, state: "applied" } };
  assert.equal(claudeConnectionView({ ...status, enabled: false, listening: false }, applied).state, "disabled");
  assert.equal(claudeConnectionView({ ...status, listening: false }, applied).state, "unavailable");

  assert.equal(claudeTakeoverStateLabel("stale"), "配置已过期");
  assert.equal(claudeTakeoverStateTone("stale"), "warn");
});

test("stale allows apply and undo; it is not a conflict", async () => {
  const { claudeActionGate } = await loadClaude();

  const staleRestorable = { desktop: { ...takeover.desktop, state: "stale", restorable: true } };
  assert.deepEqual(claudeActionGate(gateInput({ takeover: staleRestorable })), {
    apply: true,
    undo: true,
    applyBlockedReason: null,
  });

  // 过期不代表可以绕过网关：网关没监听时仍然拦下。
  assert.match(
    claudeActionGate(gateInput({
      status: { ...status, listening: false },
      takeover: staleRestorable,
    })).applyBlockedReason,
    /还没有监听/,
  );
});

test("token rotation is refused while the Desktop takeover is applied, stale or running", async () => {
  const { claudeTokenRotationGate } = await loadClaude();

  assert.deepEqual(claudeTokenRotationGate(null), { allowed: true, reason: null });
  assert.equal(claudeTokenRotationGate(takeover).allowed, true, "an unapplied Desktop does not block rotation");

  for (const state of ["applied", "stale", "running"]) {
    const gate = claudeTokenRotationGate({ desktop: { ...takeover.desktop, state } });
    assert.equal(gate.allowed, false, `desktop ${state} must disable rotation`);
    assert.match(gate.reason, /先撤销 Claude Desktop 接管（运行中先完全退出）再轮换令牌。/);
  }
});

test("apply stays disabled with a readable reason until the gateway and client are ready", async () => {
  const { claudeActionGate } = await loadClaude();

  assert.equal(claudeActionGate(gateInput()).apply, true);

  const disabled = claudeActionGate(gateInput({ settings: { ...settings, enabled: false } }));
  assert.equal(disabled.apply, false);
  assert.match(disabled.applyBlockedReason, /先启用/);

  assert.match(
    claudeActionGate(gateInput({ status: { ...status, listening: false } })).applyBlockedReason,
    /还没有监听/,
  );
  assert.match(
    claudeActionGate(gateInput({ settings: { ...settings, default_model_hash: null } })).applyBlockedReason,
    /默认模型/,
  );
  assert.match(claudeActionGate(gateInput({ dirty: true })).applyBlockedReason, /未保存/);
  assert.match(
    claudeActionGate(gateInput({ takeover: { desktop: { ...takeover.desktop, state: "missing" } } })).applyBlockedReason,
    /没有检测到 Claude Desktop/,
  );
  assert.match(
    claudeActionGate(gateInput({ takeover: { desktop: { ...takeover.desktop, state: "managed" } } })).applyBlockedReason,
    /手工填写/,
  );
  assert.match(
    claudeActionGate(gateInput({ takeover: { desktop: { ...takeover.desktop, state: "conflict" } } })).applyBlockedReason,
    /冲突/,
  );
});

test("a running or guided Claude Desktop is a blocker, not an apply button", async () => {
  const { claudeActionGate } = await loadClaude();

  const running = claudeActionGate(gateInput({
    takeover: { desktop: { ...takeover.desktop, state: "running" } },
  }));
  assert.equal(running.apply, false);
  assert.match(running.applyBlockedReason, /完全退出/);

  const guided = claudeActionGate(gateInput({
    takeover: { desktop: { ...takeover.desktop, guided: true } },
  }));
  assert.equal(guided.apply, false, "the guided path never writes the profile");
  assert.match(guided.applyBlockedReason, /手工填写/);

  assert.match(
    claudeActionGate(gateInput({ desktopModels: [] })).applyBlockedReason,
    /至少选择一个模型/,
  );
});

test("undo follows the server receipt, not the gateway switch or the backup file", async () => {
  const { claudeActionGate } = await loadClaude();

  const restorable = {
    desktop: { ...takeover.desktop, backup_path: "/tmp/desktop.bak", restorable: true },
  };
  const off = { ...settings, enabled: false };

  assert.deepEqual(claudeActionGate(gateInput({ settings: off, takeover: restorable })), {
    apply: false,
    undo: true,
    applyBlockedReason: "先启用并保存 Claude 网关。",
  });
  assert.equal(claudeActionGate(gateInput()).undo, false, "a client that was never applied has nothing to restore");
  assert.equal(
    claudeActionGate(gateInput({
      takeover: {
        desktop: { ...takeover.desktop, backup_path: "/tmp/desktop.bak", restorable: false },
      },
    })).undo,
    false,
    "a leftover backup file is not proof the receipt still belongs to this installation",
  );
});

test("the Desktop model draft starts with the default model and keeps the first item on top", async () => {
  const { claudeDesktopModelDraft, moveClaudeModel } = await loadClaude();

  assert.deepEqual(claudeDesktopModelDraft(settings), ["aaaa1111", "bbbb2222"]);
  assert.deepEqual(moveClaudeModel(["a", "b", "c"], 0, 1), ["b", "a", "c"]);
  assert.deepEqual(moveClaudeModel(["a", "b", "c"], 2, 1), ["a", "b", "c"], "out of bounds is a no-op");
});

test("model options keep library names and surface selections that left the library", async () => {
  const { claudeModelOptions, claudeModelLabel, claudeAliasOptions } = await loadClaude();
  const models = [libraryModel("aaaa1111", "Claude Sonnet")];

  assert.deepEqual(claudeModelOptions(models, ["aaaa1111", "gone3333"]), [
    { value: "aaaa1111", label: "Claude Sonnet", group: "Example" },
    { value: "gone3333", label: "gone3333", group: "模型库里已不存在" },
  ]);
  assert.equal(claudeModelLabel(models, "gone3333"), "gone3333");
  assert.equal(claudeAliasOptions(models, [])[0].value, "");
});

test("the guided view lists the exact fields Claude Desktop asks for", async () => {
  const { ClaudeDesktopTab } = await loader.load("/src/features/claude/ClaudeDesktopTab.tsx");
  const { claudeActionGate } = await loadClaude();
  const guided = { ...takeover.desktop, guided: true };
  const markup = await renderWithTooltip(createElement(ClaudeDesktopTab, {
    draft: settings,
    status,
    takeover: guided,
    models: [libraryModel("aaaa1111", "Sonnet")],
    desktopModels: ["aaaa1111"],
    gate: claudeActionGate(gateInput({ takeover: { desktop: guided } })),
    busy: false,
    error: null,
    keptAfterUndo: [],
    onDesktopModelsChange: () => {},
    onCopy: () => {},
  }));

  assert.match(markup, /Configure Third-Party Inference/);
  assert.match(markup, /Gateway base URL/);
  assert.match(markup, /Static API key/);
  assert.match(markup, /Bearer/);
  assert.match(markup, /claude-aaaa1111/);
  assert.match(markup, />Off</);
  assert.match(markup, />On</);
  assert.match(markup, /复制网关地址（Gateway base URL）/, "each guided field can be copied");
  assert.doesNotMatch(markup, /选择 Claude Desktop 模型/, "the model editor is replaced by the guide");
});

test("a running Claude Desktop is called out and cannot be applied", async () => {
  const { ClaudeDesktopTab } = await loader.load("/src/features/claude/ClaudeDesktopTab.tsx");
  const { claudeActionGate } = await loadClaude();
  const running = { ...takeover.desktop, state: "running" };
  const gate = claudeActionGate(gateInput({ takeover: { desktop: running } }));
  const markup = await renderWithTooltip(createElement(ClaudeDesktopTab, {
    draft: settings,
    status,
    takeover: running,
    models: [libraryModel("aaaa1111", "Sonnet")],
    desktopModels: ["aaaa1111"],
    gate,
    busy: false,
    error: null,
    keptAfterUndo: [],
    onDesktopModelsChange: () => {},
    onCopy: () => {},
  }));

  assert.match(markup, /先完全退出 Claude Desktop/);
  assert.match(markup, /Claude Desktop 正在运行，请先完全退出。/);
  assert.equal(gate.apply, false);
  assert.doesNotMatch(markup, /应用接管<\/button>/, "takeover commands belong to the page header");
});

test("the real server takeover JSON parses without dropping Desktop conflicts or receipts", async () => {
  const backend = new FakeBackend()
    .route("GET", `${API_ROOT}/claude/takeover`, () => ({ body: serverTakeover }))
    .route("PUT", `${API_ROOT}/claude/takeover/desktop`, () => ({ body: serverTakeover.desktop }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");

  const parsed = await api.claudeTakeover();
  assert.deepEqual(parsed, serverTakeover);
  assert.equal(parsed.desktop.restorable, true);

  const undone = await api.undoClaudeDesktop();
  assert.deepEqual(undone, serverTakeover.desktop);
});

test("Claude settings, status, takeover and the Desktop apply call stay on the management HTTP boundary", async () => {
  const backend = new FakeBackend()
    .route("GET", `${API_ROOT}/claude/settings`, () => ({ body: settings }))
    .route("PUT", `${API_ROOT}/claude/settings`, () => ({ body: settings }))
    .route("GET", `${API_ROOT}/claude/status`, () => ({ body: status }))
    .route("GET", `${API_ROOT}/claude/takeover`, () => ({ body: takeover }))
    .route("PUT", `${API_ROOT}/claude/takeover/desktop`, ({ body }) => ({ body: { ...takeover.desktop, state: body.action === "undo" ? "ready" : "applied" } }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");

  assert.deepEqual(await api.claudeSettings(), settings);
  assert.deepEqual(await api.claudeStatus(), status);
  assert.deepEqual(await api.claudeTakeover(), takeover);
  await api.applyClaudeDesktop([{ model_hash: "aaaa1111" }, { model_hash: "bbbb2222" }]);
  await api.undoClaudeDesktop();

  const writes = backend.requestsFor("PUT", `${API_ROOT}/claude/takeover/desktop`);
  assert.deepEqual(writes.map((request) => request.body), [
    { action: "apply", models: [{ model_hash: "aaaa1111" }, { model_hash: "bbbb2222" }] },
    { action: "undo", models: [] },
  ]);
});

test("a token rotation reply without a token is a localized failure", async () => {
  let reply = { token: "" };
  const backend = new FakeBackend().route("POST", `${API_ROOT}/claude/token/rotate`, () => ({ body: reply }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");

  await assert.rejects(() => api.rotateClaudeToken(), /服务端没有返回新令牌/);

  reply = { token: "rotated-token" };
  assert.deepEqual(await api.rotateClaudeToken(), { token: "rotated-token" });

  // 旧服务端把轮换结果当纯字符串返回：不能把这一整段当成 token 用。
  backend.route("POST", `${API_ROOT}/claude/token/rotate`, () => ({ body: "rotated-token" }));
  await assert.rejects(() => api.rotateClaudeToken(), /服务端没有返回新令牌/);
});

test("the Desktop tab renders conflict messages and separates blocking items from hints", async () => {
  const { ClaudeDesktopTab } = await loader.load("/src/features/claude/ClaudeDesktopTab.tsx");
  const { claudeActionGate } = await loadClaude();
  const conflicting = {
    ...takeover.desktop,
    state: "conflict",
    restorable: true,
    conflicts: [
      {
        code: "profile_conflict",
        key: "profile",
        message: "the profile already has a different gateway key",
        blocking: true,
      },
      {
        code: "policy_hint",
        key: null,
        message: "a user policy also exists; the profile was not written",
        blocking: false,
      },
    ],
  };
  const markup = await renderWithTooltip(createElement(ClaudeDesktopTab, {
    draft: settings,
    status,
    takeover: conflicting,
    models: [libraryModel("aaaa1111", "Claude Sonnet")],
    desktopModels: ["aaaa1111"],
    gate: claudeActionGate(gateInput({ takeover: { desktop: conflicting } })),
    busy: false,
    error: null,
    keptAfterUndo: [],
    onDesktopModelsChange: () => {},
    onCopy: () => {},
  }));

  assert.match(markup, /冲突与阻断/);
  assert.match(markup, /the profile already has a different gateway key/, "the server message is not reworded");
  assert.match(markup, /data-code="profile_conflict"/, "blocking items keep their stable code");
  assert.match(markup, /提示/, "non-blocking entries get their own section");
  assert.match(markup, /data-code="policy_hint"/);
  assert.match(markup, /Claude Desktop 的配置存在冲突/, "the blocked reason remains visible next to the client details");
});

test("a stale Desktop shows the expiry pill and the re-apply notice above the conflict list", async () => {
  const { ClaudeDesktopTab } = await loader.load("/src/features/claude/ClaudeDesktopTab.tsx");
  const { claudeActionGate } = await loadClaude();
  const stale = {
    ...takeover.desktop,
    state: "stale",
    restorable: true,
    conflicts: [
      { code: "takeover_stale", key: null, message: "gateway settings changed", blocking: false },
    ],
  };
  const gate = claudeActionGate(gateInput({ takeover: { desktop: stale } }));
  assert.equal(gate.apply, true, "stale is re-appliable, not a conflict");

  const markup = await renderWithTooltip(createElement(ClaudeDesktopTab, {
    draft: settings,
    status,
    takeover: stale,
    models: [libraryModel("aaaa1111", "Claude Sonnet")],
    desktopModels: ["aaaa1111"],
    gate,
    busy: false,
    error: null,
    keptAfterUndo: [],
    onDesktopModelsChange: () => {},
    onCopy: () => {},
  }));
  assert.match(markup, /配置已过期/, "the tab carries the stale pill");
  assert.match(markup, /网关设置已变化，客户端仍指向旧地址\/令牌，请重新应用接管。/);
  assert.doesNotMatch(markup, /配置冲突/);
  assert.doesNotMatch(markup, /先完全退出 Claude Desktop/, "stale does not mean the client is running");
});

test("undo results list the entries the server kept instead of claiming a full restore", async () => {
  const { ClaudeDesktopTab } = await loader.load("/src/features/claude/ClaudeDesktopTab.tsx");
  const { claudeActionGate, claudeUndoKept } = await loadClaude();
  const kept = claudeUndoKept([
    {
      code: "undo_kept_profile",
      key: "profile",
      message: "kept the profile because it no longer matches the takeover value",
      blocking: false,
    },
    {
      code: "profile_conflict",
      key: "profile",
      message: "unrelated blocking conflict",
      blocking: true,
    },
  ]);
  assert.equal(kept.length, 1, "only the kept entries are reported as kept");

  const markup = await renderWithTooltip(createElement(ClaudeDesktopTab, {
    draft: settings,
    status,
    takeover: takeover.desktop,
    models: [libraryModel("aaaa1111", "Claude Sonnet")],
    desktopModels: ["aaaa1111"],
    gate: claudeActionGate(gateInput()),
    busy: false,
    error: null,
    keptAfterUndo: kept,
    onDesktopModelsChange: () => {},
    onCopy: () => {},
  }));

  assert.match(markup, /撤销保留了这些改动/);
  assert.match(markup, /kept the profile because it no longer matches/);
  assert.doesNotMatch(markup, /unrelated blocking conflict/);
});

test("legacy string conflicts are dropped instead of leaking into the page", async () => {
  const legacy = {
    ...takeover,
    desktop: { ...takeover.desktop, conflicts: ["managed settings: /etc/claude/managed-settings.json"] },
  };
  const backend = new FakeBackend().route("GET", `${API_ROOT}/claude/takeover`, () => ({ body: legacy }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");

  const parsed = await api.claudeTakeover();
  assert.deepEqual(parsed.desktop.conflicts, [], "a string is not a conflict object; the parser must not render it");
});

const renderWithTooltip = async (element) => {
  const { TooltipProvider } = await loader.load("/src/shared/ui/Tooltip.tsx");
  return renderToStaticMarkup(createElement(TooltipProvider, null, element));
};
