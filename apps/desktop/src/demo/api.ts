import type {
  CallDetail,
  ClaudeSettings,
  ClaudeStatus,
  ClaudeTakeover,
  CodexSettings,
  CodexStatus,
  CompactionSettings,
  CursorHarnessStatus,
  CursorMode,
  DesktopSettings,
  DevinExtensionReceipt,
  DevinHostReceipt,
  DevinHostStatus,
  DevinSettings,
  LlmCall,
  Model,
  ModelTokenPricing,
  TokenPricingSettings,
  Overview,
  OverviewTokenUsageBucket,
  ProxySettings,
  StatisticsStorage,
  TabSettings,
} from "../shared/api";

const API_ROOT = "/__byok-api__/api";
const FIXED_NOW = Date.UTC(2026, 7, 27, 8, 0, 0);

const models: Model[] = [
  createModel({ hash: "mock-claude-sonnet", order: 1, name: "Claude Sonnet 4", type: "anthropic", url: "https://api.anthropic.com", modelId: "claude-sonnet-4-20250514" }),
  createModel({ hash: "mock-claude-opus", order: 2, name: "Claude Opus 4", type: "anthropic", url: "https://api.anthropic.com", modelId: "claude-opus-4-20250514" }),
  createModel({ hash: "mock-gpt", order: 3, name: "GPT-5.2", type: "openai", url: "https://api.openai.com", modelId: "gpt-5.2" }),
  createModel({ hash: "mock-o3", order: 4, name: "o3", type: "openai", url: "https://api.openai.com", modelId: "o3" }),
  createModel({ hash: "mock-deepseek-v3", order: 5, name: "DeepSeek V3.2", type: "openai", url: "https://api.deepseek.com", modelId: "deepseek-chat", endpoint: "/v1/chat/completions" }),
  createModel({ hash: "mock-deepseek-r1", order: 6, name: "DeepSeek R1", type: "openai", url: "https://api.deepseek.com", modelId: "deepseek-reasoner", endpoint: "/v1/chat/completions" }),
  createModel({ hash: "mock-gemini-pro", order: 7, name: "Gemini 2.5 Pro", type: "openai", url: "https://generativelanguage.googleapis.com", modelId: "gemini-2.5-pro", endpoint: "/v1beta/openai/chat/completions" }),
  createModel({ hash: "mock-gemini-flash", order: 8, name: "Gemini 2.5 Flash", type: "openai", url: "https://generativelanguage.googleapis.com", modelId: "gemini-2.5-flash", endpoint: "/v1beta/openai/chat/completions" }),
  createModel({ hash: "mock-qwen-max", order: 9, name: "Qwen3 Max", type: "openai", url: "https://dashscope.aliyuncs.com/compatible-mode", modelId: "qwen3-max", endpoint: "/v1/chat/completions" }),
  createModel({ hash: "mock-qwen-plus", order: 10, name: "Qwen Plus", type: "openai", url: "https://dashscope.aliyuncs.com/compatible-mode", modelId: "qwen-plus", endpoint: "/v1/chat/completions" }),
  createModel({ hash: "mock-kimi-k2", order: 11, name: "Kimi K2", type: "openai", url: "https://api.moonshot.cn", modelId: "kimi-k2-0711-preview", endpoint: "/v1/chat/completions" }),
  createModel({ hash: "mock-kimi-128k", order: 12, name: "Moonshot V1 128K", type: "openai", url: "https://api.moonshot.cn", modelId: "moonshot-v1-128k", endpoint: "/v1/chat/completions" }),
  createModel({ hash: "mock-glm-45", order: 13, name: "GLM-4.5", type: "openai", url: "https://open.bigmodel.cn", modelId: "glm-4.5", endpoint: "/api/paas/v4/chat/completions" }),
  createModel({ hash: "mock-glm-air", order: 14, name: "GLM-4.5-Air", type: "openai", url: "https://open.bigmodel.cn", modelId: "glm-4.5-air", endpoint: "/api/paas/v4/chat/completions" }),
  createModel({ hash: "mock-mistral-large", order: 15, name: "Mistral Large", type: "openai", url: "https://api.mistral.ai", modelId: "mistral-large-latest", endpoint: "/v1/chat/completions" }),
  createModel({ hash: "mock-mistral-small", order: 16, name: "Mistral Small", type: "openai", url: "https://api.mistral.ai", modelId: "mistral-small-latest", endpoint: "/v1/chat/completions" }),
];

const calls: LlmCall[] = Array.from({ length: 24 }, (_, index) => {
  const model = models[index % models.length];
  const failed = index === 7 || index === 19;
  return {
    call_kind: "provider_llm",
    client_source: "internal",
    route: "local_byok",
    call_id: `mock-call-${String(index + 1).padStart(3, "0")}`,
    run_id: `mock-run-${Math.floor(index / 3) + 1}`,
    conversation_id: `mock-conversation-${Math.floor(index / 4) + 1}`,
    provider_call_index: index + 1,
    model_hash: model.model_hash,
    provider_type: model.type,
    provider_url: model.base_url,
    request_type: model.type === "anthropic" ? "messages" : "responses",
    request_url: model.type === "anthropic" ? `${model.base_url}/v1/messages` : `${model.base_url}${model.openai_endpoint}`,
    model_id: model.model_id,
    display_name: model.display_name,
    reasoning_effort: index % 2 === 0 ? "high" : null,
    fast: index % 3 === 0,
    status: failed ? "failed" : "completed",
    finish_reason: failed ? null : "stop",
    created_at_ms: FIXED_NOW - index * 3 * 60_000,
    ttfb_ms: 210 + index * 13,
    ttfr_ms: 290 + index * 15,
    ttft_ms: 370 + index * 17,
    duration_ms: failed ? 812 : 1_420 + index * 71,
    input_tokens: 4_800 + index * 337,
    output_tokens: failed ? 0 : 820 + index * 43,
    total_tokens: failed ? 4_800 + index * 337 : 5_620 + index * 380,
    cache_read_tokens: 3_100 + index * 251,
    cache_write_tokens: 320 + index * 19,
    reasoning_tokens: index % 2 === 0 ? 420 + index * 11 : null,
    message_count: 14 + (index % 8),
    tool_count: 3 + (index % 5),
    http_status: failed ? 429 : 200,
    error_kind: failed ? "provider_rate_limit" : null,
    error_message: failed ? "Mock provider rate limit" : null,
    detailed: true,
    purpose: "chat",
  };
});

let harnessStatus: CursorHarnessStatus = {
  platform: "macos",
  ca: "ready",
  mode: "mixed",
  configured_models: models.length,
  enabled_models: models.length,
  integration: "enabled",
  settings_applied: true,
  foreign_configuration: false,
  proxy_url: "http://127.0.0.1:54321",
  ca_install_command: null,
  account: { state: "official", email: "developer@example.com" },
  catalog: { last_success_ms: 1_700_000_000_000, last_failure_ms: null, last_failure: null },
};

let codexSettings: CodexSettings = { enabled: false, port: 43120, home: "", model_hashes: [], default_model_hash: null };
let codexConfigured = false;
function codexStatus(): CodexStatus {
  return { enabled: codexSettings.enabled, listening: codexSettings.enabled, port: codexSettings.port,
    configured: codexConfigured, config_conflict: false, restorable: codexConfigured, conflicts: [],
    chatgpt_signed_in: true, conversation_detail_mode: "STEPS_COMMANDS", local_mode_guard: false,
    home: codexSettings.home || "~/.codex", calls: 0, last_error: null };
}

let claudeSettings: ClaudeSettings = {
  enabled: true,
  port: 43_121,
  default_model_hash: models[0].model_hash,
  sonnet_model_hash: models[0].model_hash,
  opus_model_hash: models[1].model_hash,
  haiku_model_hash: models[3].model_hash,
};
let claudeTakeover: ClaudeTakeover = {
  cli: {
    state: "ready",
    settings_path: "~/.claude/settings.json",
    backup_path: "",
    restorable: false,
    conflicts: [],
    missing_models: [],
    managed_sources: [],
  },
  desktop: {
    state: "ready",
    config_source: "local",
    profile_path: "~/Library/Application Support/Claude",
    backup_path: "",
    restorable: false,
    conflicts: [],
    guided: false,
  },
};
function claudeStatus(): ClaudeStatus {
  return {
    enabled: claudeSettings.enabled,
    listening: claudeSettings.enabled,
    port: claudeSettings.port,
    base_url: `http://127.0.0.1:${claudeSettings.port}`,
    token: "haxsd-demo-claude-token",
    models: new Set([claudeSettings.default_model_hash, claudeSettings.sonnet_model_hash, claudeSettings.opus_model_hash, claudeSettings.haiku_model_hash].filter(Boolean)).size,
    default_model_hash: claudeSettings.default_model_hash,
    calls: 0,
    last_error: null,
  };
}

let devinSettings: DevinSettings = {
  enabled: false,
  auth_token: "",
  api_port: 43_110,
  inference_port: 43_111,
  local_api_port: 43_112,
  upstream_api_url: "https://server.codeium.com",
  bindings: [{
    model_uid: "demo-model",
    model_hash: models[0].model_hash,
    display_name: models[0].display_name,
    context_window_tokens: models[0].context_window_tokens,
    enabled: true,
    kind: "standard",
    routes: [],
    active_route_id: null,
  }],
  default_model_uid: "demo-model",
};

let devinHostStatus: DevinHostStatus | null = null;
let devinHostReceipt: DevinHostReceipt | null = null;
let devinExtensionReceipt: DevinExtensionReceipt | null = null;

const demoTokenPrice = {
  input_per_million: 3,
  output_per_million: 15,
  cache_read_per_million: 0.3,
  cache_write_per_million: 3.75,
};
const demoCurrency = { fixed: demoTokenPrice, peak: demoTokenPrice, off_peak: demoTokenPrice };
let pricingSettings: TokenPricingSettings = {
  models: Object.fromEntries(
    models.map((model) => [model.model_hash, { mode: "fixed", cny: demoCurrency, usd: demoCurrency }]),
  ),
};
let detailed = true;
let portSettings = { proxy_port: 0, service_port: 0 };
let desktopSettings: DesktopSettings = { silent_start: false, show_dock_icon: true, developer_mode: false };
let proxySettings: ProxySettings = {
  mode: "default",
  address: "",
  auth_enabled: false,
  username: "",
  has_password: false,
};
let tabSettings: TabSettings = { mode: "public", address: "" };
let compactionSettings: CompactionSettings = {
  auto: true,
  summary_model_hash: "",
  summary_output_tokens: null,
  reserve_percent: null,
  retention_percent: null,
  max_retained_tokens: null,
};
// 演示数据：数据库文件比可清理的观测数据大得多，正好演示两种"大"的区别；抓取缓存再单列一个
// 较小的数字，演示它与库内占用是分开统计、分开清理的。
// `conversation_retention_days` 与真实服务端的回收常量一致（见 server/src/store/retention.rs）。
let storage: StatisticsStorage = {
  bytes: 26_004_480,
  database_bytes: 1_212_334_080,
  // 落盘的大工具结果与压缩历史：与库内观测数据分开报，按服务端的 7 天保留期惰性清理。
  artifact_bytes: 41_943_040,
  artifact_retention_days: 7,
  call_count: calls.length,
  trace_count: calls.length,
  conversation_retention_days: 3,
  web_cache_bytes: 8_912_896,
  web_cache_files: 412,
};

export function installDemoApi() {
  const nativeFetch = window.fetch.bind(window);

  window.fetch = async (input, init) => {
    const requestUrl = input instanceof Request ? input.url : input instanceof URL ? input.href : input;
    const url = new URL(requestUrl, window.location.href);
    if (!url.pathname.startsWith(API_ROOT)) return nativeFetch(input, init);

    const path = url.pathname.slice(API_ROOT.length) || "/";
    const method = (init?.method ?? (input instanceof Request ? input.method : "GET")).toUpperCase();
    const body = await readBody(input, init);

    if (path === "/app-info") return json({ version: "1.6.13", executable_path: "D:\\haxsd-byok\\haxsd-byok-desktop.exe", data_dir: "C:\\Users\\demo\\.haxsd-byok" });
    if (path === "/models" && method === "GET") return json(models);
    if (path === "/models" && method === "POST") return json(models);
    if (path === "/models/order") return json(models);
    if (path === "/models/discover") return json({ models: models.map((model) => model.model_id) });
    // 复制模型由服务端连凭据一起做；mock 里返回一份模型即可，与 update 的处理一致。
    if (/^\/models\/[^/]+\/duplicate$/.test(path)) return json(models[0]);
    if (path === "/models/import-v0049" && method === "GET") {
      return json({ source: "demo", total: 0, new_models: 0, existing_models: 0, models: [] });
    }
    if (path === "/models/import-v0049") return json({ imported: 0, skipped: 0, total: 0 });
    if (/^\/models\/[^/]+\/test\/[^/]+$/.test(path) && method === "POST") {
      return json({ duration_ms: 1_284, first_valid_response_ms: 418, output_tokens: 42, tokens_per_second: 38.6, tokens_estimated: false, output: "Mock connectivity test passed." });
    }
    if (/^\/models\/[^/]+\/test\/[^/]+$/.test(path) || /^\/models\/[^/]+$/.test(path)) {
      return method === "DELETE" ? empty() : json(models[0]);
    }
    if (path === "/overview") return json(createOverview(url.searchParams));
    if (path === "/llm-calls") return json(calls);
    if (path.startsWith("/llm-calls/")) return json(createCallDetail(path.slice("/llm-calls/".length)));
    if (path === "/harness/cursor/status") return json(harnessStatus);
    if (path === "/harness/cursor/ca/initialize") return json(harnessStatus);
    if (path === "/harness/cursor/mode") {
      const mode = ((body as { mode?: unknown } | null)?.mode ?? "official") as CursorMode;
      const takesOver = mode !== "official";
      harnessStatus = {
        ...harnessStatus,
        mode,
        integration: takesOver ? "enabled" : "disabled",
        settings_applied: takesOver,
        proxy_url: takesOver ? "http://127.0.0.1:54321" : null,
        account: mode === "local"
          ? { state: "local", email: "cursor@ai.com" }
          : { state: "official", email: "developer@example.com" },
      };
      return json(harnessStatus);
    }
    if (path === "/codex/status") return json(codexStatus());
    if (path === "/codex/settings" && method === "GET") return json(codexSettings);
    if (path === "/codex/settings" && method === "PUT") {
      // 原句、前缀与状态码都照抄服务端（server/src/error.rs:12 的 `configuration error: {0}` +
      // gateway.rs:201；`Error::Config` → 400 `invalid_argument`），这样演示里的失败提示也能命中
      // harnessErrorText 的映射。
      const next = body as CodexSettings;
      const onlyModelsChanged = next.enabled === codexSettings.enabled
        && next.port === codexSettings.port
        && next.home === codexSettings.home
        && next.default_model_hash === codexSettings.default_model_hash;
      if (codexConfigured && !onlyModelsChanged) {
        return json(errorBody(400, "configuration error: restore the applied Codex configuration before changing gateway settings"), 400);
      }
      codexSettings = next;
      return json(codexSettings);
    }
    if (path === "/codex/apply" && method === "POST") {
      // 服务端 apply 的两个前置条件各有一句原话（gateway.rs:258-262 / :277-281），不能合并成
      // 一句自造的"Enable Codex and select a model first"：既不像产品文案，也认不出是哪一条。
      if (!codexSettings.enabled) return json(errorBody(400, "configuration error: enable the Codex gateway before applying configuration"), 400);
      if (!codexSettings.default_model_hash) return json(errorBody(400, "configuration error: select a default Codex model"), 400);
      codexConfigured = true;
      return json(codexStatus());
    }
    if (path === "/codex/restore" && method === "POST") { codexConfigured = false; return json(codexStatus()); }
    if (path === "/claude/settings" && method === "GET") return json(claudeSettings);
    if (path === "/claude/settings" && method === "PUT") {
      claudeSettings = body as ClaudeSettings;
      return json(claudeSettings);
    }
    if (path === "/claude/status") return json(claudeStatus());
    if (path === "/claude/takeover") return json(claudeTakeover);
    if (path === "/claude/takeover/cli" && method === "PUT") {
      const action = (body as { action?: string } | null)?.action ?? "apply";
      claudeTakeover = {
        ...claudeTakeover,
        cli: {
          ...claudeTakeover.cli,
          state: action === "apply" ? "applied" : "ready",
          backup_path: action === "apply" ? "~/.claude/settings.json.haxsd-byok.bak" : "",
          restorable: action === "apply",
          conflicts: [],
        },
      };
      return json(claudeTakeover.cli);
    }
    if (path === "/claude/takeover/desktop" && method === "PUT") {
      const action = (body as { action?: string } | null)?.action ?? "apply";
      claudeTakeover = {
        ...claudeTakeover,
        desktop: {
          ...claudeTakeover.desktop,
          state: action === "apply" ? "applied" : "ready",
          backup_path: action === "apply" ? "~/Library/Application Support/Claude/config.json.haxsd-byok.bak" : "",
          restorable: action === "apply",
          conflicts: [],
        },
      };
      return json(claudeTakeover.desktop);
    }
    if (path === "/devin/settings" && method === "GET") return json(devinSettings);
    if (path === "/devin/settings") {
      devinSettings = body as DevinSettings;
      return json(devinSettings);
    }
    // 演示里也要有这几条：缺了它们，演示页面会在顶栏弹一条红色「部分数据刷新失败」，
    // 并在 Devin / 插件页显示"未处理的端点"，看起来像产品坏了。
    if (path === "/devin/status") return json({
      enabled: devinSettings.enabled,
      listening: devinSettings.enabled,
      ready: devinSettings.enabled,
      bound_ports: devinSettings.enabled ? {
        api: devinSettings.api_port,
        inference: devinSettings.inference_port,
        local_api: devinSettings.local_api_port,
      } : null,
      calls: 0,
      last_call_success: null,
      last_call_at_ms: null,
    });
    if (path === "/harness/devin/model-uids") {
      return json({
        path: "/demo/resources/app/extensions/windsurf/extension.js",
        choices: [
          { name: "SWE-1.6 Slow", uid: "swe-1-6-slow" },
          { name: "SWE-1.5", uid: "swe-1-5" },
          { name: "Claude Sonnet 4", uid: "claude-sonnet-4" },
          { name: "GPT-5.1", uid: "gpt-5-1" },
        ],
      });
    }
    if (path === "/plugins/runtime") {
      return json({ state: "ready", version: "0.1.0-demo", target: null, phase: null, downloaded_bytes: 0, total_bytes: null, error: null });
    }
    if (path === "/plugins") return json([]);
    if (path === "/settings/commit" && method === "GET") {
      return json({ model_id: "", prompt: "", prompt_locale: "zh-CN", default_prompt: "Write a commit message for the staged changes." });
    }
    if (path === "/settings/commit") {
      const next = body as { model_id?: string; prompt?: string; prompt_locale?: "zh-CN" | "en-US" } | null;
      return json({
        model_id: next?.model_id ?? "",
        prompt: next?.prompt ?? "",
        prompt_locale: next?.prompt_locale ?? "zh-CN",
        default_prompt: "Write a commit message for the staged changes.",
      });
    }
    if (path === "/settings/compaction" && method === "GET") return json(compactionSettings);
    if (path === "/settings/compaction" && method === "PUT") {
      compactionSettings = body as CompactionSettings;
      return json(compactionSettings);
    }
    if (path === "/harness/devin/host/status" && method === "GET") {
      const hostPath = url.searchParams.get("path") || "C:\\Users\\demo\\AppData\\Roaming\\Devin\\User\\settings.json";
      const configured = Boolean(devinHostReceipt);
      const extensionPath = "D:\\devin\\Devin\\resources\\app\\extensions\\windsurf\\dist\\extension.js";
      const extensionPatched = Boolean(devinExtensionReceipt);
      devinHostStatus ??= {
        path: hostPath,
        compatible: true,
        unconfigured: !configured,
        configured,
        parts: { api: configured, inference: configured },
        ports: devinHostReceipt?.ports ?? null,
        backup_path: `${hostPath}.devin-router.backup`,
        backup_available: configured,
        current_sha256: devinHostReceipt?.patched_sha256 ?? "demo-clean",
        backup_sha256: devinHostReceipt?.original_sha256 ?? null,
        restore_receipt: devinHostReceipt,
        extension: {
          path: extensionPath,
          version: "3.11.216",
          supported_sha256: "A063DAE82C91328E17AE16121E90780087674B65839B5AD39915C9C59C652FF0",
          compatible: true,
          clean: !extensionPatched,
          patched: extensionPatched,
          backup_path: `${extensionPath}.devin-router-gate.backup`,
          backup_available: extensionPatched,
          current_sha256: devinExtensionReceipt?.patched_sha256 ?? "demo-extension-clean",
          backup_sha256: devinExtensionReceipt?.original_sha256 ?? null,
          restore_receipt: devinExtensionReceipt,
          message: "",
        },
        message: "",
      };
      return json({
        ...devinHostStatus,
        path: hostPath,
        unconfigured: !configured,
        configured,
        ports: devinHostReceipt?.ports ?? null,
        restore_receipt: devinHostReceipt,
        extension: {
          ...devinHostStatus.extension,
          clean: !extensionPatched,
          patched: extensionPatched,
          backup_available: extensionPatched,
          current_sha256: devinExtensionReceipt?.patched_sha256 ?? "demo-extension-clean",
          backup_sha256: devinExtensionReceipt?.original_sha256 ?? null,
          restore_receipt: devinExtensionReceipt,
        },
      });
    }
    if (path === "/harness/devin/host/apply" && method === "POST") {
      const hostPath = (body as { path?: string } | null)?.path || "C:\\Users\\demo\\AppData\\Roaming\\Devin\\User\\settings.json";
      devinHostReceipt = { path: hostPath, backup_path: `${hostPath}.devin-router.backup`, original_sha256: "demo-original", patched_sha256: "demo-patched", ports: { api_port: devinSettings.api_port, inference_port: devinSettings.inference_port } };
      const extensionPath = "D:\\devin\\Devin\\resources\\app\\extensions\\windsurf\\dist\\extension.js";
      devinExtensionReceipt = {
        path: extensionPath,
        backup_path: `${extensionPath}.devin-router-gate.backup`,
        version: "3.11.216",
        original_sha256: "demo-extension-original",
        patched_sha256: "demo-extension-patched",
      };
      devinHostStatus = null;
      return json({ settings: devinHostReceipt, extension: devinExtensionReceipt });
    }
    if (path === "/harness/devin/host/restore" && method === "POST") {
      if (!devinHostReceipt || (body as { path?: string } | null)?.path !== devinHostReceipt.path) {
        return json(errorBody(400, "no usable Devin host restore receipt"), 400);
      }
      devinHostReceipt = null;
      devinExtensionReceipt = null;
      devinHostStatus = null;
      return json({ restored: true });
    }
    if (path === "/settings/pricing" && method === "GET") return json(pricingSettings);
    if (path.startsWith("/settings/pricing/")) {
      const modelHash = decodeURIComponent(path.slice("/settings/pricing/".length));
      if (method === "PUT") pricingSettings.models[modelHash] = body as ModelTokenPricing;
      else if (method === "DELETE") delete pricingSettings.models[modelHash];
      else return json(errorBody(405, "Unsupported pricing method"), 405);
      return json(pricingSettings);
    }
    if (path === "/settings/observability" && method === "GET") return json({ detailed });
    if (path === "/settings/observability") {
      detailed = Boolean((body as { detailed?: unknown } | null)?.detailed);
      return json({ detailed });
    }
    if (path === "/settings/ports" && method === "GET") return json(portSettings);
    if (path === "/settings/ports") {
      portSettings = body as typeof portSettings;
      return json(portSettings);
    }
    if (path === "/settings/storage/statistics" && method === "GET") return json(storage);
    if (path === "/settings/storage/statistics") {
      const scope = (body as { scope?: string } | null)?.scope ?? "details";
      // 清理不会缩小数据库文件（SQLite 复用释放页），演示里也照这个事实来。抓取缓存是文件系统
      // 里的东西，只有 web_cache 作用域清它——两者分开，与真实服务端一致。
      storage = scope === "all"
        ? { ...storage, bytes: 0, call_count: 0, trace_count: 0 }
        : scope === "web_cache"
          ? { ...storage, web_cache_bytes: 0, web_cache_files: 0 }
          : { ...storage, bytes: 0 };
      return json(storage);
    }
    if (path === "/settings/proxy" && method === "GET") return json(proxySettings);
    if (path === "/settings/proxy") {
      const next = body as Partial<ProxySettings>;
      proxySettings = { ...proxySettings, ...next, has_password: Boolean(next.has_password) };
      return json(proxySettings);
    }
    if (path === "/settings/outbound") return json({ system_proxy_bypassed: false });
    if (path === "/settings/tab" && method === "GET") return json(tabSettings);
    if (path === "/settings/tab") {
      tabSettings = body as TabSettings;
      return json(tabSettings);
    }
    if (path === "/settings/desktop" && method === "GET") return json(desktopSettings);
    // 与真实服务端一致：请求只带改动的字段，合并后回整份设置。
    if (path === "/settings/desktop") {
      desktopSettings = { ...desktopSettings, ...(body as Partial<DesktopSettings>) };
      return json(desktopSettings);
    }
    if (path === "/desktop/open-external-url") {
      const target = (body as { url?: string } | null)?.url;
      if (target) {
        const next = new URL(target, window.location.href);
        if (next.origin === window.location.origin && next.hash) window.location.hash = next.hash;
      }
      return empty();
    }
    if (path.endsWith("/dismissals")) return empty();

    return json(errorBody(404, `Unhandled demo endpoint: ${method} ${path}`), 404);
  };
}

function createModel({ hash, order, name, type, url, modelId, endpoint = "/v1/responses" }: {
  hash: string;
  order: number;
  name: string;
  type: Model["type"];
  url: string;
  modelId: string;
  endpoint?: string;
}): Model {
  return {
    model_hash: hash,
    sort_order: order,
    display_name: name,
    group_name: null,
    type,
    base_url: url,
    use_full_url: false,
    api_key_configured: true,
    tooltip_data: `${name} Mock 通道`,
    model_id: modelId,
    reasoning_effort: type === "openai" ? "high" : null,
    openai_endpoint: type === "openai" ? endpoint : "",
    openai_extra_params_enabled: false,
    openai_extra_params: {},
    custom_headers_enabled: false,
    custom_headers: {},
    anthropic_extra_params_enabled: false,
    anthropic_extra_params: {},
    context_window_tokens: 200_000,
    max_completion_tokens: type === "openai" ? 32_000 : null,
    anthropic_max_tokens: type === "anthropic" ? 32_000 : null,
    anthropic_thinking_effort: type === "anthropic" ? "high" : null,
    thinking_budget_tokens: null,
    created_at_ms: FIXED_NOW - order * 86_400_000,
    updated_at_ms: FIXED_NOW,
  };
}

function createOverview(params: URLSearchParams): Overview {
  const start = Number(params.get("start_ms"));
  const end = Number(params.get("end_ms"));
  const duration = Number.isFinite(start) && Number.isFinite(end) && end > start ? end - start : 365 * 86_400_000;
  const granularity = duration <= 2 * 60 * 60_000 ? "minute" : duration <= 2 * 86_400_000 ? "hour" : "day";
  const step = granularity === "minute" ? 60_000 : granularity === "hour" ? 3_600_000 : 86_400_000;
  const count = granularity === "minute" ? Math.min(60, Math.max(10, Math.ceil(duration / step))) : granularity === "hour" ? Math.min(24, Math.max(8, Math.ceil(duration / step))) : Math.min(365, Math.max(7, Math.ceil(duration / step)));
  const series = createSeries(count, step, Number.isFinite(end) && end > 0 ? end : FIXED_NOW);
  const totals = series.reduce((sum, bucket) => ({
    input: sum.input + bucket.input_tokens,
    cacheRead: sum.cacheRead + bucket.cache_read_tokens,
    cacheWrite: sum.cacheWrite + bucket.cache_write_tokens,
    output: sum.output + bucket.output_tokens,
  }), { input: 0, cacheRead: 0, cacheWrite: 0, output: 0 });
  const llmCalls = Math.max(12, Math.round(count * 5.4));

  return {
    metrics: {
      llm_calls: llmCalls,
      successful_calls: llmCalls - Math.max(1, Math.floor(llmCalls * 0.008)),
      failed_calls: Math.max(1, Math.floor(llmCalls * 0.008)),
      token_usage: totals.input + totals.cacheRead + totals.cacheWrite + totals.output,
      prompt_tokens: totals.input + totals.cacheRead + totals.cacheWrite,
      input_tokens: totals.input,
      cache_read_tokens: totals.cacheRead,
      cache_write_tokens: totals.cacheWrite,
      output_tokens: totals.output,
    },
    token_usage_granularity: granularity,
    token_usage_series: series,
  };
}

function createSeries(count: number, step: number, end: number): OverviewTokenUsageBucket[] {
  return Array.from({ length: count }, (_, index) => {
    const wave = 0.72 + ((index * 17) % 31) / 50;
    const input = Math.round(800 * wave);
    const read = Math.round(79_200 * wave);
    const write = Math.round(7_500 * wave);
    const output = Math.round(12_500 * wave);
    const usedModels = [models[4], models[2]];
    const first = { input: Math.round(input * 0.7), read: Math.round(read * 0.7), write: Math.round(write * 0.7), output: Math.round(output * 0.7) };
    return {
      bucket_start_ms: end - (count - index) * step,
      input_tokens: input,
      cache_read_tokens: read,
      cache_write_tokens: write,
      output_tokens: output,
      models: usedModels.map((model, part) => ({
        model_hash: model.model_hash,
        model_id: model.model_id,
        display_name: model.display_name,
        llm_calls: 3,
        input_tokens: part === 0 ? first.input : input - first.input,
        cache_read_tokens: part === 0 ? first.read : read - first.read,
        cache_write_tokens: part === 0 ? first.write : write - first.write,
        output_tokens: part === 0 ? first.output : output - first.output,
      })),
    };
  });
}

function createCallDetail(id: string): CallDetail {
  const call = calls.find((item) => item.call_id === decodeURIComponent(id)) ?? calls[0];
  return {
    call,
    request: {
      headers: { authorization: "Bearer sk-demo-••••", "content-type": "application/json" },
      body: { model: call.model_id, stream: true, messages: [{ role: "user", content: "Mock Agent request" }] },
      byte_count: 8_426,
    },
    response_chunks: [
      { seq: 1, received_offset_ms: 418, data: "event: response.created", byte_count: 128 },
      { seq: 2, received_offset_ms: 512, data: "event: response.output_text.delta", byte_count: 256 },
    ],
    cursor_trace: null,
    compaction: null,
  };
}

async function readBody(input: RequestInfo | URL, init?: RequestInit): Promise<unknown> {
  const raw = init?.body ?? (input instanceof Request ? await input.clone().text() : null);
  if (typeof raw !== "string" || raw.length === 0) return null;
  try { return JSON.parse(raw) as unknown; }
  catch { return raw; }
}

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function empty() {
  return new Response(null, { status: 204 });
}

/**
 * 演示里的错误响应：与真实服务端**同形**（server/src/error.rs 的 IntoResponse）。
 *
 * 这里以前写的是 `{error:{message}}`，而 `shared/api.ts` 读的是 `parsed.message`——读不到
 * 就把整段正文当成提示条内容，于是演示里的错误提示是一串原始 JSON。真实服务端的形状是
 * `{code,message}`，code 由状态码映射（与 error.rs 同一张表），演示要能替身就得一模一样。
 */
export function errorCode(status: number): string {
  if (status === 400) return "invalid_argument";
  if (status === 404) return "not_found";
  if (status === 409) return "aborted";
  if (status === 502) return "unavailable";
  return "internal";
}

/** 错误正文：`{code,message}`，与 error.rs 一致。 */
export function errorBody(status: number, message: string) {
  return { code: errorCode(status), message };
}
