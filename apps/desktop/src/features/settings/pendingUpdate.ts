import { useSyncExternalStore } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { message } from "../../shared/ui/message";

/**
 * 上一次应用内升级的期望值与复核结果。
 *
 * Windows 上 `install()` 启动安装器之后就结束本进程，装没装上只能由下一次启动回答；
 * 安装前把"期望升到哪个版本"写进 localStorage，启动时用实际版本比对一次。
 */
const PENDING_UPDATE_KEY = "haxsd-byok.pending-update";

type PendingUpdate = { from: string; to: string };

export type PendingUpdateOutcome = {
  from: string;
  to: string;
  /** false = 重启后版本没变，安装没有生效。 */
  applied: boolean;
};

/** 安装前调用：记录本次期望升到哪个版本。 */
export function rememberPendingUpdate(from: string, to: string) {
  localStorage.setItem(PENDING_UPDATE_KEY, JSON.stringify({ from, to }));
}

/** 安装明确失败时调用：不需要等到下次启动再判定。 */
export function forgetPendingUpdate() {
  localStorage.removeItem(PENDING_UPDATE_KEY);
}

let outcome: PendingUpdateOutcome | null = null;
const listeners = new Set<() => void>();

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function usePendingUpdateOutcome() {
  return useSyncExternalStore(subscribe, () => outcome);
}

/** 启动时核对上一次升级；没有待核对的记录时什么也不做。 */
export async function verifyPendingUpdate() {
  const pending = readPendingUpdate();
  if (!pending) return;
  let actual: string;
  try {
    actual = await getVersion();
  } catch {
    // 浏览器预览等非桌面环境读不到版本：保留记录，留给真正的启动路径判定。
    return;
  }
  forgetPendingUpdate();
  const applied = actual !== pending.from;
  outcome = { ...pending, applied };
  listeners.forEach((listener) => listener());
  if (applied) {
    message(t("上次更新已生效：{from} → {to}", { from: pending.from, to: pending.to }));
  } else {
    message.error(t("上次更新未生效：重启后仍是 {version}", { version: pending.from }), { duration: 10_000 });
  }
}

function readPendingUpdate(): PendingUpdate | null {
  const raw = localStorage.getItem(PENDING_UPDATE_KEY);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as Partial<PendingUpdate>;
    return typeof parsed.from === "string" && typeof parsed.to === "string"
      ? { from: parsed.from, to: parsed.to }
      : null;
  } catch {
    return null;
  }
}
