import type {
  ClaudeCliTakeoverState,
  ClaudeConfigSource,
  ClaudeDesktopTakeover,
  ClaudeDesktopTakeoverState,
  ClaudeSettings,
  ClaudeStatus,
  ClaudeTakeover,
  ClaudeTakeoverConflict,
} from "../../shared/api";
import type { EntryStatus } from "../../shell/connectionStatus";
import type { StatusTone } from "../../shared/ui/StatusPill";

/**
 * Claude 接入页的纯推导。
 *
 * 页面只负责渲染与发请求；"现在通没通、按钮能不能点、接管时会写什么"都从这里算，
 * 这样状态矩阵可以单测，不必启动整个应用。
 */

export type ClaudeConnectionState =
  | "checking"
  | "disabled"
  | "unavailable"
  | "pending-apply"
  | "taken-over"
  | "conflict"
  | "managed"
  | "idle";

/**
 * 顶栏与切换带上的一个状态。
 *
 * 优先级：配置冲突 > 已接管 > 网关未启用 / 未监听 > 受管配置 > 待应用。两端都
 * 没检测到时是"未检测到客户端"，不是"未启用"——后者是读到了网关而且关着的结论。
 */
export function claudeConnectionView(
  status: ClaudeStatus | null,
  takeover: ClaudeTakeover | null,
): EntryStatus<ClaudeConnectionState> {
  if (!status) return { state: "checking", tone: "idle", label: t("读取中…") };
  if (!takeover) return { state: "checking", tone: "idle", label: t("读取中…") };
  const states: (ClaudeCliTakeoverState | ClaudeDesktopTakeoverState)[] = [takeover.cli.state, takeover.desktop.state];
  if (states.includes("conflict")) return { state: "conflict", tone: "bad", label: t("配置冲突") };
  if (states.includes("applied")) return { state: "taken-over", tone: "ok", label: t("已接管") };
  if (!status.enabled) return { state: "disabled", tone: "idle", label: t("未启用") };
  if (!status.listening) return { state: "unavailable", tone: "bad", label: t("网关不可用") };
  if (states.includes("managed")) return { state: "managed", tone: "warn", label: t("受管配置") };
  if (states.includes("ready") || states.includes("running")) {
    return { state: "pending-apply", tone: "warn", label: t("待应用配置") };
  }
  return { state: "idle", tone: "idle", label: t("未检测到客户端") };
}

const takeoverLabels: Record<ClaudeCliTakeoverState | ClaudeDesktopTakeoverState, () => string> = {
  missing: () => t("未检测到"),
  ready: () => t("待接管"),
  applied: () => t("已接管"),
  conflict: () => t("配置冲突"),
  managed: () => t("受管配置"),
  running: () => t("运行中"),
};

export function claudeTakeoverStateLabel(state: ClaudeCliTakeoverState | ClaudeDesktopTakeoverState): string {
  return takeoverLabels[state]();
}

export function claudeTakeoverStateTone(state: ClaudeCliTakeoverState | ClaudeDesktopTakeoverState): StatusTone {
  switch (state) {
    case "applied": return "ok";
    case "conflict": return "bad";
    case "ready":
    case "running":
    case "managed": return "warn";
    default: return "idle";
  }
}

export function claudeConfigSourceLabel(source: ClaudeConfigSource): string {
  switch (source) {
    case "local": return t("本地配置");
    case "user_policy": return t("用户策略");
    case "machine_policy": return t("机器策略");
    default: return t("未检测到");
  }
}

export type ClaudeTakeoverTarget = "cli" | "desktop";

export interface ClaudeActionGate {
  /** 「应用接管」：网关可用、设置已保存、目标客户端处于可写状态。 */
  apply: boolean;
  /** 「撤销接管」：服务端有归属当前安装的接管收据才可点；网关关着也能撤。 */
  undo: boolean;
  /** 按钮为什么点不了；可以点时是 null。 */
  applyBlockedReason: string | null;
  /** 需要先去别的页面处理时给出入口；目前只有模型库。 */
  applyBlockedLink: "/models" | null;
}

export interface ClaudeGateInput {
  settings: ClaudeSettings | null;
  status: ClaudeStatus | null;
  takeover: ClaudeTakeover | null;
  /** Desktop 的模型草稿；CLI 不用。 */
  desktopModels?: string[];
  /** 设置还有未保存的改动：先保存，再应用，避免写入一半旧值。 */
  dirty: boolean;
}

/**
 * 两个 tab 的按钮可用性。
 *
 * 撤销只看服务端的接管收据（`restorable`），不看 enabled/listening，也不从备份文件
 * 推断：网关关掉、配置冲突时，用户仍然要能撤回已经写进客户端的东西。
 */
export function claudeActionGate(target: ClaudeTakeoverTarget, input: ClaudeGateInput): ClaudeActionGate {
  const takeover = input.takeover;
  const entry = target === "cli" ? takeover?.cli : takeover?.desktop;
  const undo = Boolean(entry?.restorable);
  const blocked = (
    reason: string,
    link: "/models" | null = null,
  ): ClaudeActionGate => ({ apply: false, undo, applyBlockedReason: reason, applyBlockedLink: link });

  if (!input.settings || !input.status) return blocked(t("正在读取网关状态…"));
  if (!input.settings.enabled) return blocked(t("先启用并保存 Claude 网关。"));
  if (!input.status.listening) return blocked(t("网关还没有监听；检查端口后保存设置。"));
  if (!input.settings.default_model_hash) return blocked(t("先在网关里选择默认模型。"));
  if (input.dirty) return blocked(t("有未保存的改动，点右上角「保存」"));
  if (!takeover) return blocked(t("正在读取接管状态…"));

  // 服务端告诉我们哪些哈希已经不在模型库里；写进客户端之前必须先换掉。
  const missing = takeover.cli.missing_models;
  if (missing.includes(input.settings.default_model_hash)) {
    return blocked(t("默认模型已不在模型库里，请到模型库重新选择。"), "/models");
  }
  const selected = [
    input.settings.sonnet_model_hash,
    input.settings.opus_model_hash,
    input.settings.haiku_model_hash,
  ].filter((hash): hash is string => Boolean(hash));
  if (selected.some((hash) => missing.includes(hash))) {
    return blocked(t("选中的模型已不在模型库里，请重新选择。"), "/models");
  }

  if (target === "cli") {
    if (takeover.cli.state === "missing") return blocked(t("没有找到 Claude Code 的配置文件，无法接管。"));
    if (takeover.cli.state === "conflict") return blocked(t("Claude Code 的配置存在冲突，先处理下面的条目。"));
    if (takeover.cli.state === "managed") return blocked(t("Claude Code 的配置由系统策略托管，本应用不会改写它。"));
    return { apply: true, undo, applyBlockedReason: null, applyBlockedLink: null };
  }

  if (takeover.desktop.state === "missing") return blocked(t("没有检测到 Claude Desktop。"));
  if (takeover.desktop.state === "running") return blocked(t("Claude Desktop 正在运行，请先完全退出。"));
  if (takeover.desktop.guided || takeover.desktop.state === "managed") {
    return blocked(t("Claude Desktop 需要按引导手工填写，本应用不写入配置。"));
  }
  if (takeover.desktop.state === "conflict") return blocked(t("Claude Desktop 的配置存在冲突，先处理下面的条目。"));
  if ((input.desktopModels ?? []).length === 0) return blocked(t("至少选择一个模型。"));
  return { apply: true, undo, applyBlockedReason: null, applyBlockedLink: null };
}

/** 引导视图只在受管或明确只能手工配置时出现。 */
export function claudeDesktopGuided(takeover: ClaudeDesktopTakeover | null): boolean {
  return Boolean(takeover && (takeover.guided || takeover.state === "managed"));
}

export interface ClaudeConflictGroups {
  /** 会拦住写入的条目。 */
  blocking: ClaudeTakeoverConflict[];
  /** 只提示、不挡路的条目。 */
  hints: ClaudeTakeoverConflict[];
}

/** 冲突与提示分开呈现：两者对用户的含义不同，不能混在一张列表里。 */
export function claudeConflictGroups(conflicts: ClaudeTakeoverConflict[]): ClaudeConflictGroups {
  return {
    blocking: conflicts.filter((conflict) => conflict.blocking),
    hints: conflicts.filter((conflict) => !conflict.blocking),
  };
}

/**
 * 撤销后服务端保留的条目。
 *
 * 撤销只还原"当前值仍等于接管值"的键；用户之后改过的内容会留在客户端里。页面必须
 * 把这些条目显示出来，而不是无条件说"已撤销"。
 */
const keptAfterUndoCodes = new Set([
  "user_modified",
  "env_replaced",
  "env_removed",
  "undo_kept_profile",
  "applied_id_changed",
]);

export function claudeUndoKept(conflicts: ClaudeTakeoverConflict[]): ClaudeTakeoverConflict[] {
  return conflicts.filter((conflict) => keptAfterUndoCodes.has(conflict.code));
}

/** 客户端状态的一句话说明；放在状态卡里，不重复页面用途。 */
export function claudeTakeoverDescription(
  target: ClaudeTakeoverTarget,
  state: ClaudeCliTakeoverState | ClaudeDesktopTakeoverState | null,
): string {
  if (!state) return t("正在读取接管状态…");
  switch (state) {
    case "missing": return target === "cli"
      ? t("没有找到 Claude Code 的配置文件。")
      : t("没有检测到 Claude Desktop。");
    case "ready": return t("配置已就绪，可以应用接管。");
    case "applied": return t("模型请求已走本机网关。");
    case "conflict": return t("配置里已有本应用之外的内容，先处理冲突。");
    case "managed": return t("配置由系统策略托管，本应用不会改写它。");
    case "running": return t("Claude Desktop 正在运行，请先完全退出。");
  }
}
