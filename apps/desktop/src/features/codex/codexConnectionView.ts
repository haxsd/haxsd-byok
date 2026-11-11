import type { CodexConfigConflict, CodexManagedConflict, CodexStatus } from "../../shared/api";
import type { EntryStatus } from "../../shell/connectionStatus";
import type { StatusTone } from "../../shared/ui/StatusPill";

export type CodexConnectionState = "checking" | "disabled" | "pending-apply" | "taken-over" | "unavailable" | "conflict" | "managed" | "local-drift" | "policy-unavailable" | "incomplete-update" | "unreadable";

/**
 * 登录态只是账号侧的提示，不参与路由是否接管的判定。
 *
 * 服务端只认有效的 ChatGPT tokens / personal access token；keyring 存储无法从磁盘判断时
 * 按已登录处理。接管只替换模型请求的凭据，不写登录文件；未登录时客户端的 Work 界面可能
 * 拒绝本地任务，所以界面要提醒"先登录，再接管"。
 */
export type CodexLoginState = "checking" | "signed-in" | "signed-out" | "unknown";

export function codexLoginState(status: CodexStatus | null): CodexLoginState {
  if (!status) return "checking";
  // 配置/收据不可读时服务端无法读取 auth.json；false 是未知，不是“未登录”。
  if (status.conflict_kind === "unreadable") return "unknown";
  return status.chatgpt_signed_in ? "signed-in" : "signed-out";
}

/** 登录状态只占一个 pill；详细解释放 title，不再铺成正文。 */
export interface CodexLoginView {
  state: CodexLoginState;
  label: string;
  tone: StatusTone;
  title: string;
}

export function codexLoginView(status: CodexStatus | null): CodexLoginView {
  switch (codexLoginState(status)) {
    case "checking":
      return { state: "checking", label: t("登录检测中…"), tone: "idle", title: t("正在读取 Codex 登录状态") };
    case "signed-in":
      return { state: "signed-in", label: t("ChatGPT 已登录"), tone: "ok", title: t("接管不会退出或替换登录；模型请求仍走本机网关") };
    case "unknown":
      return { state: "unknown", label: t("登录状态未知"), tone: "idle", title: t("Codex 配置无法读取，无法确认 ChatGPT 登录状态") };
    default:
      return { state: "signed-out", label: t("ChatGPT 未登录"), tone: "info", title: t("建议先登录再接管；未登录时 Codex 的 Work 界面可能拒绝本地任务，接管不会替你切换界面") };
  }
}

/** 没接通时最多显示两条下一步，避免把状态卡变成检查清单。 */
export interface CodexNextStep {
  key: string;
  label: string;
  hint: string;
}

export function codexOutstandingSteps(status: CodexStatus | null, selectedModelCount: number, libraryModelCount: number): CodexNextStep[] {
  if (!status || status.configured) return [];
  switch (status.conflict_kind) {
    case "managed":
      return [{ key: "managed", label: t("管理员策略限制"), hint: t("管理员 requirements 覆盖了路由；本机应用无法覆盖，请调整策略或撤销接管") }];
    case "policy_unavailable":
      return [{ key: "policy", label: t("无法读取管理员策略"), hint: t("检查 requirements 文件权限或内容后刷新；撤销接管仍可用") }];
    case "incomplete_update":
      return [{ key: "restore-pending", label: t("先撤销上次未完成的应用"), hint: t("撤销会清理这次可能写入的本机路由，然后可重新应用") }];
    case "local_drift":
      return [{ key: "restore-drift", label: t("撤销后重新应用"), hint: t("Codex 仍指向本机网关，但模型或目录已被改动") }];
    case "unreadable":
      return [{ key: "unreadable", label: t("检查 Codex 配置目录"), hint: t("配置文件或收据无法解析；修复后刷新状态") }];
    default:
      break;
  }
  const steps: CodexNextStep[] = [];
  if (selectedModelCount === 0) {
    steps.push(libraryModelCount === 0
      ? { key: "models", label: t("添加模型"), hint: t("在模型库添加至少一个 API 模型") }
      : { key: "models", label: t("选择模型"), hint: t("在下方选择至少一个可用模型并保存") });
  }
  if (!status.enabled) {
    steps.push({ key: "enable", label: t("启用网关"), hint: t("打开下方「启用 Codex 网关」并保存") });
  } else if (!status.listening) {
    steps.push({ key: "gateway", label: t("启动网关"), hint: t("检查端口是否被占用后保存") });
  } else if (!status.config_conflict) {
    steps.push({ key: "apply", label: t("应用到 Codex"), hint: t("点击右上角「应用到 Codex」") });
  }
  if (!status.chatgpt_signed_in) {
    steps.push({ key: "login", label: t("先登录 ChatGPT（推荐）"), hint: t("未登录时 Work 界面可能无法新建任务；接管不会修改 Codex 的界面模式") });
  }
  return steps.slice(0, 2);
}

/** 状态灯下面只回答“现在通没通”，不重复页面用途。 */
export function codexVerdictDescription(state: CodexConnectionState): string {
  switch (state) {
    case "checking": return t("正在读取 Codex 配置…");
    case "disabled": return t("启用网关后，Codex 的模型请求才会走这里。");
    case "pending-apply": return t("配置已就绪，点击「应用到 Codex」。");
    case "taken-over": return t("模型请求已走本机网关。");
    case "unavailable": return t("本机网关没有在监听。");
    case "conflict": return t("Codex 配置被其他程序占用。");
    case "managed": return t("管理员策略覆盖了 Codex 路由，本机网关无法应用。");
    case "local-drift": return t("Codex 仍指向本机网关，但模型或目录已被改动。");
    case "policy-unavailable": return t("无法读取管理员策略；本地路由状态暂时无法确认。");
    case "incomplete-update": return t("上次应用没有完成；配置可能已写入，也可能没有。");
    case "unreadable": return t("Codex 配置或收据无法解析。");
  }
}

/** 未检测到 ChatGPT 登录时，应用接管前要多一次确认，避免把未登录当成接管 bug。 */
export function codexApplyNeedsSignInConfirm(status: CodexStatus | null): boolean {
  return status !== null && !status.chatgpt_signed_in;
}

export function codexConnectionView(status: CodexStatus | null): EntryStatus<CodexConnectionState> {
  // 还没读到状态时不能说「未启用」：那是「读到了，而且关着」的结论。
  if (!status) return { state: "checking", tone: "idle", label: t("读取中…") };
  switch (status.conflict_kind) {
    case "managed":
      return { state: "managed", tone: "warn", label: t("管理员策略限制") };
    case "local_drift":
      return { state: "local-drift", tone: "warn", label: t("本机路由已变化") };
    case "policy_unavailable":
      return { state: "policy-unavailable", tone: "bad", label: t("无法读取管理员策略") };
    case "incomplete_update":
      return { state: "incomplete-update", tone: "bad", label: t("上次应用未完成") };
    case "unreadable":
      return { state: "unreadable", tone: "bad", label: t("配置读取失败") };
    case "foreign_provider":
      return { state: "conflict", tone: "bad", label: t("配置冲突") };
    default:
      break;
  }
  if (status.config_conflict) return { state: "conflict", tone: "bad", label: t("配置冲突") };
  if (status.configured && !status.listening) return { state: "unavailable", tone: "bad", label: t("网关不可用") };
  if (!status.enabled) return { state: "disabled", tone: "idle", label: t("未启用") };
  if (!status.listening) return { state: "unavailable", tone: "bad", label: t("网关不可用") };
  if (!status.configured) return { state: "pending-apply", tone: "warn", label: t("待应用配置") };
  return { state: "taken-over", tone: "ok", label: t("已接管") };
}

/** Codex 阶段卡片：Codex 自己的状态描述，区分已接入、仍归本机但漂移、策略覆盖等。 */
export interface CodexClientStage {
  detail: string;
  state: "up" | "down" | "unknown";
}

export function codexClientStage(status: CodexStatus | null): CodexClientStage {
  if (!status) return { detail: t("读取中…"), state: "unknown" };
  if (status.configured) return { detail: t("指向本机网关"), state: "up" };
  switch (status.conflict_kind) {
    case "local_drift":
      return { detail: t("已写入但模型/目录已变"), state: "unknown" };
    case "incomplete_update":
      return { detail: t("上次应用未完成"), state: "unknown" };
    case "managed":
      return { detail: t("由管理员策略覆盖"), state: "down" };
    case "policy_unavailable":
      return { detail: t("无法确认管理员策略"), state: "unknown" };
    case "foreign_provider":
      return { detail: t("由其他程序占用"), state: "down" };
    case "unreadable":
      return { detail: t("配置无法解析"), state: "unknown" };
    default:
      return { detail: t("未写入配置"), state: "down" };
  }
}

/**
 * 冲突字段在界面上的名字。未知 key 直接显示原 key：服务端加了新的受管字段时，
 * 用户看到的仍是一个能拿去搜索的字段名，而不是一行空标签。
 */
function conflictFieldLabel(key: string): string {
  switch (key) {
    case "model": return t("模型");
    case "model_provider": return t("模型供应商");
    case "model_catalog_json": return t("模型目录");
    case "model_context_window": return t("上下文窗口");
    case "model_auto_compact_token_limit": return t("自动压缩阈值");
    case "web_search": return t("联网搜索");
    case "model_providers.haxsd-byok": return t("网关配置");
    default: return key;
  }
}

/**
 * 一个 TOML 值的显示文本：字符串值带着引号（`"live"`），去掉外围引号；空串（字段没设置
 * 过）显示「（未设置）」，免得界面上出现「接管时  → 现在 」这样两边都空的一行。
 */
function conflictValueText(value: string): string {
  const bare = value.length >= 2 && value.startsWith("\"") && value.endsWith("\"") ? value.slice(1, -1) : value;
  return bare === "" ? t("（未设置）") : bare;
}

export interface CodexConflictRow {
  /** 服务端的字段名，列表的 key 用它。 */
  key: string;
  /** 字段的中文名；未知 key 是原 key。 */
  field: string;
  /** 接管时写下的值：已去掉引号，未设置时是「（未设置）」。 */
  applied: string;
  /** 现在的值。 */
  current: string;
}

/** 冲突条目转成冲突卡片要显示的一行。 */
export function codexConflictRows(conflicts: CodexConfigConflict[]): CodexConflictRow[] {
  return conflicts.map((conflict) => ({
    key: conflict.key,
    field: conflictFieldLabel(conflict.key),
    applied: conflictValueText(conflict.applied),
    current: conflictValueText(conflict.current),
  }));
}

export interface CodexManagedConflictRow {
  key: string;
  field: string;
  required: string;
  published: string;
}

/** 管理员 requirements 的差异行；值由服务端脱敏，界面不再加工。 */
export function codexManagedConflictRows(conflicts: CodexManagedConflict[]): CodexManagedConflictRow[] {
  return conflicts.map((conflict) => ({
    key: conflict.field,
    field: conflictFieldLabel(conflict.field),
    required: conflict.required,
    published: conflict.published,
  }));
}

export interface CodexActionGate {
  /** 「应用到 Codex」：只有没有配置冲突/策略覆盖/半写状态时才可用；字段漂移不挡它。 */
  apply: boolean;
  /** 「撤销接管」：有接管凭证（`restorable`）就能点，字段冲突下也照点。 */
  restore: boolean;
}

/**
 * 接入页两个按钮的可用条件。
 *
 * 「应用」只允许在 `conflict_kind=none` 时点击：managed/policy_unavailable/
 * incomplete_update/local_drift/foreign_provider 都不能靠再应用一次覆盖。
 * 「撤销」看 `restorable`——有没有接管凭证，有就该能撤销。
 */
export function codexActionGate(status: CodexStatus | null, options: { dirty: boolean; enabled: boolean }): CodexActionGate {
  return {
    apply: Boolean(status && status.listening && status.conflict_kind === "none" && options.enabled && !options.dirty),
    restore: Boolean(status?.restorable),
  };
}
