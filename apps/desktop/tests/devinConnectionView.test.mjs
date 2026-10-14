// Behavior tests for the Devin connection status derivation.
//
// The page keeps an editable draft and the last saved settings side by side. These
// tests pin which of the two the status claims come from: the saved configuration
// and the server-reported listening state, never the unsaved draft.

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

async function loadView() {
  loader.invalidateAll();
  const module = await loader.load("/src/features/devin/devinConnectionView.ts");
  return module.devinConnectionView;
}

function devinSettings(overrides = {}) {
  return {
    enabled: false,
    auth_token: "",
    api_port: 43_110,
    inference_port: 43_111,
    local_api_port: 43_112,
    upstream_api_url: "https://server.self-serve.windsurf.com",
    bindings: [],
    ...overrides,
  };
}

function binding(overrides = {}) {
  return {
    model_uid: "swe-1-6-slow",
    model_hash: "hash-1",
    display_name: "Example",
    context_window_tokens: 128_000,
    enabled: true,
    kind: "standard",
    routes: [],
    active_route_id: null,
    ...overrides,
  };
}

function model() {
  return {
    model_hash: "hash-1",
    sort_order: 1,
    display_name: "Example",
    group_name: null,
    type: "openai",
    base_url: "https://api.example.com",
    use_full_url: false,
    api_key_configured: true,
    tooltip_data: "",
    model_id: "example-model",
    reasoning_effort: null,
    openai_endpoint: "/v1/chat/completions",
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
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
  };
}

function hostStatus(overrides = {}) {
  return {
    path: "C:\\Devin\\extension.js",
    compatible: true,
    clean: true,
    patched: false,
    parts: { api: false, restart: false, inference: false, local_api: false },
    ports: null,
    backup_path: "",
    backup_available: false,
    current_sha256: "current",
    backup_sha256: null,
    restore_receipt: null,
    message: "",
    ...overrides,
  };
}

function viewInput(overrides = {}) {
  return {
    draft: devinSettings(),
    saved: devinSettings(),
    models: [],
    hostStatus: hostStatus(),
    gatewayListening: false,
    devinCalls: 0,
    ...overrides,
  };
}

test("a saved-off gateway that is still listening is reported as such, not as disabled", async () => {
  const devinConnectionView = await loadView();
  // 已保存关闭，但上一轮运行里起来的网关任务还没随进程退出：端口仍然在应答请求。
  const saved = devinSettings({ enabled: false });

  const view = devinConnectionView(viewInput({ draft: saved, saved, gatewayListening: true }));

  assert.equal(view.gateway.ready, false, "a closed gateway is never ready");
  assert.equal(view.gateway.pendingRestart, false, "nothing waits for a restart; the ports are already open");
  assert.equal(view.gateway.tone, "warn", "ports are live while the setting says off — that is a warning, not idle");
  assert.equal(view.gateway.listening, true);
  assert.match(view.gateway.label, /监听/, "the label must say the ports are still listening");
  assert.equal(view.summary.tone, "warn");
  assert.equal(view.hero.connected, false);
});

test("an unsaved switch does not claim the gateway is saved or running", async () => {
  const devinConnectionView = await loadView();
  const saved = devinSettings({ enabled: false });
  const draft = devinSettings({ enabled: true });

  const view = devinConnectionView(viewInput({ draft, saved, gatewayListening: false }));

  assert.equal(view.pendingChanges, true, "the draft differs from what is stored");
  assert.equal(view.gateway.tone, "idle", "a draft switch must not make the gateway look enabled");
  assert.equal(view.gateway.ready, false);
  assert.equal(view.gateway.pendingRestart, false, "nothing was saved, so nothing waits for a restart");
  assert.equal(view.summary.tone, "idle");
  assert.deepEqual(
    view.hero.steps.map((step) => step.key),
    ["enable", "bind", "connect", "verify"],
    "the enable step is still outstanding",
  );
  assert.equal(view.hero.connected, false);
});

test("a saved gateway whose ports are not listening yet waits for a restart", async () => {
  const devinConnectionView = await loadView();
  const saved = devinSettings({ enabled: true });

  const view = devinConnectionView(viewInput({ draft: saved, saved, gatewayListening: false }));

  assert.equal(view.gateway.pendingRestart, true);
  assert.equal(view.gateway.ready, false);
  assert.equal(view.gateway.tone, "warn");
  assert.equal(view.summary.tone, "warn");
  assert.ok(view.hero.steps.some((step) => step.key === "enable"), "the gateway is not running yet");
});

test("an unknown listening state is reported as checking, not as a pending restart", async () => {
  const devinConnectionView = await loadView();
  const saved = devinSettings({ enabled: true });

  const view = devinConnectionView(viewInput({ draft: saved, saved, gatewayListening: null }));

  assert.equal(view.gateway.pendingRestart, false, "nothing is known about the ports yet");
  assert.equal(view.gateway.ready, false);
  assert.equal(view.gateway.tone, "warn");
  assert.ok(view.hero.steps.some((step) => step.key === "enable"));
});

test("a running gateway pointed at by the host reports the whole path as connected", async () => {
  const devinConnectionView = await loadView();
  const saved = devinSettings({ enabled: true, bindings: [binding()] });

  const view = devinConnectionView(viewInput({
    draft: saved,
    saved,
    models: [model()],
    hostStatus: hostStatus({
      clean: false,
      patched: true,
      parts: { api: true, restart: true, inference: true, local_api: true },
      ports: { api_port: saved.api_port, inference_port: saved.inference_port, local_api_port: saved.local_api_port },
    }),
    gatewayListening: true,
    devinCalls: 3,
  }));

  assert.equal(view.gateway.ready, true);
  assert.equal(view.gateway.tone, "ok");
  assert.equal(view.summary.tone, "ok");
  assert.equal(view.hero.connected, true);
  assert.deepEqual(view.hero.steps, [], "nothing is left to do");
  assert.deepEqual(view.hero.stages.map((stage) => stage.state), ["up", "up", "up"]);
  assert.equal(view.pendingChanges, false);
});

test("the status panel describes the saved ports, not the edited ones", async () => {
  const devinConnectionView = await loadView();
  const saved = devinSettings({ enabled: true, bindings: [binding()] });
  const draft = devinSettings({ ...saved, api_port: 9991, inference_port: 9992, local_api_port: 9993 });

  const view = devinConnectionView(viewInput({
    draft,
    saved,
    models: [model()],
    hostStatus: hostStatus({
      clean: false,
      patched: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port, local_api_port: saved.local_api_port },
    }),
    gatewayListening: true,
    devinCalls: 1,
  }));

  const gatewayStage = view.hero.stages.find((stage) => stage.key === "gateway");
  assert.ok(gatewayStage.detail.includes(String(saved.api_port)));
  assert.ok(!gatewayStage.detail.includes(String(draft.api_port)), "the draft port is not what the gateway listens on");
  assert.equal(view.pendingChanges, true);
});

test("an unsaved binding does not count as a bound model", async () => {
  const devinConnectionView = await loadView();
  const saved = devinSettings({ enabled: true, bindings: [] });
  const draft = devinSettings({ enabled: true, bindings: [binding()] });

  const view = devinConnectionView(viewInput({
    draft,
    saved,
    models: [model()],
    hostStatus: hostStatus({
      clean: false,
      patched: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port, local_api_port: saved.local_api_port },
    }),
    gatewayListening: true,
    devinCalls: 1,
  }));

  assert.equal(view.pendingChanges, true);
  assert.ok(view.hero.steps.some((step) => step.key === "bind"), "the added mapping is not stored yet");
  assert.equal(view.hero.stages.find((stage) => stage.key === "model").state, "unknown");
});
