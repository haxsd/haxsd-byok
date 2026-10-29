import type { CodexConfigConflict, CodexStatus } from "../../shared/api";
import type { EntryStatus } from "../../shell/connectionStatus";
import type { StatusTone } from "../../shared/ui/StatusPill";

export type CodexConnectionState = "checking" | "disabled" | "pending-apply" | "taken-over" | "unavailable" | "conflict";

/**
 * 登录态只是账号侧的提示，不参与路由是否接管的判定。
 *
 * 服务端只认有效的 ChatGPT tokens / personal access token；keyring 存储无法从磁盘判断时
 * 按已登录处理。接管只替换模型请求的凭据，不写登录文件；未登录时客户端会停在需要账号的
 * Work 界面并拒绝本地任务，所以界面要提醒"先登录，再接管"。
 */
export type CodexLoginState = "checking" | "signed-in" | "signed-out";

export function codexLoginState(status: CodexStatus | null): CodexLoginState {
  if (!status) return "checking";
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
    default:
      return { state: "signed-out", label: t("ChatGPT 未登录"), tone: "info", title: t("建议先登录再接管；未登录时自动保持 Codex 本地模式，接管后可能找不到登录入口") };
  }
}

/** 未登录时客户端实际停在哪个界面：本地模式可用，Work 模式会被客户端拒绝。 */
export interface CodexClientModeView {
  label: string;
  tone: StatusTone;
  title: string;
}

export function codexClientModeView(status: CodexStatus | null): CodexClientModeView | null {
  if (!status || status.chatgpt_signed_in) return null;
  switch (status.conversation_detail_mode) {
    case "STEPS_COMMANDS":
      return { label: t("本地模式"), tone: "info", title: t("未登录时自动保持。如果 Codex 已打开在 Work/英文界面，请完全退出后重开；只关闭窗口不会生效") };
    case "STEPS_PROSE":
      return { label: t("Work 模式"), tone: "warn", title: t("未登录时客户端会拒绝本地任务；接管会自动切回本地模式") };
    default:
      return null;
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
    steps.push({ key: "login", label: t("先登录 ChatGPT（推荐）"), hint: t("未登录时自动使用 Codex 本地模式，接管后可能找不到登录入口") });
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
  }
}

/** 未检测到 ChatGPT 登录时，应用接管前要多一次确认，避免把未登录当成接管 bug。 */
export function codexApplyNeedsSignInConfirm(status: CodexStatus | null): boolean {
  return status !== null && !status.chatgpt_signed_in;
}

export function codexConnectionView(status: CodexStatus | null): EntryStatus<CodexConnectionState> {
  // 还没读到状态时不能说「未启用」：那是「读到了，而且关着」的结论。
  if (!status) return { state: "checking", tone: "idle", label: t("读取中…") };
  if (status.config_conflict) return { state: "conflict", tone: "bad", label: t("配置冲突") };
  if (status.configured && !status.listening) return { state: "unavailable", tone: "bad", label: t("网关不可用") };
  if (!status.enabled) return { state: "disabled", tone: "idle", label: t("未启用") };
  if (!status.listening) return { state: "unavailable", tone: "bad", label: t("网关不可用") };
  if (!status.configured) return { state: "pending-apply", tone: "warn", label: t("待应用配置") };
  return { state: "taken-over", tone: "ok", label: t("已接管") };
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

export interface CodexActionGate {
  /** 「应用到 Codex」：只有路由被别人占着（`config_conflict`）时才挡住；字段冲突不挡它。 */
  apply: boolean;
  /** 「撤销接管」：有接管凭证（`restorable`）就能点，字段冲突下也照点。 */
  restore: boolean;
}

/**
 * 接入页两个按钮的可用条件。
 *
 * 「撤销接管」以前看 `configured`：受管字段被外部改过时它是假，于是用户既应用不了、
 * 也撤销不了，卡在冲突里出不来。现在看 `restorable`——有没有接管凭证，有就该能撤销。
 */
export function codexActionGate(status: CodexStatus | null, options: { dirty: boolean; enabled: boolean }): CodexActionGate {
  return {
    apply: Boolean(status && status.listening && !status.config_conflict && options.enabled && !options.dirty),
    restore: Boolean(status?.restorable),
  };
}
