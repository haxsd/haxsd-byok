import type { CodexConfigConflict, CodexStatus } from "../../shared/api";
import type { EntryStatus } from "../../shell/connectionStatus";

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
