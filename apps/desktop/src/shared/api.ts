import type { CommitPromptLocale, Locale } from "../i18n/runtime";

export type ModelType = "openai" | "anthropic";

export interface Model {
  model_hash: string;
  sort_order: number;
  display_name: string;
  group_name: string | null;
  type: ModelType;
  base_url: string;
  use_full_url: boolean;
  api_key: string;
  tooltip_data: string;
  model_id: string;
  reasoning_effort: string | null;
  openai_endpoint: string;
  openai_extra_params_enabled: boolean;
  openai_extra_params: Record<string, unknown>;
  custom_headers_enabled: boolean;
  custom_headers: Record<string, string>;
  anthropic_extra_params_enabled: boolean;
  anthropic_extra_params: Record<string, unknown>;
  context_window_tokens: number | null;
  max_completion_tokens: number | null;
  anthropic_max_tokens: number | null;
  anthropic_thinking_effort: string | null;
  thinking_budget_tokens: number | null;
  created_at_ms: number;
  updated_at_ms: number;
}

export interface ModelInput {
  sort_order: number;
  display_name: string;
  group_name: string | null;
  type: ModelType;
  base_url: string;
  use_full_url: boolean;
  api_key: string;
  tooltip_data: string;
  model_id: string;
  reasoning_effort: string | null;
  openai_endpoint: string;
  openai_extra_params_enabled: boolean;
  openai_extra_params: Record<string, unknown>;
  custom_headers_enabled: boolean;
  custom_headers: Record<string, string>;
  anthropic_extra_params_enabled: boolean;
  anthropic_extra_params: Record<string, unknown>;
  context_window_tokens: number | null;
  max_completion_tokens: number | null;
  anthropic_max_tokens: number | null;
  anthropic_thinking_effort: string | null;
  thinking_budget_tokens: number | null;
}

export interface ModelDiscoveryInput {
  type: ModelType;
  base_url: string;
  api_key: string;
  custom_headers_enabled: boolean;
  custom_headers: Record<string, string>;
}

export interface LegacyModelImportPreviewItem {
  model_hash: string;
  display_name: string;
  model_id: string;
  type: ModelType;
  existing: boolean;
}

export interface LegacyModelImportPreview {
  source: string;
  total: number;
  new_models: number;
  existing_models: number;
  models: LegacyModelImportPreviewItem[];
}

export interface LegacyModelImportResult {
  imported: number;
  skipped: number;
  total: number;
}

export interface ModelConnectivityResult {
  duration_ms: number;
  first_valid_response_ms: number | null;
  output_tokens: number;
  tokens_per_second: number;
  tokens_estimated: boolean;
  output: string;
}

export type CaState = "missing" | "untrusted" | "ready" | "invalid";
export type IntegrationState = "disabled" | "enabled" | "degraded";
export interface CursorHarnessStatus {
  platform: string;
  ca: CaState;
  configured_models: number;
  enabled_models: number;
  integration: IntegrationState;
  settings_applied: boolean;
  proxy_url: string | null;
  ca_install_command: string | null;
}

export interface PortSettings {
  proxy_port: number;
  service_port: number;
}

export type DevinBindingKind = "standard" | "context_compression";

export interface DevinRoute {
  route_id: string;
  model_hash: string;
  label: string;
  enabled: boolean;
}

export interface DevinModelBinding {
  model_uid: string;
  model_hash: string;
  display_name: string;
  context_window_tokens: number | null;
  enabled: boolean;
  kind: DevinBindingKind;
  routes: DevinRoute[];
  active_route_id: string | null;
}

export interface DevinSettings {
  enabled: boolean;
  auth_token: string;
  api_port: number;
  inference_port: number;
  local_api_port: number;
  /** Every request the gateway does not serve locally is forwarded here. */
  upstream_api_url: string;
  bindings: DevinModelBinding[];
}

/** Reported by the server: the gateway ports belong to another origin, so the page cannot probe them. */
export interface DevinStatus {
  enabled: boolean;
  listening: boolean;
  calls: number;
}

export interface DevinHostPorts {
  api_port: number;
  inference_port: number;
  local_api_port: number;
}

export interface DevinHostPatchStatus {
  path: string;
  compatible: boolean;
  clean: boolean;
  patched: boolean;
  parts: { api: boolean; restart: boolean; inference: boolean; local_api: boolean };
  ports: DevinHostPorts | null;
  backup_path: string;
  backup_available: boolean;
  current_sha256: string;
  backup_sha256: string | null;
  message: string;
}

export interface DevinHostPatchReceipt {
  path: string;
  backup_path: string;
  original_sha256: string;
  patched_sha256: string;
  ports: DevinHostPorts;
}

export interface StatisticsStorage {
  bytes: number;
  call_count: number;
  trace_count: number;
}

export type StatisticsStorageScope = "details" | "all";

export type ProxyMode = "default" | "custom";

export interface ProxySettings {
  mode: ProxyMode;
  address: string;
  auth_enabled: boolean;
  username: string;
  has_password: boolean;
}

export interface ProxySettingsInput {
  mode: ProxyMode;
  address: string;
  auth_enabled: boolean;
  username: string;
  password?: string;
}

export type TabMode = "public" | "direct" | "custom";

export interface TabSettings {
  mode: TabMode;
  address: string;
}

export interface DesktopSettings {
  silent_start: boolean;
  show_dock_icon: boolean;
}

export interface CommitSettings {
  model_id: string;
  prompt: string;
  prompt_locale: CommitPromptLocale;
}

export interface CommitSettingsView extends CommitSettings {
  default_prompt: string;
}

/** 一组 Token 单价（每百万 token）。 */
export interface TokenPrice {
  /** 输入价格（缓存未命中）。 */
  input_per_million: number;
  output_per_million: number;
  /** 输入价格（缓存命中）。 */
  cache_read_per_million: number;
  cache_write_per_million: number;
}

/** 单一币种下的整套价格。 */
export interface CurrencyPricing {
  /** 「固定单价」模式使用的价格。 */
  fixed: TokenPrice;
  /** 高峰时段价格。 */
  peak: TokenPrice;
  /** 低谷时段价格。 */
  off_peak: TokenPrice;
}

/** 首页「价值估算」的计价模式。 */
export type PricingMode = "fixed" | "peak_off_peak";

/**
 * 首页「价值估算」的价格设置。
 *
 * 界面语言决定使用哪一套价格：简体中文用人民币，英文用美元。
 * 两种币种各自维护完整的价格表，切换语言不会互相影响。
 */
export interface TokenPricingSettings {
  mode: PricingMode;
  /** 人民币价格。 */
  cny: CurrencyPricing;
  /** 美元价格。 */
  usd: CurrencyPricing;
}

export type PluginRuntimeState = "uninitialized" | "initializing" | "ready" | "failed" | "unsupported";
export type PluginRuntimePhase = "checking" | "downloading" | "verifying" | "installing" | "validating";

export interface PluginRuntimeStatus {
  state: PluginRuntimeState;
  version: string;
  target: string | null;
  phase: PluginRuntimePhase | null;
  downloaded_bytes: number;
  total_bytes: number | null;
  error: string | null;
}

/** 插件提供的显示文本:纯字符串或 locale → 文本映射。 */
export type PluginLocalizedText = string | Record<string, string>;

export function pluginText(value: PluginLocalizedText | null | undefined, locale: string): string {
  if (!value) return "";
  if (typeof value === "string") return value;
  if (value[locale]) return value[locale];
  const language = locale.split("-")[0].toLowerCase();
  for (const [key, text] of Object.entries(value)) {
    const normalized = key.toLowerCase();
    if (normalized === language || normalized.startsWith(`${language}-`)) return text;
  }
  return value["en-US"] ?? value["en"] ?? Object.values(value)[0] ?? "";
}

export interface PluginResourceState {
  status: "ready" | "cooling" | "invalid";
  retryAtMs?: number | null;
  message?: string | null;
}

export interface PluginResourceMetric {
  id: string;
  label: PluginLocalizedText;
  unit: "percent" | "count";
  value: number;
  resetAtMs?: number | null;
}

export interface PluginResourceView {
  id: string;
  state: PluginResourceState;
  displayName: string;
  description: PluginLocalizedText | null;
  metrics: PluginResourceMetric[];
  createdAtMs: number;
}

export interface PluginAddMethod {
  type: "oauth2.0" | "oauth2.authorization-code";
  id: string;
  displayName: PluginLocalizedText;
  description: PluginLocalizedText | null;
  callback?: { port: number | null; path: string | null };
}

export interface PluginImportDescriptor {
  displayName: PluginLocalizedText;
  description: PluginLocalizedText | null;
  accept: string[];
  multiple: boolean;
}

export interface PluginResourceAction {
  id: string;
  displayName: PluginLocalizedText;
  description: PluginLocalizedText | null;
  target: "resource" | "card";
  destructive: boolean;
}

export interface PluginResourceActionField {
  id: string;
  label: PluginLocalizedText;
  value: string;
}

export interface PluginResourceActionCard {
  id: string;
  title: PluginLocalizedText;
  status: PluginLocalizedText | null;
  grantedAtMs: number | null;
  expiresAtMs: number | null;
  fields: PluginResourceActionField[];
}

export interface PluginResourceActionResult {
  title: PluginLocalizedText;
  description: PluginLocalizedText | null;
  cards: PluginResourceActionCard[];
}

export interface PluginResourceDescriptor {
  type: string;
  displayName: PluginLocalizedText;
  add: PluginAddMethod[];
  import: PluginImportDescriptor | null;
  canRefresh: boolean;
  canRemove: boolean;
  actions: PluginResourceAction[];
  resources: PluginResourceView[];
}

export interface PluginModelDescriptor {
  id: string;
  pluginId: string;
  pluginName: string;
  providerId: string;
  modelId: string;
  displayName: string;
  description: string | null;
  icon: string;
  providerType: string;
  maxOutputTokens: number | null;
  images: boolean;
  enabled: boolean;
}

export interface PluginProviderDescriptor {
  id: string;
  pluginId: string;
  displayName: PluginLocalizedText;
  description: PluginLocalizedText | null;
  providerType: string;
  resourceType: string | null;
  hasModels: boolean;
  configured: boolean;
  models: PluginModelDescriptor[];
}

export interface PluginDescriptor {
  id: string;
  name: string;
  version: string;
  author: string | null;
  icon: string;
  providers: PluginProviderDescriptor[];
  resources: PluginResourceDescriptor[];
}

export interface PluginOAuthBegin {
  sessionId: string;
  userCode: string | null;
  verificationUrl: string;
  verificationUrlComplete: string | null;
  expiresAtMs: number;
  pollIntervalMs: number;
}

export type PluginOAuthPoll =
  | { status: "pending"; pollIntervalMs: number }
  | { status: "completed"; added: number; updated: number; modelSyncError: string | null }
  | { status: "denied"; message: string | null }
  | { status: "failed"; message: string };

export interface PluginImportFile {
  name: string;
  content: string;
}

export interface PluginImportResult {
  added: number;
  updated: number;
  warnings: string[];
  modelSyncError: string | null;
}

export function configuredPluginModels(plugins: PluginDescriptor[]): PluginModelDescriptor[] {
  return plugins.flatMap((plugin) =>
    plugin.providers.flatMap((provider) => provider.configured ? provider.models.filter((model) => model.enabled) : []));
}

export interface OverviewMetrics {
  llm_calls: number;
  successful_calls: number;
  failed_calls: number;
  token_usage: number;
  prompt_tokens: number;
  input_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  output_tokens: number;
}

export type TokenUsageGranularity = "minute" | "hour" | "day";

export interface OverviewTokenUsageBucket {
  bucket_start_ms: number;
  input_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  output_tokens: number;
}

export interface Overview {
  metrics: OverviewMetrics;
  token_usage_granularity: TokenUsageGranularity;
  token_usage_series: OverviewTokenUsageBucket[];
}

export interface LlmCall {
  call_kind: "provider_llm" | "cursor_official";
  route: "local_byok" | "cursor_official";
  call_id: string;
  run_id: string;
  conversation_id: string;
  provider_call_index: number;
  model_hash: string | null;
  provider_type: string;
  provider_url: string;
  request_type: string;
  request_url: string;
  model_id: string;
  display_name: string;
  reasoning_effort: string | null;
  fast: boolean | null;
  status: string;
  finish_reason: string | null;
  created_at_ms: number;
  ttfb_ms: number | null;
  ttft_ms: number | null;
  ttfr_ms: number | null;
  duration_ms: number | null;
  input_tokens: number | null;
  output_tokens: number | null;
  total_tokens: number | null;
  cache_read_tokens: number | null;
  cache_write_tokens: number | null;
  reasoning_tokens: number | null;
  message_count: number;
  tool_count: number;
  http_status: number | null;
  error_kind: string | null;
  error_message: string | null;
  detailed: boolean;
}

export interface CallDetail {
  call: LlmCall;
  request: { headers: unknown; body: unknown; byte_count: number } | null;
  response_chunks: Array<{ seq: number; received_offset_ms: number; data: string; byte_count: number }>;
  cursor_trace: {
    trace: {
      request_id: string;
      conversation_id: string | null;
      route: "local_byok" | "cursor_official";
      model_id: string | null;
      status: string;
      request_bytes: number;
      response_bytes: number;
      response_event_count: number;
      http_status: number | null;
      received_at_ms: number;
      first_response_at_ms: number | null;
      finished_at_ms: number | null;
      error_message: string | null;
    };
    artifacts: Array<{
      seq: number;
      artifact_type: string;
      source: string;
      metadata: unknown;
      created_at_ms: number;
      byte_count: number;
      encoding: "utf8" | "base64";
      data: string;
    }>;
  } | null;
}

const packagedDesktop = "__TAURI_INTERNALS__" in window
  || window.location.protocol === "tauri:"
  || window.location.hostname === "tauri.localhost";
const API_ROOT = "/__byok-api__/api";

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(`${API_ROOT}${path}`, {
      ...init,
      headers: init?.body ? { "content-type": "application/json", ...init.headers } : init?.headers,
    });
  } catch (cause) {
    throw new Error(t("无法连接本地管理服务"), { cause });
  }
  if (!response.ok) {
    const body = await response.text();
    let message = body;
    try {
      const parsed = JSON.parse(body) as { message?: unknown };
      if (typeof parsed.message === "string") message = parsed.message;
    } catch {
      // Plain-text errors are already suitable for display.
    }
    throw new Error(message || `${response.status} ${response.statusText}`);
  }
  if (response.status === 204) return undefined as T;
  return response.json() as Promise<T>;
}

export const api = {
  models: () => request<Model[]>("/models"),
  createModels: (models: ModelInput[]) => request<Model[]>("/models", { method: "POST", body: JSON.stringify({ models }) }),
  reorderModels: (modelHashes: string[]) => request<Model[]>("/models/order", { method: "PUT", body: JSON.stringify({ model_hashes: modelHashes }) }),
  discoverModels: (input: ModelDiscoveryInput) => request<{ models: string[] }>("/models/discover", { method: "POST", body: JSON.stringify(input) }),
  previewV0049Models: () => request<LegacyModelImportPreview>("/models/import-v0049"),
  importV0049Models: () => request<LegacyModelImportResult>("/models/import-v0049", { method: "POST" }),
  updateModel: (hash: string, model: ModelInput) => request<Model>(`/models/${hash}`, { method: "PUT", body: JSON.stringify(model) }),
  deleteModel: (hash: string) => request<void>(`/models/${hash}`, { method: "DELETE" }),
  testModel: (hash: string, testId: string, signal?: AbortSignal) => request<ModelConnectivityResult>(`/models/${encodeURIComponent(hash)}/test/${encodeURIComponent(testId)}`, { method: "POST", signal }),
  cancelModelTest: (hash: string, testId: string) => request<void>(`/models/${encodeURIComponent(hash)}/test/${encodeURIComponent(testId)}`, { method: "DELETE" }),
  overview: (filter?: { startMs: number; endMs: number; modelHashes?: string[]; bucketMs?: number }) => {
    const params = new URLSearchParams();
    if (filter) {
      params.set("start_ms", String(filter.startMs));
      params.set("end_ms", String(filter.endMs));
      if (filter.modelHashes?.length) params.set("model_hashes", JSON.stringify(filter.modelHashes));
      if (filter.bucketMs) params.set("bucket_ms", String(filter.bucketMs));
    }
    const query = params.toString();
    return request<Overview>(`/overview${query ? `?${query}` : ""}`);
  },
  cursorHarness: () => request<CursorHarnessStatus>("/harness/cursor/status"),
  devinSettings: () => request<DevinSettings>("/devin/settings"),
  devinStatus: () => request<DevinStatus>("/devin/status"),
  setDevinSettings: (settings: DevinSettings) => request<DevinSettings>("/devin/settings", { method: "PUT", body: JSON.stringify(settings) }),
  /** Without a path the server finds the installation itself. */
  devinHostStatus: (path?: string) => request<DevinHostPatchStatus>(path ? `/harness/devin/host/status?path=${encodeURIComponent(path)}` : "/harness/devin/host/status"),
  applyDevinHostPatch: (path?: string) => request<DevinHostPatchReceipt>("/harness/devin/host/apply", { method: "POST", body: JSON.stringify(path ? { path } : {}) }),
  restoreDevinHostPatch: (receipt: DevinHostPatchReceipt) => request<{ restored: boolean }>("/harness/devin/host/restore", { method: "POST", body: JSON.stringify({ receipt }) }),
  initializeCursorCa: () => request<CursorHarnessStatus>("/harness/cursor/ca/initialize", { method: "POST" }),
  plugins: () => request<PluginDescriptor[]>("/plugins"),
  pluginOAuthBegin: (pluginId: string, resourceType: string, methodId: string) => request<PluginOAuthBegin>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/add/${encodeURIComponent(methodId)}/begin`, { method: "POST" }),
  pluginOAuthPoll: (sessionId: string, signal?: AbortSignal) => request<PluginOAuthPoll>(`/plugins/oauth/${encodeURIComponent(sessionId)}/poll`, { method: "POST", signal }),
  importPluginResources: (pluginId: string, resourceType: string, files: PluginImportFile[]) => request<PluginImportResult>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/import`, { method: "POST", body: JSON.stringify(files) }),
  refreshPluginResource: (pluginId: string, resourceType: string, resourceId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}/refresh`, { method: "POST" }),
  pluginResourceAction: (pluginId: string, resourceType: string, resourceId: string, actionId: string, input: unknown = {}) => request<PluginResourceActionResult>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}/actions/${encodeURIComponent(actionId)}`, { method: "POST", body: JSON.stringify(input) }),
  deletePluginResource: (pluginId: string, resourceType: string, resourceId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}`, { method: "DELETE" }),
  syncPluginModels: (pluginId: string, providerId: string) => request<{ models: number }>(`/plugins/${encodeURIComponent(pluginId)}/providers/${encodeURIComponent(providerId)}/models/sync`, { method: "POST" }),
  setPluginModelEnabled: (pluginId: string, providerId: string, modelId: string, enabled: boolean) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/providers/${encodeURIComponent(providerId)}/models/enabled`, { method: "PUT", body: JSON.stringify({ modelId, enabled }) }),
  pluginResourceExportUrl: (servicePort: number, pluginId: string, resourceType: string) => `http://127.0.0.1:${servicePort}${API_ROOT}/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/export`,
  removePluginConfiguration: (pluginId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}`, { method: "DELETE" }),
  pluginRuntime: () => request<PluginRuntimeStatus>("/plugins/runtime"),
  initializePluginRuntime: () => request<PluginRuntimeStatus>("/plugins/runtime", { method: "POST" }),
  cancelPluginRuntimeInitialization: () => request<PluginRuntimeStatus>("/plugins/runtime", { method: "DELETE" }),
  openCursorCaInstallTerminal: async (command: string) => {
    if (!packagedDesktop) throw new Error(t("请在桌面应用中打开终端安装 CA"));
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("open_terminal_with_command", { command });
  },
  copyCursorText: async (text: string) => {
    if (!packagedDesktop) throw new Error(t("请在桌面应用中复制到系统剪贴板"));
    const { writeText } = await import("@tauri-apps/plugin-clipboard-manager");
    await writeText(text);
  },
  setCursorEnabled: (enabled: boolean) => request<CursorHarnessStatus>("/harness/cursor/enabled", { method: "PUT", body: JSON.stringify({ enabled }) }),
  calls: () => request<LlmCall[]>("/llm-calls?limit=200"),
  call: (id: string) => request<CallDetail>(`/llm-calls/${encodeURIComponent(id)}`),
  openCallDetails: async (id: string) => {
    const url = new URL(window.location.href);
    url.hash = `/calls/${encodeURIComponent(id)}`;
    await request<void>("/desktop/open-external-url", { method: "POST", body: JSON.stringify({ url: url.toString() }) });
  },
  openExternalUrl: (url: string) => request<void>("/desktop/open-external-url", { method: "POST", body: JSON.stringify({ url }) }),
  observability: () => request<{ detailed: boolean }>("/settings/observability"),
  setObservability: (detailed: boolean) => request<{ detailed: boolean }>("/settings/observability", { method: "PUT", body: JSON.stringify({ detailed }) }),
  ports: () => request<PortSettings>("/settings/ports"),
  setPorts: (settings: PortSettings) => request<PortSettings>("/settings/ports", { method: "PUT", body: JSON.stringify(settings) }),
  statisticsStorage: () => request<StatisticsStorage>("/settings/storage/statistics"),
  clearStatisticsStorage: (scope: StatisticsStorageScope) => request<StatisticsStorage>("/settings/storage/statistics", { method: "DELETE", body: JSON.stringify({ scope }) }),
  proxySettings: () => request<ProxySettings>("/settings/proxy"),
  setProxySettings: (settings: ProxySettingsInput) => request<ProxySettings>("/settings/proxy", { method: "PUT", body: JSON.stringify(settings) }),
  tabSettings: () => request<TabSettings>("/settings/tab"),
  setTabSettings: (settings: TabSettings) => request<TabSettings>("/settings/tab", { method: "PUT", body: JSON.stringify(settings) }),
  desktopSettings: () => request<DesktopSettings>("/settings/desktop"),
  setDesktopSettings: (settings: DesktopSettings) => request<DesktopSettings>("/settings/desktop", { method: "PUT", body: JSON.stringify(settings) }),
  commitSettings: (locale: Locale) => request<CommitSettingsView>("/settings/commit", { headers: { "accept-language": locale } }),
  setCommitSettings: (settings: CommitSettings) => request<CommitSettingsView>("/settings/commit", { method: "PUT", body: JSON.stringify(settings) }),
  pricingSettings: () => request<TokenPricingSettings>("/settings/pricing"),
  setPricingSettings: (settings: TokenPricingSettings) => request<TokenPricingSettings>("/settings/pricing", { method: "PUT", body: JSON.stringify(settings) }),
};
