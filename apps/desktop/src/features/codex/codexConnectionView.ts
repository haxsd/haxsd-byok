import type { CodexStatus } from "../../shared/api";
import type { EntryStatus } from "../../shell/connectionStatus";

export type CodexConnectionState = "disabled" | "pending-apply" | "taken-over" | "unavailable" | "conflict";

export function codexConnectionView(status: CodexStatus | null): EntryStatus<CodexConnectionState> {
  if (status?.config_conflict) return { state: "conflict", tone: "bad", label: t("配置冲突") };
  if (status?.configured && !status.listening) return { state: "unavailable", tone: "bad", label: t("网关不可用") };
  if (!status?.enabled) return { state: "disabled", tone: "idle", label: t("未启用") };
  if (!status.listening) return { state: "unavailable", tone: "bad", label: t("网关不可用") };
  if (!status.configured) return { state: "pending-apply", tone: "warn", label: t("待应用配置") };
  return { state: "taken-over", tone: "ok", label: t("已接管") };
}
