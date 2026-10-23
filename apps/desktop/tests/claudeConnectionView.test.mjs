// Claude 接入页的状态推导、接管门槛与预览生成。
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
  cli: {
    state: "ready",
    settings_path: "/home/u/.claude/settings.json",
    backup_path: "",
    restorable: false,
    conflicts: [],
    missing_models: [],
    managed_sources: [],
  },
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

test("the entry counts Claude only when a client is actually applied", async () => {
  const { claudeConnectionView } = await loadClaude();

  assert.equal(claudeConnectionView(null, null).state, "checking");
  assert.equal(claudeConnectionView(status, null).state, "checking", "takeover must be read before claiming a state");

  const applied = {
    cli: { ...takeover.cli, state: "applied", backup_path: "/tmp/backup.json", restorable: true },
    desktop: takeover.desktop,
  };
  assert.equal(claudeConnectionView(status, applied).state, "taken-over");
  assert.equal(claudeConnectionView(status, applied).tone, "ok");
  assert.equal(claudeConnectionView(status, applied).label, "已接管");

  const conflict = { cli: { ...takeover.cli, state: "conflict" }, desktop: takeover.desktop };
  assert.equal(claudeConnectionView(status, conflict).state, "conflict");
  assert.equal(claudeConnectionView(status, conflict).tone, "bad");

  assert.equal(claudeConnectionView({ ...status, enabled: false, listening: false }, takeover).state, "disabled");
  assert.equal(claudeConnectionView({ ...status, listening: false }, takeover).state, "unavailable");
  assert.equal(
    claudeConnectionView(status, { cli: takeover.cli, desktop: { ...takeover.desktop, state: "managed" } }).state,
    "managed",
  );
  assert.equal(claudeConnectionView(status, takeover).state, "pending-apply");
  assert.equal(
    claudeConnectionView(status, {
      cli: { ...takeover.cli, state: "missing" },
      desktop: { ...takeover.desktop, state: "missing" },
    }).state,
    "idle",
  );
});

test("a stale client is amber and outranks applied, but never hides a conflict", async () => {
  const { claudeConnectionView, claudeTakeoverStateLabel, claudeTakeoverStateTone } = await loadClaude();

  const staleCli = {
    cli: { ...takeover.cli, state: "stale" },
    desktop: { ...takeover.desktop, state: "applied" },
  };
  const stale = claudeConnectionView(status, staleCli);
  assert.equal(stale.state, "stale", "a stale client outranks an applied one");
  assert.equal(stale.tone, "warn", "stale is a warning, not a healthy green");
  assert.equal(stale.label, "配置已过期");
  assert.equal(claudeConnectionView(status, {
    cli: { ...takeover.cli, state: "stale" },
    desktop: { ...takeover.desktop, state: "conflict" },
  }).state, "conflict", "conflict still wins over stale");
  assert.equal(
    claudeConnectionView({ ...status, enabled: false, listening: false }, staleCli).state,
    "stale",
    "a disabled gateway does not turn a stale client back into 'disabled'",
  );

  // 网关关着或没监听时不能继续显示绿色「已接管」：那样会让用户以为请求正在走网关。
  const applied = { cli: { ...takeover.cli, state: "applied" }, desktop: takeover.desktop };
  assert.equal(claudeConnectionView({ ...status, enabled: false, listening: false }, applied).state, "disabled");
  assert.equal(claudeConnectionView({ ...status, listening: false }, applied).state, "unavailable");

  assert.equal(claudeTakeoverStateLabel("stale"), "配置已过期");
  assert.equal(claudeTakeoverStateTone("stale"), "warn");
});

test("stale allows apply and undo; it is not a conflict", async () => {
  const { claudeActionGate } = await loadClaude();

  const staleRestorable = {
    cli: { ...takeover.cli, state: "stale", restorable: true },
    desktop: { ...takeover.desktop, state: "stale", restorable: true },
  };
  const cli = claudeActionGate("cli", gateInput({ takeover: staleRestorable }));
  assert.deepEqual(cli, { apply: true, undo: true, applyBlockedReason: null, applyBlockedLink: null });

  const desktop = claudeActionGate("desktop", gateInput({ takeover: staleRestorable }));
  assert.equal(desktop.apply, true, "re-applying is the fix for a stale client");
  assert.equal(desktop.undo, true);

  // 过期不代表可以绕过网关：网关没监听时仍然拦下。
  assert.match(
    claudeActionGate("cli", gateInput({
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
  assert.equal(
    claudeTokenRotationGate({ ...takeover, cli: { ...takeover.cli, state: "applied" } }).allowed,
    true,
    "an applied CLI is synced by the server, so it does not block rotation",
  );

  for (const state of ["applied", "stale", "running"]) {
    const gate = claudeTokenRotationGate({
      ...takeover,
      desktop: { ...takeover.desktop, state },
    });
    assert.equal(gate.allowed, false, `desktop ${state} must disable rotation`);
    assert.match(gate.reason, /先撤销 Claude Desktop 接管（运行中先完全退出）再轮换令牌。/);
  }
});

test("apply stays disabled with a readable reason until the gateway and client are ready", async () => {
  const { claudeActionGate } = await loadClaude();

  assert.equal(claudeActionGate("cli", gateInput()).apply, true);
  assert.equal(claudeActionGate("desktop", gateInput()).apply, true);

  const disabled = claudeActionGate("cli", gateInput({ settings: { ...settings, enabled: false } }));
  assert.equal(disabled.apply, false);
  assert.match(disabled.applyBlockedReason, /先启用/);

  assert.match(
    claudeActionGate("cli", gateInput({ status: { ...status, listening: false } })).applyBlockedReason,
    /还没有监听/,
  );
  assert.match(
    claudeActionGate("cli", gateInput({ settings: { ...settings, default_model_hash: null } })).applyBlockedReason,
    /默认模型/,
  );
  assert.match(claudeActionGate("cli", gateInput({ dirty: true })).applyBlockedReason, /未保存/);
  assert.match(
    claudeActionGate("cli", gateInput({ takeover: { ...takeover, cli: { ...takeover.cli, state: "missing" } } })).applyBlockedReason,
    /没有找到 Claude Code/,
  );
  assert.match(
    claudeActionGate("cli", gateInput({ takeover: { ...takeover, cli: { ...takeover.cli, state: "managed" } } })).applyBlockedReason,
    /系统策略/,
  );
  assert.match(
    claudeActionGate("cli", gateInput({ takeover: { ...takeover, cli: { ...takeover.cli, state: "conflict" } } })).applyBlockedReason,
    /冲突/,
  );
});

test("a running or guided Claude Desktop is a blocker, not an apply button", async () => {
  const { claudeActionGate } = await loadClaude();

  const running = claudeActionGate("desktop", gateInput({
    takeover: { ...takeover, desktop: { ...takeover.desktop, state: "running" } },
  }));
  assert.equal(running.apply, false);
  assert.match(running.applyBlockedReason, /完全退出/);

  const guided = claudeActionGate("desktop", gateInput({
    takeover: { ...takeover, desktop: { ...takeover.desktop, guided: true } },
  }));
  assert.equal(guided.apply, false, "the guided path never writes the profile");
  assert.match(guided.applyBlockedReason, /手工填写/);

  assert.match(
    claudeActionGate("desktop", gateInput({ desktopModels: [] })).applyBlockedReason,
    /至少选择一个模型/,
  );
});

test("undo follows the server receipt, not the gateway switch or the backup file", async () => {
  const { claudeActionGate } = await loadClaude();

  const restorable = {
    // 备份文件在，但收据不属于当前目录：不能撤销。
    cli: { ...takeover.cli, state: "conflict", backup_path: "/tmp/claude.bak", restorable: true },
    desktop: { ...takeover.desktop, backup_path: "/tmp/desktop.bak", restorable: true },
  };
  const off = { ...settings, enabled: false };

  assert.deepEqual(claudeActionGate("cli", gateInput({ settings: off, takeover: restorable })), {
    apply: false,
    undo: true,
    applyBlockedReason: "先启用并保存 Claude 网关。",
    applyBlockedLink: null,
  });
  assert.equal(claudeActionGate("desktop", gateInput({ settings: off, takeover: restorable })).undo, true);
  assert.equal(claudeActionGate("cli", gateInput()).undo, false, "a client that was never applied has nothing to restore");
  assert.equal(
    claudeActionGate("cli", gateInput({
      takeover: {
        ...takeover,
        cli: { ...takeover.cli, backup_path: "/tmp/claude.bak", restorable: false },
      },
    })).undo,
    false,
    "a leftover backup file is not proof the receipt still belongs to this installation",
  );
});

test("the CLI preview names every env key the apply writes", async () => {
  const { claudeCliEnv, claudeCliPreview, claudeEnvText, claudeCliEnvKeys } = await loadClaude();
  const models = [libraryModel("aaaa1111", "Claude Sonnet"), libraryModel("bbbb2222", "Claude Opus")];

  const env = claudeCliEnv(status, settings, models);
  assert.deepEqual(env, {
    ANTHROPIC_BASE_URL: "http://127.0.0.1:43121",
    ANTHROPIC_AUTH_TOKEN: "haxsd-test-token",
    CLAUDE_CODE_ATTRIBUTION_HEADER: "0",
    ANTHROPIC_DEFAULT_SONNET_MODEL: "aaaa1111",
    ANTHROPIC_DEFAULT_SONNET_MODEL_NAME: "Claude Sonnet",
    ANTHROPIC_DEFAULT_OPUS_MODEL: "bbbb2222",
    ANTHROPIC_DEFAULT_OPUS_MODEL_NAME: "Claude Opus",
  }, "unset aliases are omitted instead of written as empty strings");

  const parsed = JSON.parse(claudeCliPreview(status, settings, models));
  assert.deepEqual(parsed, { env }, "the preview is the settings.json env object, read-only JSON");
  assert.match(claudeEnvText(status, settings, models), /^ANTHROPIC_BASE_URL=http:\/\/127\.0\.0\.1:43121$/m);
  assert.deepEqual([...claudeCliEnvKeys], [
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_ATTRIBUTION_HEADER",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
  ], "the undo list is the same nine keys the server manages");
  assert.equal(Object.keys(claudeCliEnv(status, settings, models)).length, 7);
  assert.deepEqual(claudeCliEnv(null, null), {}, "still reading must not invent a preview");
  assert.equal(
    claudeCliEnv(status, settings, models).ANTHROPIC_DEFAULT_SONNET_MODEL_NAME,
    "Claude Sonnet",
    "the display name comes from the model library, matching what the server writes",
  );
});

test("a model that left the library blocks apply with a link to the model library", async () => {
  const { claudeActionGate } = await loadClaude();

  const staleDefault = claudeActionGate("cli", gateInput({
    takeover: { ...takeover, cli: { ...takeover.cli, missing_models: ["aaaa1111"] } },
  }));
  assert.equal(staleDefault.apply, false);
  assert.match(staleDefault.applyBlockedReason, /默认模型已不在模型库里/);
  assert.equal(staleDefault.applyBlockedLink, "/models");

  const staleAlias = claudeActionGate("desktop", gateInput({
    takeover: { ...takeover, cli: { ...takeover.cli, missing_models: ["bbbb2222"] } },
  }));
  assert.equal(staleAlias.apply, false);
  assert.match(staleAlias.applyBlockedReason, /选中的模型已不在模型库里/);
  assert.equal(staleAlias.applyBlockedLink, "/models");
});

test("the Desktop model draft starts with the default model and keeps the first item on top", async () => {
  const { claudeDesktopModelDraft, moveClaudeModel } = await loadClaude();

  assert.deepEqual(claudeDesktopModelDraft(settings), ["aaaa1111", "bbbb2222"]);
  assert.deepEqual(claudeDesktopModelDraft({ ...settings, sonnet_model_hash: "aaaa1111" }), ["aaaa1111", "bbbb2222"]);
  assert.deepEqual(claudeDesktopModelDraft(null), []);

  assert.deepEqual(moveClaudeModel(["a", "b", "c"], 2, -1), ["a", "c", "b"]);
  assert.deepEqual(moveClaudeModel(["a", "b", "c"], 0, 1), ["b", "a", "c"]);
  assert.deepEqual(moveClaudeModel(["a", "b"], 0, -1), ["a", "b"], "moving above the first item is a no-op");
  assert.deepEqual(moveClaudeModel(["a", "b"], 1, 1), ["a", "b"]);
});

test("the guided view lists the exact fields Claude Desktop asks for", async () => {
  const { claudeDesktopGuideItems, claudeDesktopGuided } = await loadClaude();

  assert.equal(claudeDesktopGuided(takeover.desktop), false);
  assert.equal(claudeDesktopGuided({ ...takeover.desktop, guided: true }), true);
  assert.equal(claudeDesktopGuided({ ...takeover.desktop, state: "managed" }), true);

  const items = claudeDesktopGuideItems({
    baseUrl: "http://127.0.0.1:43121",
    token: "haxsd-test-token",
    models: ["aaaa1111", "bbbb2222"],
    modelNames: (hash) => hash === "aaaa1111" ? "Sonnet" : "Opus",
  });
  const byKey = Object.fromEntries(items.map((item) => [item.key, item.value]));

  assert.deepEqual(Object.keys(byKey), ["base_url", "api_key", "auth", "models", "discovery", "allow_code"]);
  assert.equal(byKey.base_url, "http://127.0.0.1:43121");
  assert.equal(byKey.api_key, "haxsd-test-token");
  assert.equal(byKey.auth, "Bearer");
  assert.equal(byKey.models, "claude-aaaa1111（Sonnet）\nclaude-bbbb2222（Opus）");
  assert.equal(byKey.discovery, "Off");
  assert.equal(byKey.allow_code, "On");
  assert.ok(items.every((item) => item.hint.length > 0), "every field explains what to choose");
});

test("model options keep library names and surface selections that left the library", async () => {
  const { claudeAliasOptions, claudeModelOptions, claudeModelLabel } = await loadClaude();
  const models = [libraryModel("aaaa1111", "Sonnet"), libraryModel("bbbb2222", "Opus")];

  const options = claudeModelOptions(models, ["aaaa1111", "gone3333"]);
  assert.deepEqual(options.map((option) => [option.value, option.label]), [
    ["aaaa1111", "Sonnet"],
    ["bbbb2222", "Opus"],
    ["gone3333", "gone3333"],
  ]);
  assert.equal(options.at(-1).group, "模型库里已不存在");

  const aliases = claudeAliasOptions(models, ["aaaa1111"]);
  assert.deepEqual(aliases[0], { value: "", label: "使用默认模型", group: "回落" });
  assert.equal(claudeModelLabel(models, "bbbb2222"), "Opus");
  assert.equal(claudeModelLabel(models, "gone3333"), "gone3333");
});

test("Claude settings, status, takeover and both apply calls stay on the management HTTP boundary", async () => {
  const appliedCli = { ...takeover.cli, state: "applied", restorable: true };
  const appliedDesktop = { ...takeover.desktop, state: "applied", restorable: true };
  const backend = new FakeBackend()
    .route("GET", `${API_ROOT}/claude/settings`, () => ({ body: settings }))
    .route("PUT", `${API_ROOT}/claude/settings`, ({ body }) => ({ body }))
    .route("GET", `${API_ROOT}/claude/status`, () => ({ body: status }))
    .route("GET", `${API_ROOT}/claude/takeover`, () => ({ body: takeover }))
    .route("POST", `${API_ROOT}/claude/token/rotate`, () => ({ body: { token: "rotated-token", cli_synced: true } }))
    .route("PUT", `${API_ROOT}/claude/takeover/cli`, ({ body }) => ({
      body: body.action === "apply" ? appliedCli : { ...takeover.cli, state: "ready" },
    }))
    .route("PUT", `${API_ROOT}/claude/takeover/desktop`, ({ body }) => ({
      body: body.action === "apply" ? appliedDesktop : takeover.desktop,
    }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");

  assert.deepEqual(await api.claudeSettings(), settings);
  assert.deepEqual(await api.setClaudeSettings(settings), settings);
  assert.equal((await api.claudeStatus()).models, 3);
  assert.equal((await api.claudeTakeover()).desktop.config_source, "local");
  assert.deepEqual(
    await api.rotateClaudeToken(),
    { token: "rotated-token", cliSynced: true },
    "rotation returns both the new token and whether the server synced the CLI",
  );

  const cliApplied = await api.applyClaudeCli({ sonnet_model_hash: "aaaa1111" });
  assert.equal(cliApplied.state, "applied", "apply returns the server status");
  const cliUndone = await api.undoClaudeCli();
  assert.equal(cliUndone.state, "ready");
  assert.equal(cliUndone.restorable, false, "undo returns the server status instead of undefined");
  await api.applyClaudeDesktop([{ model_hash: "aaaa1111" }, { model_hash: "bbbb2222" }]);
  await api.undoClaudeDesktop();

  const cliWrites = backend.requestsFor("PUT", `${API_ROOT}/claude/takeover/cli`);
  assert.deepEqual(cliWrites.map((request) => request.body), [
    { action: "apply", sonnet_model_hash: "aaaa1111" },
    { action: "undo" },
  ]);
  const desktopWrites = backend.requestsFor("PUT", `${API_ROOT}/claude/takeover/desktop`);
  assert.deepEqual(desktopWrites.map((request) => request.body), [
    { action: "apply", models: [{ model_hash: "aaaa1111" }, { model_hash: "bbbb2222" }] },
    { action: "undo", models: [] },
  ]);
  assert.equal(backend.requestsFor("PUT", `${API_ROOT}/claude/settings`).length, 1);
});

test("a status response without the gateway fields is a read failure, not a fake disabled gateway", async () => {
  const backend = new FakeBackend().route("GET", `${API_ROOT}/claude/status`, () => ({
    body: { enabled: true, port: 43_121 },
  }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");

  await assert.rejects(() => api.claudeStatus(), /读取失败/);
});

test("a token rotation reply without a token or a sync flag is a localized failure", async () => {
  let reply = { token: "", cli_synced: true };
  const backend = new FakeBackend().route("POST", `${API_ROOT}/claude/token/rotate`, () => ({ body: reply }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");

  await assert.rejects(() => api.rotateClaudeToken(), /服务端没有返回新令牌/);

  reply = { token: "rotated-token" };
  await assert.rejects(() => api.rotateClaudeToken(), /服务端没有返回令牌同步结果/);

  // 旧服务端把轮换结果当纯字符串返回：不能把这一整段当成 token 用。
  backend.route("POST", `${API_ROOT}/claude/token/rotate`, () => ({ body: "rotated-token" }));
  await assert.rejects(() => api.rotateClaudeToken(), /服务端没有返回新令牌/);
});

test("Claude configuration errors are shown as readable text", async () => {
  const { clientErrorText } = await loader.load("/src/features/harness/harnessErrorText.ts");

  assert.equal(
    clientErrorText(new Error("configuration error: select a default Claude model before enabling the gateway")),
    "先在网关里选择默认模型，再启用。",
  );
  assert.equal(clientErrorText(new Error("configuration error: Claude port must be non-zero")), "端口不能为 0。");
  assert.match(
    clientErrorText(new Error("configuration error: Claude gateway port 43121 is already in use by another program on this machine; close it or choose a different port for the gateway")),
    /已被本机其它程序占用/,
  );
  assert.match(
    clientErrorText(new Error("configuration error: Claude gateway port 3100 is already used by haxsd byok as the management service port; choose a different port for the gateway")),
    /管理服务端口/,
  );
});

const renderWithTooltip = async (element) => {
  const { TooltipProvider } = await loader.load("/src/shared/ui/Tooltip.tsx");
  return renderToStaticMarkup(createElement(TooltipProvider, null, element));
};

test("the CLI tab renders conflict messages and separates blocking items from hints", async () => {
  const { ClaudeCliTab } = await loader.load("/src/features/claude/ClaudeCliTab.tsx");
  const { claudeActionGate } = await loadClaude();
  const conflicting = {
    ...takeover.cli,
    state: "conflict",
    restorable: true,
    conflicts: [
      {
        code: "env_base_url",
        key: "ANTHROPIC_BASE_URL",
        message: "env.ANTHROPIC_BASE_URL is already set to https://api.example.com; applying would replace it",
        blocking: true,
      },
      {
        code: "api_key_helper",
        key: "apiKeyHelper",
        message: "apiKeyHelper is configured",
        blocking: true,
      },
      {
        code: "shell_env",
        key: "ANTHROPIC_AUTH_TOKEN",
        message: "ANTHROPIC_AUTH_TOKEN is also set in the user environment",
        blocking: false,
      },
    ],
  };
  const markup = await renderWithTooltip(createElement(ClaudeCliTab, {
    saved: settings,
    draft: settings,
    status,
    takeover: conflicting,
    models: [libraryModel("aaaa1111", "Claude Sonnet")],
    gate: claudeActionGate("cli", gateInput({ takeover: { cli: conflicting, desktop: takeover.desktop } })),
    busy: false,
    error: null,
    keptAfterUndo: [],
    onChange: () => {},
    onApply: () => {},
    onUndo: () => {},
    onCopy: () => {},
    onOpenModels: () => {},
  }));

  assert.match(markup, /冲突与阻断/);
  assert.match(markup, /env\.ANTHROPIC_BASE_URL is already set to https:\/\/api\.example\.com/, "the server message is not reworded");
  assert.match(markup, /apiKeyHelper is configured/);
  assert.match(markup, /data-code="env_base_url"/, "blocking items keep their stable code");
  assert.match(markup, /提示/, "non-blocking entries get their own section");
  assert.match(markup, /data-code="shell_env"/);
  assert.match(markup, /ANTHROPIC_DEFAULT_SONNET_MODEL_NAME/, "the settings.json preview includes display-name keys");
  assert.match(markup, /CLAUDE_CODE_ATTRIBUTION_HEADER/);
  assert.match(markup, /应用接管<\/button>/, "the apply button stays rendered");
  assert.match(markup, /disabled/, "apply is disabled while the client conflicts");
});

test("a stale tab shows the expiry pill and the re-apply notice above the conflict list", async () => {
  const { ClaudeCliTab } = await loader.load("/src/features/claude/ClaudeCliTab.tsx");
  const { ClaudeDesktopTab } = await loader.load("/src/features/claude/ClaudeDesktopTab.tsx");
  const { claudeActionGate } = await loadClaude();
  const staleCli = {
    ...takeover.cli,
    state: "stale",
    restorable: true,
    conflicts: [
      { code: "takeover_stale", key: null, message: "gateway settings changed", blocking: false },
    ],
  };
  const cliGate = claudeActionGate("cli", gateInput({ takeover: { ...takeover, cli: staleCli } }));
  assert.equal(cliGate.apply, true, "stale is re-appliable, not a conflict");

  const cliMarkup = await renderWithTooltip(createElement(ClaudeCliTab, {
    saved: settings,
    draft: settings,
    status,
    takeover: staleCli,
    models: [libraryModel("aaaa1111", "Claude Sonnet")],
    gate: cliGate,
    busy: false,
    error: null,
    keptAfterUndo: [],
    onChange: () => {},
    onApply: () => {},
    onUndo: () => {},
    onCopy: () => {},
    onOpenModels: () => {},
  }));
  assert.match(cliMarkup, /配置已过期/, "the tab carries the stale pill");
  assert.match(cliMarkup, /网关设置已变化，客户端仍指向旧地址\/令牌，请重新应用接管。/);
  assert.doesNotMatch(cliMarkup, /配置冲突/);
  assert.ok(
    cliMarkup.indexOf('aria-label="配置已过期"') < cliMarkup.indexOf('aria-label="提示"'),
    "the re-apply notice sits above the server hints",
  );

  const staleDesktop = { ...takeover.desktop, state: "stale" };
  const desktopMarkup = await renderWithTooltip(createElement(ClaudeDesktopTab, {
    draft: settings,
    status,
    takeover: staleDesktop,
    models: [libraryModel("aaaa1111", "Claude Sonnet")],
    desktopModels: ["aaaa1111"],
    gate: claudeActionGate("desktop", gateInput({ takeover: { ...takeover, desktop: staleDesktop } })),
    busy: false,
    error: null,
    keptAfterUndo: [],
    onDesktopModelsChange: () => {},
    onApply: () => {},
    onUndo: () => {},
    onCopy: () => {},
  }));
  assert.match(desktopMarkup, /配置已过期/);
  assert.match(desktopMarkup, /网关设置已变化，客户端仍指向旧地址\/令牌，请重新应用接管。/);
  assert.doesNotMatch(desktopMarkup, /先完全退出 Claude Desktop/, "stale does not mean the client is running");
});

test("undo results list the entries the server kept instead of claiming a full restore", async () => {
  const { ClaudeCliTab } = await loader.load("/src/features/claude/ClaudeCliTab.tsx");
  const { claudeActionGate, claudeUndoKept } = await loadClaude();
  const kept = claudeUndoKept([
    {
      code: "user_modified",
      key: "ANTHROPIC_DEFAULT_SONNET_MODEL",
      message: "kept env.ANTHROPIC_DEFAULT_SONNET_MODEL because it no longer matches the takeover value",
      blocking: false,
    },
    {
      code: "env_base_url",
      key: "ANTHROPIC_BASE_URL",
      message: "unrelated blocking conflict",
      blocking: true,
    },
  ]);
  assert.equal(kept.length, 1, "only the kept entries are reported as kept");

  const markup = await renderWithTooltip(createElement(ClaudeCliTab, {
    saved: settings,
    draft: settings,
    status,
    takeover: takeover.cli,
    models: [libraryModel("aaaa1111", "Claude Sonnet")],
    gate: claudeActionGate("cli", gateInput()),
    busy: false,
    error: null,
    keptAfterUndo: kept,
    onChange: () => {},
    onApply: () => {},
    onUndo: () => {},
    onCopy: () => {},
    onOpenModels: () => {},
  }));

  assert.match(markup, /撤销保留了这些改动/);
  assert.match(markup, /kept env\.ANTHROPIC_DEFAULT_SONNET_MODEL/);
  assert.doesNotMatch(markup, /unrelated blocking conflict/);
});

test("the guided Desktop view lists every field and hides the write button", async () => {
  const { ClaudeDesktopTab } = await loader.load("/src/features/claude/ClaudeDesktopTab.tsx");
  const { claudeActionGate } = await loadClaude();
  const guided = { ...takeover.desktop, guided: true };
  const markup = await renderWithTooltip(createElement(ClaudeDesktopTab, {
    draft: settings,
    status,
    takeover: guided,
    models: [libraryModel("aaaa1111", "Sonnet")],
    desktopModels: ["aaaa1111"],
    gate: claudeActionGate("desktop", gateInput({ takeover: { cli: takeover.cli, desktop: guided } })),
    busy: false,
    error: null,
    keptAfterUndo: [],
    onDesktopModelsChange: () => {},
    onApply: () => {},
    onUndo: () => {},
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
  assert.doesNotMatch(markup, /应用接管<\/button>/, "the guided path never shows a write button");
  assert.doesNotMatch(markup, /选择 Claude Desktop 模型/, "the model editor is replaced by the guide");
});

test("a running Claude Desktop is called out and cannot be applied", async () => {
  const { ClaudeDesktopTab } = await loader.load("/src/features/claude/ClaudeDesktopTab.tsx");
  const { claudeActionGate } = await loadClaude();
  const running = { ...takeover.desktop, state: "running" };
  const gate = claudeActionGate("desktop", gateInput({ takeover: { cli: takeover.cli, desktop: running } }));
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
    onApply: () => {},
    onUndo: () => {},
    onCopy: () => {},
  }));

  assert.match(markup, /先完全退出 Claude Desktop/);
  assert.match(markup, /Claude Desktop 正在运行，请先完全退出。/);
  assert.equal(gate.apply, false);
  assert.match(markup, /应用接管<\/button>/);
  assert.match(markup, /disabled/);
});

test("the real server takeover JSON parses without dropping conflicts, receipts or missing models", async () => {
  const backend = new FakeBackend()
    .route("GET", `${API_ROOT}/claude/takeover`, () => ({ body: serverTakeover }))
    .route("PUT", `${API_ROOT}/claude/takeover/cli`, () => ({ body: serverTakeover.cli }))
    .route("PUT", `${API_ROOT}/claude/takeover/desktop`, () => ({ body: serverTakeover.desktop }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");

  const parsed = await api.claudeTakeover();
  // 服务端的每个字段都要原样落地：冲突不是字符串，收据与缺失模型也不能丢。
  assert.deepEqual(parsed, serverTakeover);
  assert.deepEqual(parsed.cli.conflicts.map((conflict) => [conflict.code, conflict.blocking]), [
    ["env_base_url", true],
    ["user_modified", true],
    ["shell_env", false],
  ]);
  assert.equal(parsed.cli.restorable, true);
  assert.deepEqual(parsed.cli.missing_models, ["gone3333"]);
  assert.deepEqual(parsed.cli.managed_sources, [
    "C:\\Program Files\\ClaudeCode\\managed-settings.d\\10-company.json",
  ]);
  assert.equal(parsed.desktop.restorable, true);

  // 撤销/应用的回包是同一个状态结构，解析器必须给出同样的对象。
  const undone = await api.undoClaudeCli();
  assert.deepEqual(undone, serverTakeover.cli);
  const desktop = await api.undoClaudeDesktop();
  assert.deepEqual(desktop, serverTakeover.desktop);
});

test("legacy string conflicts are dropped instead of leaking into the page", async () => {
  const legacy = {
    ...takeover,
    cli: { ...takeover.cli, conflicts: ["managed settings: /etc/claude-code/managed-settings.json"] },
  };
  const backend = new FakeBackend().route("GET", `${API_ROOT}/claude/takeover`, () => ({ body: legacy }));
  backend.install();
  const { api } = await loader.load("/src/shared/api.ts");

  const parsed = await api.claudeTakeover();
  assert.deepEqual(parsed.cli.conflicts, [], "a string is not a conflict object; the parser must not render it");
});
