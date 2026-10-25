import type { CursorHarnessStatus, DevinStatus } from "../shared/api";

/**
 * 顶部常驻状态与各入口状态的推导。
 *
 * 全局那一颗只回答「本地管理服务能不能连上」：离线就是未连接，否则就是已连接。
 * 某个客户端自己没启用（Devin 网关关着、Cursor 处于官方模式）属于该客户端的
 * 入口状态，不影响全局——「整个应用已关闭」不是一种真实状态。
 */

export type StatusTone = "ok" | "warn" | "idle" | "bad";

export interface LocalServiceStatus {
  state: "connected" | "disconnected";
  tone: StatusTone;
  label: string;
}

export function localServiceStatus(offline: boolean): LocalServiceStatus {
  return offline
    ? { state: "disconnected", tone: "bad", label: t("服务未连接") }
    : { state: "connected", tone: "ok", label: t("服务已连接") };
}

export interface EntryStatus<State extends string> {
  state: State;
  tone: StatusTone;
  label: string;
}

export type CursorEntryState = "official" | "taken-over" | "not-taken-over";

export function cursorEntryStatus(status: CursorHarnessStatus | null): EntryStatus<CursorEntryState> | null {
  if (!status) return null;
  if (status.mode === "official") return { state: "official", tone: "idle", label: t("官方") };
  if (status.settings_applied) return { state: "taken-over", tone: "ok", label: t("已接管") };
  return { state: "not-taken-over", tone: "idle", label: t("未接管") };
}

export type DevinEntryState = "running" | "pending-restart" | "disabled-listening" | "disabled";

export function devinEntryStatus(status: DevinStatus | null): EntryStatus<DevinEntryState> | null {
  if (!status) return null;
  if (status.enabled) {
    return status.ready
      ? { state: "running", tone: "ok", label: t("运行中") }
      : { state: "pending-restart", tone: "warn", label: t("待重启") };
  }
  // 已保存关闭、端口却还在听：网关任务是在上一轮运行里起来的，会一直应答到进程退出。
  // 这时报"未启用"会骗人——端口是真的开着，还可能在产生调用记录。
  return status.listening
    ? { state: "disabled-listening", tone: "warn", label: t("已关闭（仍在监听，重启后停止）") }
    : { state: "disabled", tone: "idle", label: t("未启用") };
}
