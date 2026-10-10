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
  /** 凭据只留在服务端：这里只说明"已配置"，明文永远不回传。 */
  api_key_configured: boolean;
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
  /** 新建时必填；编辑时留空表示沿用已保存的凭据。 */
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
  /** 留空并带上 model_hash 时，服务端用已保存的凭据发起发现请求。 */
  api_key: string;
  model_hash?: string;
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
  /** Cursor 的代理配置由另一个软件写入：我们不覆盖它，界面要说明这件事。 */
  foreign_configuration: boolean;
  proxy_url: string | null;
  ca_install_command: string | null;
}

export interface PortSettings {
  proxy_port: number;
  service_port: number;
}

/** 运行副本的自证信息：版本、可执行文件路径、数据目录。 */
export interface AppInfo {
  version: string;
  executable_path: string;
  data_dir: string;
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

/** 选择器里的一个模型：显示用名字，映射用 uid（两者不是同一个字符串）。 */
export interface DevinModelChoice {
  name: string;
  uid: string;
}

/** Devin 客户端自己会请求的模型标识；`byok` 是给第三方模型用的那一类。 */
export interface DevinModelUids {
  path: string;
  choices: DevinModelChoice[];
}

export interface StatisticsStorage {
  bytes: number;
  call_count: number;
  trace_count: number;
}

export type StatisticsStorageScope = "details" | "all";

export type ProxyMode = "default" | "custom" | "direct";

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

/** 出网的运行时状态：设置存的是"该怎么走"，这里回答"此刻实际怎么走"。 */
export interface OutboundStatus {
  /** 系统代理刚被判定连不上，出网临时绕开它直连；兜底窗口到期后自动放回。 */
  system_proxy_bypassed: boolean;
}

export type TabMode = "public" | "direct" | "custom";

export interface TabSettings {
  mode: TabMode;
  address: string;
}

export interface DesktopSettings {
  silent_start: boolean;
  show_dock_icon: boolean;
  /** 开发者模式：显示内部标识与协议字段，并让日志记录更详细。默认关。 */
  developer_mode: boolean;
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

/**
 * 本地管理服务不可达（进程还没起来、重启中、被防火墙挡住）。
 *
 * 单独一个类型是为了让界面能区分「服务没连上」和「服务答了一个错误」：前者要持续
 * 重试并明确告诉用户，后者只是一条失败信息。以前两者都只是弹一条提示，于是服务挂了
 * 的界面看起来像「还没有数据」。
 */
export class ServiceUnreachableError extends Error {
  constructor(options?: ErrorOptions) {
    super(t("无法连接本地管理服务"), options);
    this.name = "ServiceUnreachableError";
  }
}

/**
 * 管理请求超时：本地服务在给定时间内没有回应。
 *
 * 和 ServiceUnreachableError 分开：超时通常说明服务还活着、只是这次太慢，
 * 界面不该把它当成「服务没连上」持续重试，只当作一次普通的模块失败。
 */
export class RequestTimeoutError extends Error {
  constructor(options?: ErrorOptions) {
    super(t("管理请求超时"), options);
    this.name = "RequestTimeoutError";
  }
}

/** 默认请求超时（毫秒）。长任务显式传 `timeoutMs: null` 关闭。 */
const DEFAULT_TIMEOUT_MS = 15000;

/** 请求选项：在标准 RequestInit 之上多一个可选的超时毫秒数。 */
type RequestOptions = RequestInit & { timeoutMs?: number | null };

/** 判断一个错误是否由超时中止产生（原生 AbortSignal.timeout 抛 name 为 TimeoutError 的异常）。 */
function isTimeoutAbort(cause: unknown): boolean {
  if (typeof cause !== "object" || cause === null) return false;
  return (cause as { name?: unknown }).name === "TimeoutError";
}

async function request<T>(path: string, init?: RequestOptions): Promise<T> {
  const { timeoutMs, ...rest } = init ?? {};
  const callerSignal = rest.signal ?? undefined;
  const effectiveTimeoutMs = typeof timeoutMs === "undefined"
    ? DEFAULT_TIMEOUT_MS
    : typeof timeoutMs === "number" && timeoutMs > 0 ? timeoutMs : null;
  const hasTimeout = effectiveTimeoutMs !== null;

  // AbortSignal.timeout / AbortSignal.any 不一定存在：按需探测，缺失时用兜底实现。
  const abortStatics: {
    timeout?: (ms: number) => AbortSignal;
    any?: (signals: AbortSignal[]) => AbortSignal;
  } = typeof AbortSignal !== "undefined"
    ? (AbortSignal as unknown as { timeout?: (ms: number) => AbortSignal; any?: (signals: AbortSignal[]) => AbortSignal })
    : {};
  const nativeTimeout = abortStatics.timeout;
  const nativeAny = abortStatics.any;

  let combinedSignal: AbortSignal | undefined = callerSignal;
  let timedOut = false;
  let timeoutTimer: ReturnType<typeof setTimeout> | null = null;

  if (hasTimeout) {
    if (nativeTimeout && (!callerSignal || nativeAny)) {
      // 优先用平台自带的超时信号；有调用方信号时用 any 合并两者。
      const timeoutSignal = nativeTimeout.call(AbortSignal, effectiveTimeoutMs);
      combinedSignal = callerSignal && nativeAny
        ? nativeAny.call(AbortSignal, [callerSignal, timeoutSignal])
        : timeoutSignal;
    } else {
      // 兜底：AbortController + setTimeout，并把调用方的取消转发进来。
      const controller = new AbortController();
      timeoutTimer = setTimeout(() => {
        timedOut = true;
        controller.abort(new DOMException("timeout", "TimeoutError"));
      }, effectiveTimeoutMs);
      combinedSignal = controller.signal;
      if (callerSignal) {
        if (callerSignal.aborted) controller.abort(callerSignal.reason);
        else callerSignal.addEventListener("abort", () => controller.abort(callerSignal.reason), { once: true });
      }
    }
  }

  const abortedByTimeout = (cause: unknown): boolean =>
    timedOut || isTimeoutAbort(cause) || isTimeoutAbort(combinedSignal?.reason);

  try {
    let response: Response;
    try {
      response = await fetch(`${API_ROOT}${path}`, {
        ...rest,
        signal: combinedSignal,
        headers: rest.body ? { "content-type": "application/json", ...rest.headers } : rest.headers,
      });
    } catch (cause) {
      // 超时和「连不上」必须分开：前者是这次请求太慢，后者是服务没起来。
      if (abortedByTimeout(cause)) throw new RequestTimeoutError({ cause });
      // 调用方主动取消：原样抛出，交给上层判断。
      if (callerSignal?.aborted) throw cause;
      throw new ServiceUnreachableError({ cause });
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
  } finally {
    if (timeoutTimer !== null) clearTimeout(timeoutTimer);
  }
}

export const api = {
  appInfo: () => request<AppInfo>("/app-info"),
  models: (signal?: AbortSignal) => request<Model[]>("/models", { signal }),
  createModels: (models: ModelInput[]) => request<Model[]>("/models", { method: "POST", body: JSON.stringify({ models }) }),
  duplicateModel: (hash: string, displayName: string) => request<Model>(`/models/${encodeURIComponent(hash)}/duplicate`, { method: "POST", body: JSON.stringify({ display_name: displayName }) }),
  reorderModels: (modelHashes: string[]) => request<Model[]>("/models/order", { method: "PUT", body: JSON.stringify({ model_hashes: modelHashes }) }),
  discoverModels: (input: ModelDiscoveryInput) => request<{ models: string[] }>("/models/discover", { method: "POST", body: JSON.stringify(input) }),
  previewV0049Models: () => request<LegacyModelImportPreview>("/models/import-v0049"),
  importV0049Models: () => request<LegacyModelImportResult>("/models/import-v0049", { method: "POST" }),
  updateModel: (hash: string, model: ModelInput) => request<Model>(`/models/${hash}`, { method: "PUT", body: JSON.stringify(model) }),
  deleteModel: (hash: string) => request<void>(`/models/${hash}`, { method: "DELETE" }),
  // 连通性测试可能跑几十秒，关掉默认超时，沿用调用方传进来的取消信号。
  testModel: (hash: string, testId: string, signal?: AbortSignal) => request<ModelConnectivityResult>(`/models/${encodeURIComponent(hash)}/test/${encodeURIComponent(testId)}`, { method: "POST", signal, timeoutMs: null }),
  cancelModelTest: (hash: string, testId: string) => request<void>(`/models/${encodeURIComponent(hash)}/test/${encodeURIComponent(testId)}`, { method: "DELETE" }),
  overview: (filter?: { startMs: number; endMs: number; modelHashes?: string[]; bucketMs?: number }, signal?: AbortSignal) => {
    const params = new URLSearchParams();
    if (filter) {
      params.set("start_ms", String(filter.startMs));
      params.set("end_ms", String(filter.endMs));
      if (filter.modelHashes?.length) params.set("model_hashes", JSON.stringify(filter.modelHashes));
      if (filter.bucketMs) params.set("bucket_ms", String(filter.bucketMs));
    }
    const query = params.toString();
    return request<Overview>(`/overview${query ? `?${query}` : ""}`, { signal });
  },
  cursorHarness: (signal?: AbortSignal) => request<CursorHarnessStatus>("/harness/cursor/status", { signal }),
  devinSettings: () => request<DevinSettings>("/devin/settings"),
  devinStatus: (signal?: AbortSignal) => request<DevinStatus>("/devin/status", { signal }),
  setDevinSettings: (settings: DevinSettings) => request<DevinSettings>("/devin/settings", { method: "PUT", body: JSON.stringify(settings) }),
  /** Without a path the server finds the installation itself. */
  devinHostStatus: (path?: string) => request<DevinHostPatchStatus>(path ? `/harness/devin/host/status?path=${encodeURIComponent(path)}` : "/harness/devin/host/status"),
  /** The identifiers Devin itself asks for, read from the installed client. */
  devinModelUids: (path?: string) => request<DevinModelUids>(path ? `/harness/devin/model-uids?path=${encodeURIComponent(path)}` : "/harness/devin/model-uids"),
  applyDevinHostPatch: (path?: string) => request<DevinHostPatchReceipt>("/harness/devin/host/apply", { method: "POST", body: JSON.stringify(path ? { path } : {}) }),
  restoreDevinHostPatch: (receipt: DevinHostPatchReceipt) => request<{ restored: boolean }>("/harness/devin/host/restore", { method: "POST", body: JSON.stringify({ receipt }) }),
  initializeCursorCa: () => request<CursorHarnessStatus>("/harness/cursor/ca/initialize", { method: "POST" }),
  plugins: (signal?: AbortSignal) => request<PluginDescriptor[]>("/plugins", { signal }),
  pluginOAuthBegin: (pluginId: string, resourceType: string, methodId: string) => request<PluginOAuthBegin>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/add/${encodeURIComponent(methodId)}/begin`, { method: "POST" }),
  // OAuth 轮询由调用方按服务端给的间隔反复发起，单次请求不该用默认超时掐断。
  pluginOAuthPoll: (sessionId: string, signal?: AbortSignal) => request<PluginOAuthPoll>(`/plugins/oauth/${encodeURIComponent(sessionId)}/poll`, { method: "POST", signal, timeoutMs: null }),
  importPluginResources: (pluginId: string, resourceType: string, files: PluginImportFile[]) => request<PluginImportResult>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/import`, { method: "POST", body: JSON.stringify(files) }),
  refreshPluginResource: (pluginId: string, resourceType: string, resourceId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}/refresh`, { method: "POST" }),
  pluginResourceAction: (pluginId: string, resourceType: string, resourceId: string, actionId: string, input: unknown = {}) => request<PluginResourceActionResult>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}/actions/${encodeURIComponent(actionId)}`, { method: "POST", body: JSON.stringify(input) }),
  deletePluginResource: (pluginId: string, resourceType: string, resourceId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}`, { method: "DELETE" }),
  syncPluginModels: (pluginId: string, providerId: string) => request<{ models: number }>(`/plugins/${encodeURIComponent(pluginId)}/providers/${encodeURIComponent(providerId)}/models/sync`, { method: "POST" }),
  setPluginModelEnabled: (pluginId: string, providerId: string, modelId: string, enabled: boolean) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/providers/${encodeURIComponent(providerId)}/models/enabled`, { method: "PUT", body: JSON.stringify({ modelId, enabled }) }),
  pluginResourceExportUrl: (servicePort: number, pluginId: string, resourceType: string) => `http://127.0.0.1:${servicePort}${API_ROOT}/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/export`,
  removePluginConfiguration: (pluginId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}`, { method: "DELETE" }),
  pluginRuntime: (signal?: AbortSignal) => request<PluginRuntimeStatus>("/plugins/runtime", { signal }),
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
  /**
   * 打开日志目录（`<数据目录>\logs`）。
   *
   * 走一个只认这一个路径的窄命令：`tauri-plugin-opener` 的路径权限没有开，
   * 而它的通用 `openPath` 会把"打开文件系统任意路径"的能力交给 webview。
   */
  openLogDirectory: async () => {
    if (!packagedDesktop) throw new Error(t("请在桌面应用中打开日志目录"));
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("open_log_directory");
  },
  setCursorEnabled: (enabled: boolean) => request<CursorHarnessStatus>("/harness/cursor/enabled", { method: "PUT", body: JSON.stringify({ enabled }) }),
  calls: (signal?: AbortSignal) => request<LlmCall[]>("/llm-calls?limit=200", { signal }),
  call: (id: string) => request<CallDetail>(`/llm-calls/${encodeURIComponent(id)}`),
  openCallDetails: async (id: string) => {
    const url = new URL(window.location.href);
    url.hash = `/calls/${encodeURIComponent(id)}`;
    await request<void>("/desktop/open-external-url", { method: "POST", body: JSON.stringify({ url: url.toString() }) });
  },
  openExternalUrl: (url: string) => request<void>("/desktop/open-external-url", { method: "POST", body: JSON.stringify({ url }) }),
  observability: (signal?: AbortSignal) => request<{ detailed: boolean }>("/settings/observability", { signal }),
  setObservability: (detailed: boolean) => request<{ detailed: boolean }>("/settings/observability", { method: "PUT", body: JSON.stringify({ detailed }) }),
  ports: (signal?: AbortSignal) => request<PortSettings>("/settings/ports", { signal }),
  setPorts: (settings: PortSettings) => request<PortSettings>("/settings/ports", { method: "PUT", body: JSON.stringify(settings) }),
  statisticsStorage: () => request<StatisticsStorage>("/settings/storage/statistics"),
  clearStatisticsStorage: (scope: StatisticsStorageScope) => request<StatisticsStorage>("/settings/storage/statistics", { method: "DELETE", body: JSON.stringify({ scope }) }),
  proxySettings: () => request<ProxySettings>("/settings/proxy"),
  setProxySettings: (settings: ProxySettingsInput) => request<ProxySettings>("/settings/proxy", { method: "PUT", body: JSON.stringify(settings) }),
  outboundStatus: () => request<OutboundStatus>("/settings/outbound"),
  tabSettings: () => request<TabSettings>("/settings/tab"),
  setTabSettings: (settings: TabSettings) => request<TabSettings>("/settings/tab", { method: "PUT", body: JSON.stringify(settings) }),
  desktopSettings: (signal?: AbortSignal) => request<DesktopSettings>("/settings/desktop", { signal }),
  setDesktopSettings: (settings: DesktopSettings) => request<DesktopSettings>("/settings/desktop", { method: "PUT", body: JSON.stringify(settings) }),
  commitSettings: (locale: Locale) => request<CommitSettingsView>("/settings/commit", { headers: { "accept-language": locale } }),
  setCommitSettings: (settings: CommitSettings) => request<CommitSettingsView>("/settings/commit", { method: "PUT", body: JSON.stringify(settings) }),
  pricingSettings: (signal?: AbortSignal) => request<TokenPricingSettings>("/settings/pricing", { signal }),
  setPricingSettings: (settings: TokenPricingSettings) => request<TokenPricingSettings>("/settings/pricing", { method: "PUT", body: JSON.stringify(settings) }),
};
