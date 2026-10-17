import type { CommitPromptLocale, Locale } from "../i18n/runtime";
import { clipConsoleBody, errorBodyMessage, errorBodyNeedsConsole } from "./utils/errorBody";

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
/** 模型来源模式：官方 / 本地 / 混合。 */
export type CursorMode = "official" | "local" | "mixed";
/** 客户端当前用的账号：没有 / 我们注入的本地账号 / 用户自己的账号。 */
export type CursorAccountState = "missing" | "local" | "official";
export interface CursorAccount {
  state: CursorAccountState;
  email: string | null;
}
/** 官方模型目录最近一次抓取的成败；本地模式不使用官方目录。 */
export interface CatalogHealth {
  last_success_ms: number | null;
  last_failure_ms: number | null;
  last_failure: string | null;
}
export interface CursorHarnessStatus {
  platform: string;
  ca: CaState;
  mode: CursorMode;
  configured_models: number;
  enabled_models: number;
  integration: IntegrationState;
  settings_applied: boolean;
  /** Cursor 的代理配置由另一个软件写入：我们不覆盖它，界面要说明这件事。 */
  foreign_configuration: boolean;
  proxy_url: string | null;
  ca_install_command: string | null;
  account: CursorAccount;
  catalog: CatalogHealth;
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
export interface CodexSettings {
  enabled: boolean;
  port: number;
  home: string;
  model_hashes: string[];
  default_model_hash: string | null;
}

/** 一个受管字段与接管快照的差异：`applied`/`current` 是 TOML 值的渲染文本（未设置时是空串）。 */
export interface CodexConfigConflict {
  key: string;
  applied: string;
  current: string;
}

export interface CodexStatus {
  enabled: boolean;
  listening: boolean;
  port: number;
  configured: boolean;
  config_conflict: boolean;
  /** 有接管凭证：撤销接管随时可用，哪怕受管字段被外部改过。 */
  restorable: boolean;
  /** 与接管快照不一致的受管字段；没有差异时是空数组。 */
  conflicts: CodexConfigConflict[];
  home: string;
  calls: number;
  last_error: string | null;
}

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
  restore_receipt: DevinHostPatchReceipt | null;
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

/** 一次后台清理的进度快照（服务端 `ClearProgress`）。 */
export interface ClearProgress {
  /** 已删掉的行数。 */
  deleted_rows: number;
  /** 已删掉的缓存文件数：只有 `web_cache` 作用域会动它，其它作用域一直是 0。 */
  deleted_files: number;
}

export interface StatisticsStorage {
  /** 可清理的观测数据（调用行、请求/响应正文、追踪附件）的估算大小。 */
  bytes: number;
  /** 数据库文件的当前大小：清理不会让它变小，界面要同时说清这两件事。 */
  database_bytes: number;
  call_count: number;
  trace_count: number;
  /** 服务端对话副本的闲置回收窗口（天）：只读，来自服务端的回收常量。 */
  conversation_retention_days: number;
  /**
   * 抓取结果缓存（`<数据目录>/cache/web`）的当前占用。
   *
   * 它不在数据库里，服务端刻意与 `bytes` 分开报：界面也必须分行显示，把两个数字合并会让
   * "清理之后数据库文件没变小"更难解释。只有 `web_cache` 作用域会清它。
   */
  web_cache_bytes: number;
  web_cache_files: number;
  /** 有后台清理在跑时是它此刻的进度；清理是后台分批跑的，界面据此显示"正在清理"并轮询。 */
  clearing?: ClearProgress | null;
}

/**
 * 清理作用域。
 *
 * `web_cache` 只删抓取缓存文件，不碰数据库：它不该被"清理全部统计"顺手带走——用户点哪一个
 * 应该清哪一个。
 */
export type StatisticsStorageScope = "details" | "all" | "web_cache";


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

/** 待核对的升级记录（`{from,to}`），见 features/settings/pendingUpdate.ts。 */
export interface PendingUpdateRecord {
  from: string;
  to: string;
}

/**
 * 界面偏好在服务端的形状（17/P2-1 / 01/P2-3）。
 *
 * 这些字段以前只存在 webview 的 localStorage 里，而 localStorage 按 origin 隔离、origin 里
 * 带着管理端口——端口一变（产品明说的行为）偏好就全读不到了。现在服务端是唯一权威。
 *
 * 语义：字段**缺席** = 这个字段服务端还没有值（旧版本服务端，或从没设置过）；`null` = 显式
 * 清空回默认。PUT 时缺席 = 不改，`null` = 清空，有值 = 设置；`disclosure` 是**按卡片键合并**，
 * 不是整份替换（否则两个卡片先后折叠会互相撤销）。
 */
export interface DesktopUiPreferences {
  /** "midnight" | "default-dark" | "default-light" */
  theme?: string | null;
  /** "system" | "zh-CN" | "en-US" */
  locale?: string | null;
  tutorial_read?: boolean | null;
  pending_update?: PendingUpdateRecord | null;
  devin_host_path?: string | null;
  /** 折叠状态：卡片键 → 是否展开。 */
  disclosure?: Record<string, boolean> | null;
}

export interface DesktopSettings extends DesktopUiPreferences {
  silent_start: boolean;
  show_dock_icon: boolean;
  /** 开发者模式：显示内部标识与协议字段，并让日志记录更详细。默认关。 */
  developer_mode: boolean;
}

/**
 * 桌面设置的一次修订：只提交改动的字段。
 *
 * 服务端在写锁内读回已保存的设置、合并这次修订再写回。前端不得先 GET 整份设置
 * 再整份 PUT：两个开关各自读到的旧快照会把对方刚保存的字段撤销掉。
 */
export type DesktopSettingsPatch = Partial<DesktopSettings>;

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
export interface ModelTokenPricing {
  mode: PricingMode;
  /** null means no estimate is available in this currency. */
  cny: CurrencyPricing | null;
  usd: CurrencyPricing | null;
}

/** 单价按模型的稳定标识保存，缺失条目表示未定价。 */
export interface TokenPricingSettings {
  models: Record<string, ModelTokenPricing>;
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

export interface ModelTokenUsage {
  model_hash: string | null;
  model_id: string;
  display_name: string;
  llm_calls: number;
  input_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  output_tokens: number;
}

export interface OverviewTokenUsageBucket {
  bucket_start_ms: number;
  input_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  output_tokens: number;
  models: ModelTokenUsage[];
}

export interface Overview {
  metrics: OverviewMetrics;
  token_usage_granularity: TokenUsageGranularity;
  token_usage_series: OverviewTokenUsageBucket[];
}

export interface LlmCall {
  client_source: "cursor" | "devin" | "codex" | "internal";
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

/**
 * 服务端列表里的坏行（null、缺标识字段）在这里丢掉，并留一条控制台记录。
 *
 * 这些数组会直接进界面外壳（命令面板、顶栏计数），以前一条坏数据会一路抛到渲染，
 * 把整棵组件树卸掉——窗口只剩空白，也没有恢复入口（实测：`[null]` 的模型列表就能
 * 让整个窗口白屏）。宁可少显示一行，也不要整屏崩掉。
 *
 * 只校验标识字段（以及调用方点名要求的数组字段）：其余字段由界面按"读不到就当空"的方式
 * 处理（见调用页的搜索、模型库的筛选）。控制台信息只给开发者看，不参与翻译。
 */
function usableRows<T>(payload: unknown, identityField: string, label: string, requiredArrays: readonly string[] = []): T[] {
  if (!Array.isArray(payload)) {
    console.warn(`[api] ${label}：返回的不是数组，已按空列表处理`);
    return [];
  }
  const rows = payload.filter((row): row is Record<string, unknown> => {
    if (typeof row !== "object" || row === null) return false;
    if (typeof (row as Record<string, unknown>)[identityField] !== "string") return false;
    return requiredArrays.every((field) => Array.isArray((row as Record<string, unknown>)[field]));
  });
  const dropped = payload.length - rows.length;
  if (dropped > 0) console.warn(`[api] ${label}：丢弃了 ${dropped} 条结构不完整的数据`);
  return rows as T[];
}

/** 单个对象的接口：结构不对时给一句用户能读懂的失败，而不是把渲染带崩。 */
function requireRow<T>(payload: unknown, identityField: string, label: string): T {
  if (typeof payload !== "object" || payload === null || typeof (payload as Record<string, unknown>)[identityField] !== "string") {
    console.warn(`[api] ${label}：返回的数据不完整`);
    // 写入类接口：失败的说法沿用产品里已有的那一句（保存失败），具体原因在控制台。
    throw new Error(t("保存失败"));
  }
  return payload as T;
}

/** 读取类接口拿到坏结构时的说法：读是读到了，但内容不能用。 */
function unusableRead(label: string): Error {
  console.warn(`[api] ${label}：返回的数据不完整`);
  return new Error(t("读取失败"));
}

/**
 * 状态类接口（Devin / Codex）：`listening` 这类字段缺失时，界面会把"没在监听"当成事实显示，
 * 比报错更糟——用户会照着这个假状态去改配置。缺字段时按一次读取失败处理。
 */
function requireStatus<T>(payload: T, label: string): T {
  const status = payload as unknown as Record<string, unknown> | null;
  if (
    typeof status !== "object" ||
    status === null ||
    typeof status.listening !== "boolean" ||
    typeof status.calls !== "number"
  ) {
    throw unusableRead(label);
  }
  return payload as T;
}

/**
 * Codex 状态里的可用部分。
 *
 * `restorable` 与 `conflicts` 是后加的字段：缺少 `conflicts` 时按空数组处理——冲突卡片要
 * 遍历它，`undefined` 会在渲染里抛错。坏条目（缺 key 或某个值不是字符串）丢掉并留一条
 * 控制台记录，和列表接口的处置一致。缺 `restorable` 时退回旧口径（`configured`），
 * 撤销按钮对旧服务端的行为不变。
 */
function usableCodexStatus(payload: unknown): CodexStatus {
  const status = requireStatus(payload as CodexStatus, "Codex 状态");
  const raw = Array.isArray(status.conflicts) ? status.conflicts as unknown[] : [];
  const conflicts = raw.filter(
    (conflict): conflict is CodexConfigConflict => {
      if (typeof conflict !== "object" || conflict === null) return false;
      const candidate = conflict as Record<string, unknown>;
      return typeof candidate.key === "string"
        && typeof candidate.applied === "string"
        && typeof candidate.current === "string";
    },
  );
  if (conflicts.length !== raw.length) {
    console.warn(`[api] Codex 状态：丢弃了 ${raw.length - conflicts.length} 条结构不完整的冲突记录`);
  }
  return {
    ...status,
    conflicts,
    restorable: typeof status.restorable === "boolean" ? status.restorable : status.configured,
  };
}

/**
 * 设置类接口：对象本身（以及调用方点名的数组字段）结构不对时给一句可读失败。
 *
 * `/devin/settings` 缺 `bindings` 时，Devin 页会在渲染里抛错——页面级边界会兜住，但用户
 * 只会看到"这个页面出错了"，看不出是数据坏了；这里提前拦住，读失败就说读失败，写入的
 * 回包坏了就说保存失败（沿用既有写法）。
 */
function usableSettings<T>(payload: unknown, requiredArrays: readonly string[], label: string, write: boolean): T {
  const settings = payload as Record<string, unknown> | null;
  const broken =
    typeof settings !== "object" ||
    settings === null ||
    requiredArrays.some((field) => !Array.isArray(settings[field]));
  if (broken) {
    console.warn(`[api] ${label}：返回的数据不完整`);
    throw new Error(write ? t("保存失败") : t("读取失败"));
  }
  return payload as T;
}

/** Codex 设置的可用部分：`model_hashes` 里非字符串的条目丢掉（渲染只看字符串）。 */
function usableCodexSettings(payload: unknown, write: boolean): CodexSettings {
  const settings = usableSettings<CodexSettings>(payload, ["model_hashes"], "Codex 设置", write);
  const hashes = settings.model_hashes.filter((hash): hash is string => typeof hash === "string");
  if (hashes.length !== settings.model_hashes.length) {
    console.warn(`[api] Codex 设置：丢弃了 ${settings.model_hashes.length - hashes.length} 个非字符串模型标识`);
  }
  return { ...settings, model_hashes: hashes };
}

/** Devin 设置的可用部分：坏绑定与坏路由丢掉，缺字段交给界面按"读不到就当空"处理。 */
function usableDevinSettings(payload: unknown, write: boolean): DevinSettings {
  const settings = usableSettings<DevinSettings>(payload, ["bindings"], "Devin 设置", write);
  const bindings = settings.bindings
    .filter((binding): binding is DevinModelBinding => {
      if (typeof binding !== "object" || binding === null) return false;
      return Array.isArray((binding as DevinModelBinding).routes);
    })
    .map((binding) => ({
      ...binding,
      routes: binding.routes.filter((route): route is DevinRoute => (
        typeof route === "object" && route !== null && typeof route.route_id === "string"
      )),
    }));
  if (bindings.length !== settings.bindings.length) {
    console.warn(`[api] Devin 设置：丢弃了 ${settings.bindings.length - bindings.length} 条结构不完整的映射`);
  }
  return { ...settings, bindings };
}

/**
 * 价格设置只由「模型哈希 → 一套价格」组成，语言/币种各一套。
 *
 * `models` 不是对象时按空表处理：那一页只是暂时没有单价，不该让整个设置页出错。
 */
function usablePricing(payload: unknown): TokenPricingSettings {
  const models = (payload as TokenPricingSettings | null)?.models;
  if (typeof models !== "object" || models === null || Array.isArray(models)) {
    console.warn("[api] 价格设置：models 不是对象，已按空表处理");
    return { models: {} };
  }
  return { models };
}

/** 默认请求超时（毫秒）。长任务显式传 `timeoutMs: null` 关闭。 */
const DEFAULT_TIMEOUT_MS = 15000;

/**
 * 写请求的超时（毫秒）。
 *
 * 读和写的超时不是一回事：读超时就是"没读到"，写超时是"服务端可能已经落库了"。15 秒对
 * 一次批量导入、一次宿主补丁（要读写文件、算 SHA-256）根本不够，而且超时会把这次写入
 * 报成失败，诱导用户重复提交。写请求一律给 60 秒（服务端自己的关停预算才 30 秒）。
 * 显式传 `timeoutMs` 仍然优先。
 */
const WRITE_TIMEOUT_MS = 60_000;

/** 这个方法算不算写。GET/HEAD 之外的都按写处理（含 DELETE、PUT、POST、PATCH）。 */
export function requestTimeoutMs(method: string | undefined): number {
  const normalized = (method ?? "GET").toUpperCase();
  return normalized === "GET" || normalized === "HEAD" ? DEFAULT_TIMEOUT_MS : WRITE_TIMEOUT_MS;
}

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
    ? requestTimeoutMs(rest.method as string | undefined)
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
      const message = errorBodyMessage(response.status, response.statusText, body);
      // 被截断或被整段丢掉的正文留一份现场：提示条只放一句能看的话，细节不该消失。
      if (errorBodyNeedsConsole(message, body)) {
        console.warn(`[api] ${response.status} ${path} 的错误正文（提示条用的是截断版）`, clipConsoleBody(body));
      }
      throw new Error(message);
    }
    if (response.status === 204) return undefined as T;
    return response.json() as Promise<T>;
  } finally {
    if (timeoutTimer !== null) clearTimeout(timeoutTimer);
  }
}

export const api = {
  appInfo: () => request<AppInfo>("/app-info"),
  models: (signal?: AbortSignal) => request<unknown>("/models", { signal }).then(rows => usableRows<Model>(rows, "model_hash", "模型列表")),
  createModels: (models: ModelInput[]) => request<unknown>("/models", { method: "POST", body: JSON.stringify({ models }) }).then(rows => usableRows<Model>(rows, "model_hash", "模型库")),
  duplicateModel: (hash: string, displayName: string) => request<unknown>(`/models/${encodeURIComponent(hash)}/duplicate`, { method: "POST", body: JSON.stringify({ display_name: displayName }) }).then(row => requireRow<Model>(row, "model_hash", "复制模型")),
  reorderModels: (modelHashes: string[]) => request<unknown>("/models/order", { method: "PUT", body: JSON.stringify({ model_hashes: modelHashes }) }).then(rows => usableRows<Model>(rows, "model_hash", "模型库")),
  discoverModels: (input: ModelDiscoveryInput) => request<{ models: string[] }>("/models/discover", { method: "POST", body: JSON.stringify(input) }),
  previewV0049Models: () => request<LegacyModelImportPreview>("/models/import-v0049"),
  importV0049Models: () => request<LegacyModelImportResult>("/models/import-v0049", { method: "POST" }),
  updateModel: (hash: string, model: ModelInput) => request<unknown>(`/models/${hash}`, { method: "PUT", body: JSON.stringify(model) }).then(row => requireRow<Model>(row, "model_hash", "更新模型")),
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
    return request<Overview>(`/overview${query ? `?${query}` : ""}`, { signal }).then(report => {
      // 分桶序列与四项计数是首页每一块数据的入口：缺了它们，页面会在 .map 处抛错。
      // 这里当作一次普通的读取失败（保留上一份数据），而不是把坏结构交给渲染。
      if (typeof report !== "object" || report === null
        || !Array.isArray(report.token_usage_series)
        || typeof report.metrics !== "object" || report.metrics === null
        || typeof report.metrics.llm_calls !== "number") {
        throw unusableRead("概览统计");
      }
      return report;
    });
  },
  cursorHarness: (signal?: AbortSignal) => request<CursorHarnessStatus>("/harness/cursor/status", { signal }),
  codexSettings: () => request<unknown>("/codex/settings").then(settings => usableCodexSettings(settings, false)),
  setCodexSettings: (settings: CodexSettings) => request<unknown>("/codex/settings", { method: "PUT", body: JSON.stringify(settings) }).then(saved => usableCodexSettings(saved, true)),
  codexStatus: (signal?: AbortSignal) => request<unknown>("/codex/status", { signal }).then(usableCodexStatus),
  applyCodex: () => request<unknown>("/codex/apply", { method: "POST" }).then(usableCodexStatus),
  restoreCodex: () => request<unknown>("/codex/restore", { method: "POST" }).then(usableCodexStatus),
  devinSettings: () => request<unknown>("/devin/settings").then(settings => usableDevinSettings(settings, false)),
  devinStatus: (signal?: AbortSignal) => request<DevinStatus>("/devin/status", { signal }).then(status => requireStatus(status, "Devin 状态")),
  setDevinSettings: (settings: DevinSettings) => request<unknown>("/devin/settings", { method: "PUT", body: JSON.stringify(settings) }).then(saved => usableDevinSettings(saved, true)),
  /** Without a path the server finds the installation itself. */
  devinHostStatus: (path?: string) => request<DevinHostPatchStatus>(path ? `/harness/devin/host/status?path=${encodeURIComponent(path)}` : "/harness/devin/host/status"),
  /** The identifiers Devin itself asks for, read from the installed client. */
  devinModelUids: (path?: string) => request<DevinModelUids>(path ? `/harness/devin/model-uids?path=${encodeURIComponent(path)}` : "/harness/devin/model-uids"),
  applyDevinHostPatch: (path?: string) => request<DevinHostPatchReceipt>("/harness/devin/host/apply", { method: "POST", body: JSON.stringify(path ? { path } : {}) }),
  restoreDevinHostPatch: (path?: string) => request<{ restored: boolean }>("/harness/devin/host/restore", { method: "POST", body: JSON.stringify(path ? { path } : {}) }),
  initializeCursorCa: () => request<CursorHarnessStatus>("/harness/cursor/ca/initialize", { method: "POST" }),
  plugins: (signal?: AbortSignal) => request<unknown>("/plugins", { signal }).then(rows => usableRows<PluginDescriptor>(
    rows,
    "id",
    "插件列表",
    // providers / resources 是插件信息的两个数组入口：缺了它们，插件列表与模型选择器
    // 都会在 .flatMap 处抛错，所以整条丢掉（控制台留记录）。
    ["providers", "resources"],
  )),
  pluginOAuthBegin: (pluginId: string, resourceType: string, methodId: string) => request<PluginOAuthBegin>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/add/${encodeURIComponent(methodId)}/begin`, { method: "POST" }),
  // OAuth 轮询由调用方按服务端给的间隔反复发起，单次请求不该用默认超时掐断。
  pluginOAuthPoll: (sessionId: string, signal?: AbortSignal) => request<PluginOAuthPoll>(`/plugins/oauth/${encodeURIComponent(sessionId)}/poll`, { method: "POST", signal, timeoutMs: null }),
  importPluginResources: (pluginId: string, resourceType: string, files: PluginImportFile[]) => request<PluginImportResult>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/import`, { method: "POST", body: JSON.stringify(files) }),
  refreshPluginResource: (pluginId: string, resourceType: string, resourceId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}/refresh`, { method: "POST" }),
  pluginResourceAction: (pluginId: string, resourceType: string, resourceId: string, actionId: string, input: unknown = {}) => request<PluginResourceActionResult>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}/actions/${encodeURIComponent(actionId)}`, { method: "POST", body: JSON.stringify(input) }),
  deletePluginResource: (pluginId: string, resourceType: string, resourceId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}`, { method: "DELETE" }),
  syncPluginModels: (pluginId: string, providerId: string) => request<{ models: number }>(`/plugins/${encodeURIComponent(pluginId)}/providers/${encodeURIComponent(providerId)}/models/sync`, { method: "POST" }),
  setPluginModelEnabled: (pluginId: string, providerId: string, modelId: string, enabled: boolean) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/providers/${encodeURIComponent(providerId)}/models/enabled`, { method: "PUT", body: JSON.stringify({ modelId, enabled }) }),
  /**
   * 导出地址必须留在页面真正的 origin 上（`/__byok-api__/` 与页面同源）。
   *
   * 不能用已保存的 `service_port`：那是下次启动的期望端口，改完还没重启时，
   * 浏览器会拿着一个没人监听的端口去导出。
   */
  pluginResourceExportUrl: (pluginId: string, resourceType: string) => `${window.location.origin}${API_ROOT}/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/export`,
  removePluginConfiguration: (pluginId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}`, { method: "DELETE" }),
  pluginRuntime: (signal?: AbortSignal) => request<PluginRuntimeStatus>("/plugins/runtime", { signal }),
  initializePluginRuntime: () => request<PluginRuntimeStatus>("/plugins/runtime", { method: "POST" }),
  cancelPluginRuntimeInitialization: () => request<PluginRuntimeStatus>("/plugins/runtime", { method: "DELETE" }),
  openCursorCaInstallTerminal: async () => {
    if (!packagedDesktop) throw new Error(t("请在桌面应用中打开终端安装 CA"));
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("open_cursor_ca_install_terminal");
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
  setCursorMode: (mode: CursorMode) => request<CursorHarnessStatus>("/harness/cursor/mode", { method: "PUT", body: JSON.stringify({ mode }) }),
  calls: (signal?: AbortSignal) => request<unknown>("/llm-calls?limit=200", { signal }).then(rows => usableRows<LlmCall>(rows, "call_id", "调用记录")),
  /**
   * 一次调用的完整记录。
   *
   * 详情是独立窗口里的整页内容：少一个字段（比如没有 response_chunks）以前会在渲染
   * 时抛错，把窗口变成空白。这里在边界上把形状补齐/校验，页面只需要处理正常数据。
   */
  call: (id: string) => request<CallDetail>(`/llm-calls/${encodeURIComponent(id)}`).then(detail => {
    const call = detail?.call;
    if (typeof call !== "object" || call === null || typeof call.call_id !== "string") {
      throw unusableRead("调用详情");
    }
    return { ...detail, response_chunks: Array.isArray(detail.response_chunks) ? detail.response_chunks : [] };
  }),
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
  setDesktopSettings: (patch: DesktopSettingsPatch) => request<DesktopSettings>("/settings/desktop", { method: "PUT", body: JSON.stringify(patch) }),
  commitSettings: (locale: Locale) => request<CommitSettingsView>("/settings/commit", { headers: { "accept-language": locale } }),
  setCommitSettings: (settings: CommitSettings) => request<CommitSettingsView>("/settings/commit", { method: "PUT", body: JSON.stringify(settings) }),
  pricingSettings: (signal?: AbortSignal) => request<TokenPricingSettings>("/settings/pricing", { signal }).then(usablePricing),
  setModelPricing: (modelHash: string, pricing: ModelTokenPricing) => request<TokenPricingSettings>(`/settings/pricing/${encodeURIComponent(modelHash)}`, { method: "PUT", body: JSON.stringify(pricing) }).then(usablePricing),
  removeModelPricing: (modelHash: string) => request<TokenPricingSettings>(`/settings/pricing/${encodeURIComponent(modelHash)}`, { method: "DELETE" }).then(usablePricing),
};
