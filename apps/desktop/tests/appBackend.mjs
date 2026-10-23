// Minimal stand-in for the local management server, shaped like the real one.
//
// Responses mirror the complete DTOs (all fields the frontend reads), and the
// desktop settings PUT implements the merge contract the server side now keeps:
// the request carries only the changed fields, the response is the full object.

import { API_ROOT, FakeBackend } from "./harness.mjs";

export function emptyOverview() {
  return {
    metrics: {
      llm_calls: 0,
      successful_calls: 0,
      failed_calls: 0,
      token_usage: 0,
      prompt_tokens: 0,
      input_tokens: 0,
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      output_tokens: 0,
    },
    token_usage_granularity: "day",
    token_usage_series: [],
  };
}

export function defaultModelPricing() {
  const price = { input_per_million: 2, output_per_million: 8, cache_read_per_million: 0.04, cache_write_per_million: 0 };
  return { mode: "peak_off_peak", cny: { fixed: price, peak: price, off_peak: price }, usd: { fixed: price, peak: price, off_peak: price } };
}

export function defaultPricing() {
  return { models: { "hash-1": defaultModelPricing() } };
}

export function cursorHarnessStatus() {
  return {
    platform: "win32",
    ca: "ready",
    mode: "official",
    configured_models: 0,
    enabled_models: 0,
    integration: "disabled",
    settings_applied: false,
    foreign_configuration: false,
    proxy_url: null,
    ca_install_command: null,
    account: { state: "missing", email: null },
    catalog: { last_success_ms: null, last_failure_ms: null, last_failure: null },
  };
}

export function pluginRuntimeStatus() {
  return { state: "ready", version: "0.0.0", target: null, phase: null, downloaded_bytes: 0, total_bytes: null, error: null };
}

export function llmCallFixture() {
  return {
    call_kind: "provider_llm",
    client_source: "internal",
    route: "local_byok",
    call_id: "call-1",
    run_id: "run-1",
    conversation_id: "conversation-1",
    provider_call_index: 1,
    model_hash: "hash-1",
    provider_type: "openai",
    provider_url: "https://api.example.com",
    request_type: "chat",
    request_url: "https://api.example.com/v1/chat/completions",
    model_id: "example-model",
    display_name: "Example",
    reasoning_effort: null,
    fast: null,
    status: "succeeded",
    finish_reason: "stop",
    created_at_ms: 1_700_000_000_000,
    ttfb_ms: 12,
    ttft_ms: 20,
    ttfr_ms: null,
    duration_ms: 300,
    input_tokens: 10,
    output_tokens: 5,
    total_tokens: 15,
    cache_read_tokens: 0,
    cache_write_tokens: 0,
    reasoning_tokens: null,
    message_count: 1,
    tool_count: 0,
    http_status: 200,
    error_kind: null,
    error_message: null,
    detailed: false,
  };
}

/**
 * Install a fake management API and return `{ backend, state }`.
 *
 * `state` is the mutable server-side truth; tests mutate it to describe what a
 * later read returns, and read it to describe what the server actually kept.
 */
export function installAppBackend(overrides = {}) {
  const backend = new FakeBackend();
  const state = {
    models: [],
    calls: [],
    overview: emptyOverview(),
    detailed: false,
    ports: { proxy_port: 8090, service_port: 3100 },
    pricing: defaultPricing(),
    cursorHarness: cursorHarnessStatus(),
    devinStatus: { enabled: false, listening: false, calls: 0 },
    codexStatus: { enabled: false, listening: false, port: 43120, configured: false, config_conflict: false, restorable: false, conflicts: [], home: "test-home", calls: 0, last_error: null },
    claudeStatus: { enabled: false, listening: false, port: 43121, base_url: "http://127.0.0.1:43121", token: "test-token", models: 0, default_model_hash: null, calls: 0, last_error: null },
    claudeTakeover: {
      cli: {
        state: "missing",
        settings_path: "",
        backup_path: "",
        restorable: false,
        conflicts: [],
        missing_models: [],
        managed_sources: [],
      },
      desktop: {
        state: "missing",
        config_source: "none",
        profile_path: "",
        backup_path: "",
        restorable: false,
        conflicts: [],
        guided: false,
      },
    },
    pluginRuntime: pluginRuntimeStatus(),
    plugins: [],
    desktop: { silent_start: false, show_dock_icon: true, developer_mode: false },
    ...overrides,
  };

  backend.route("GET", `${API_ROOT}/codex/status`, () => ({ body: state.codexStatus }));
  backend.route("GET", `${API_ROOT}/claude/status`, () => ({ body: state.claudeStatus }));
  backend.route("GET", `${API_ROOT}/claude/takeover`, () => ({ body: state.claudeTakeover }));
  backend.route("GET", `${API_ROOT}/models`, () => ({ body: state.models }));
  backend.route("GET", `${API_ROOT}/llm-calls`, () => ({ body: state.calls }));
  backend.route("GET", `${API_ROOT}/overview`, () => ({ body: state.overview }));
  backend.route("GET", `${API_ROOT}/settings/observability`, () => ({ body: { detailed: state.detailed } }));
  backend.route("PUT", `${API_ROOT}/settings/observability`, (request) => {
    state.detailed = request.body.detailed;
    return { body: { detailed: state.detailed } };
  });
  backend.route("GET", `${API_ROOT}/settings/ports`, () => ({ body: { ...state.ports } }));
  backend.route("PUT", `${API_ROOT}/settings/ports`, (request) => {
    state.ports = { ...request.body };
    return { body: { ...state.ports } };
  });
  backend.route("GET", `${API_ROOT}/settings/pricing`, () => ({ body: state.pricing }));
  for (const key of ["hash-1", "hash-2"]) {
    backend.route("PUT", `${API_ROOT}/settings/pricing/${key}`, request => {
      state.pricing = { models: { ...state.pricing.models, [key]: request.body } };
      return { body: state.pricing };
    });
    backend.route("DELETE", `${API_ROOT}/settings/pricing/${key}`, () => {
      const models = { ...state.pricing.models };
      delete models[key];
      state.pricing = { models };
      return { body: state.pricing };
    });
  }
  backend.route("GET", `${API_ROOT}/harness/cursor/status`, () => ({ body: state.cursorHarness }));
  backend.route("GET", `${API_ROOT}/devin/status`, () => ({ body: state.devinStatus }));
  backend.route("GET", `${API_ROOT}/plugins/runtime`, () => ({ body: state.pluginRuntime }));
  backend.route("GET", `${API_ROOT}/plugins`, () => ({ body: state.plugins }));
  backend.route("GET", `${API_ROOT}/settings/desktop`, () => ({ body: { ...state.desktop } }));
  // The real server merges the received fields into the stored settings and
  // answers with the full object; the frontend must never need to read first.
  backend.route("PUT", `${API_ROOT}/settings/desktop`, (request) => {
    state.desktop = { ...state.desktop, ...request.body };
    return { body: { ...state.desktop } };
  });
  backend.install();
  return { backend, state };
}
