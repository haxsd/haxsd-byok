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
    default_model_uid: null,
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

function enabledWithBinding(overrides = {}) {
  return devinSettings({
    enabled: true,
    bindings: [binding(overrides)],
    default_model_uid: "swe-1-6-slow",
  });
}

function model(overrides = {}) {
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
    ...overrides,
  };
}

function hostStatus(overrides = {}) {
  return {
    path: "C:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json",
    compatible: true,
    unconfigured: true,
    configured: false,
    parts: { api: false, inference: false },
    ports: null,
    backup_path: "",
    backup_available: false,
    current_sha256: "current",
    backup_sha256: null,
    restore_receipt: null,
    extension: extensionStatus(),
    message: "",
    ...overrides,
  };
}

function extensionStatus(overrides = {}) {
  return {
    path: "D:\\devin\\Devin\\resources\\app\\extensions\\windsurf\\dist\\extension.js",
    version: "3.11.216",
    supported_sha256: "A063DAE82C91328E17AE16121E90780087674B65839B5AD39915C9C59C652FF0",
    compatible: true,
    clean: true,
    patched: false,
    backup_path: "",
    backup_available: false,
    current_sha256: "extension-clean",
    backup_sha256: null,
    restore_receipt: null,
    message: "",
    ...overrides,
  };
}

function gateway(overrides = {}) {
  return {
    listening: false,
    ready: false,
    ...overrides,
  };
}

function viewInput(overrides = {}) {
  return {
    draft: devinSettings(),
    saved: devinSettings(),
    models: [],
    hostStatus: hostStatus(),
    gateway: gateway(),
    devinCalls: 0,
    lastCallSuccess: null,
    ...overrides,
  };
}

test("a saved-off gateway that is still listening is reported as such, not as disabled", async () => {
  const devinConnectionView = await loadView();
  // 已保存关闭，但上一轮运行里起来的网关任务还没随进程退出：端口仍然在应答请求。
  const saved = devinSettings({ enabled: false });

  const view = devinConnectionView(viewInput({ draft: saved, saved, gateway: gateway({ listening: true }) }));

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

  const view = devinConnectionView(viewInput({ draft, saved }));

  assert.equal(view.pendingChanges, true, "the draft differs from what is stored");
  assert.equal(view.gateway.tone, "idle", "a draft switch must not make the gateway look enabled");
  assert.equal(view.gateway.ready, false);
  assert.equal(view.gateway.pendingRestart, false, "nothing was saved, so nothing waits for a restart");
  assert.equal(view.summary.tone, "idle");
  assert.deepEqual(
    view.hero.steps.map((step) => step.key),
    ["model", "connect", "verify"],
    "the model choice is still outstanding",
  );
  assert.equal(view.hero.connected, false);
});

test("a saved gateway whose ports are not listening yet waits for a restart", async () => {
  const devinConnectionView = await loadView();
  const saved = devinSettings({ enabled: true });

  const view = devinConnectionView(viewInput({ draft: saved, saved }));

  assert.equal(view.gateway.pendingRestart, true);
  assert.equal(view.gateway.ready, false);
  assert.equal(view.gateway.tone, "warn");
  assert.equal(view.summary.tone, "warn");
  assert.ok(view.hero.steps.some((step) => step.key === "model"), "the gateway is not ready from the user's point of view");
});

test("an unknown listening state is reported as checking, not as a pending restart", async () => {
  const devinConnectionView = await loadView();
  const saved = devinSettings({ enabled: true });

  const view = devinConnectionView(viewInput({ draft: saved, saved, gateway: null }));

  assert.equal(view.gateway.pendingRestart, false, "nothing is known about the ports yet");
  assert.equal(view.gateway.ready, false);
  assert.equal(view.gateway.tone, "warn");
  assert.ok(view.hero.steps.some((step) => step.key === "model"));
});

test("a running gateway pointed at by the host reports the whole path as connected", async () => {
  const devinConnectionView = await loadView();
  const saved = enabledWithBinding();

  const view = devinConnectionView(viewInput({
    draft: saved,
    saved,
    models: [model()],
    hostStatus: hostStatus({
      unconfigured: false,
      configured: true,
      parts: { api: true, inference: true },
      ports: { api_port: saved.api_port, inference_port: saved.inference_port },
      extension: extensionStatus({ clean: false, patched: true }),
    }),
    gateway: gateway({ listening: true, ready: true }),
    devinCalls: 3,
    lastCallSuccess: true,
  }));

  assert.equal(view.gateway.ready, true);
  assert.equal(view.gateway.tone, "ok");
  assert.equal(view.summary.tone, "ok");
  assert.equal(view.hero.connected, true);
  assert.deepEqual(view.hero.steps, [], "nothing is left to do");
  assert.deepEqual(view.hero.stages.map((stage) => stage.state), ["up", "up", "up"]);
  assert.equal(view.pendingChanges, false);
});

test("settings alone do not claim the host is connected before the gate patch", async () => {
  const devinConnectionView = await loadView();
  const saved = enabledWithBinding();

  const view = devinConnectionView(viewInput({
    draft: saved,
    saved,
    models: [model()],
    hostStatus: hostStatus({
      unconfigured: false,
      configured: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port },
    }),
    gateway: gateway({ listening: true, ready: true }),
    devinCalls: 1,
    lastCallSuccess: true,
  }));

  assert.equal(view.hero.connected, false);
  assert.match(
    view.hero.stages.find((stage) => stage.key === "devin").detail,
    /缺兼容补丁/,
    "写入设置但没打补丁时，Devin 仍会忽略端点",
  );
  assert.ok(view.hero.steps.some((step) => step.key === "connect"));
});

test("the status panel hides saved ports from the primary flow", async () => {
  const devinConnectionView = await loadView();
  const saved = enabledWithBinding();
  const draft = devinSettings({ ...saved, api_port: 9991, inference_port: 9992, local_api_port: 9993 });

  const view = devinConnectionView(viewInput({
    draft,
    saved,
    models: [model()],
    hostStatus: hostStatus({
      unconfigured: false,
      configured: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port },
    }),
    gateway: gateway({ listening: true, ready: true }),
    devinCalls: 1,
    lastCallSuccess: true,
  }));

  const gatewayStage = view.hero.stages.find((stage) => stage.key === "gateway");
  assert.equal(gatewayStage.detail, "已运行");
  assert.ok(!gatewayStage.detail.includes(String(draft.api_port)), "technical ports stay in advanced settings");
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
      unconfigured: false,
      configured: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port },
    }),
    gateway: gateway({ listening: true, ready: true }),
    devinCalls: 1,
    lastCallSuccess: true,
  }));

  assert.equal(view.pendingChanges, true);
  assert.ok(view.hero.steps.some((step) => step.key === "model"), "the added model is not stored yet");
  assert.equal(view.hero.stages.find((stage) => stage.key === "model").state, "unknown");
});

test("listening on the old ports waits for a restart instead of claiming readiness", async () => {
  const devinConnectionView = await loadView();
  const saved = enabledWithBinding();

  const view = devinConnectionView(viewInput({
    draft: saved,
    saved,
    models: [model()],
    hostStatus: hostStatus({
      unconfigured: false,
      configured: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port },
      extension: extensionStatus({ clean: false, patched: true }),
    }),
    gateway: gateway({ listening: true }),
    devinCalls: 2,
    lastCallSuccess: true,
  }));

  assert.equal(view.gateway.ready, false);
  assert.equal(view.gateway.pendingRestart, true);
  assert.equal(view.gateway.label, "端口已更改，重启后生效");
  assert.equal(view.hero.connected, false);
  assert.deepEqual(
    view.hero.steps.map((step) => step.key),
    ["gateway"],
    "the only thing left is rebinding the saved ports",
  );
  assert.equal(view.hero.steps[0].hint, "端口已更改，重启后生效");
  assert.equal(view.summary.label, "待完成");
});

test("a failed latest call is not counted as a verified connection", async () => {
  const devinConnectionView = await loadView();
  const saved = enabledWithBinding();

  const view = devinConnectionView(viewInput({
    draft: saved,
    saved,
    models: [model()],
    hostStatus: hostStatus({
      unconfigured: false,
      configured: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port },
      extension: extensionStatus({ clean: false, patched: true }),
    }),
    gateway: gateway({ listening: true, ready: true }),
    devinCalls: 4,
    lastCallSuccess: false,
  }));

  assert.equal(view.gateway.ready, true);
  assert.equal(view.hero.connected, false, "a historical success cannot cover a later failure");
  assert.deepEqual(view.hero.steps.map((step) => step.key), ["verify"]);
  assert.equal(view.hero.steps[0].hint, "最近一次调用未成功，请检查模型或重新测试");
  assert.equal(view.summary.label, "待完成");
});

test("a deleted model keeps the model step outstanding", async () => {
  const devinConnectionView = await loadView();
  const saved = enabledWithBinding({ model_hash: "hash-deleted" });

  const view = devinConnectionView(viewInput({
    draft: saved,
    saved,
    models: [model()],
    hostStatus: hostStatus({
      unconfigured: false,
      configured: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port },
      extension: extensionStatus({ clean: false, patched: true }),
    }),
    gateway: gateway({ listening: true, ready: true }),
    devinCalls: 1,
    lastCallSuccess: true,
  }));

  assert.deepEqual(view.hero.steps.map((step) => step.key), ["model"]);
  assert.equal(view.hero.steps[0].hint, "模型已删除或不可用");
  assert.equal(view.hero.stages.find((stage) => stage.key === "model").state, "down");
  assert.equal(view.hero.stages.find((stage) => stage.key === "model").detail, "模型已删除或哈希无效");
});

test("an unread gateway state says it is checking instead of claiming zero steps", async () => {
  const devinConnectionView = await loadView();
  const saved = enabledWithBinding();

  const view = devinConnectionView(viewInput({
    draft: saved,
    saved,
    models: [model()],
    hostStatus: hostStatus({
      unconfigured: false,
      configured: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port },
      extension: extensionStatus({ clean: false, patched: true }),
    }),
    gateway: null,
    devinCalls: 1,
    lastCallSuccess: true,
  }));

  assert.deepEqual(view.hero.steps, []);
  assert.equal(view.hero.connected, false);
  assert.equal(view.hero.title, "正在检查连接…");
  assert.equal(view.hero.description, "检查中…");
  assert.equal(view.summary.label, "检查中…");
});

test("multiple mappings without a default never fall back to the first one", async () => {
  const devinConnectionView = await loadView();
  const saved = devinSettings({
    enabled: true,
    bindings: [
      binding(),
      binding({ model_uid: "second-uid", model_hash: "hash-2", display_name: "Second" }),
    ],
    default_model_uid: null,
  });

  const view = devinConnectionView(viewInput({
    draft: saved,
    saved,
    models: [model(), model({ model_hash: "hash-2", display_name: "Second" })],
    hostStatus: hostStatus({
      unconfigured: false,
      configured: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port },
      extension: extensionStatus({ clean: false, patched: true }),
    }),
    gateway: gateway({ listening: true, ready: true }),
    devinCalls: 2,
    lastCallSuccess: true,
  }));

  assert.deepEqual(view.hero.steps.map((step) => step.key), ["model"]);
  assert.equal(view.hero.steps[0].hint, "选择默认模型后保存");
  assert.equal(view.hero.stages.find((stage) => stage.key === "model").detail, "未选择默认模型");
  assert.equal(view.hero.connected, false);
});

test("the explicit default selects the second mapping instead of the first", async () => {
  const devinConnectionView = await loadView();
  const saved = devinSettings({
    enabled: true,
    bindings: [
      binding(),
      binding({ model_uid: "second-uid", model_hash: "hash-2", display_name: "Second" }),
    ],
    default_model_uid: "second-uid",
  });

  const view = devinConnectionView(viewInput({
    draft: saved,
    saved,
    models: [model(), model({ model_hash: "hash-2", display_name: "Second" })],
    hostStatus: hostStatus({
      unconfigured: false,
      configured: true,
      ports: { api_port: saved.api_port, inference_port: saved.inference_port },
      extension: extensionStatus({ clean: false, patched: true }),
    }),
    gateway: gateway({ listening: true, ready: true }),
    devinCalls: 2,
    lastCallSuccess: true,
  }));

  assert.deepEqual(view.hero.steps, []);
  assert.equal(view.hero.connected, true);
  assert.equal(view.hero.stages.find((stage) => stage.key === "model").detail, "Second");
  assert.match(view.hero.description, /Second/);
});
