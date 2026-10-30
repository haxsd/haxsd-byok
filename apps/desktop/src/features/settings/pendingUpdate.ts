import { useSyncExternalStore } from "react";
import { getVersion } from "@tauri-apps/api/app";
import type { PendingUpdateRecord } from "../../shared/api";
import { message } from "../../shared/ui/message";
import { getUiPreferences, loadUiPreferences, saveUiPreferences } from "../../shared/store/uiPreferences";

/**
 * 上一次应用内升级的期望值与复核结果。
 *
 * Windows 上 `install()` 启动安装器之后就结束本进程，装没装上只能由下一次启动回答；
 * 安装前把"期望升到哪个版本"记下来，启动时用实际版本比对一次。
 *
 * 这条记录也存在服务端（原来在 localStorage，而那份值绑在管理端口 origin 上：端口一变，
 * "装没装上"的判定就永远看不到记录了）。安装器一启动进程就没了，所以调用方必须先等
 * PUT 确认，再启动安装器；只写内存快照无法留给下一次启动。
 */

export type PendingUpdateOutcome = {
  from: string;
  to: string;
  actual: string;
  /** false = 重启后版本没变，安装没有生效。 */
  applied: boolean;
};

const PENDING_WRITE_TIMEOUT_MS = 2_000;

function savePendingUpdate(value: PendingUpdateRecord | null) {
  // 安装器启动前不能让一次本地 PUT 的 60 秒写超时拖住升级；服务快时仍会先完成，
  // 服务慢时最多等 2 秒后照常启动安装器（内存里的快照也保留本次判定）。
  return Promise.race([
    saveUiPreferences({ pendingUpdate: value }),
    new Promise<boolean>((resolve) => {
      globalThis.setTimeout(() => resolve(false), PENDING_WRITE_TIMEOUT_MS);
    }),
  ]);
}

/** 安装前调用：记录本次期望升到哪个版本。 */
export function rememberPendingUpdate(from: string, to: string) {
  return savePendingUpdate({ from, to });
}

/** 安装明确失败时调用：不需要等到下次启动再判定。 */
export function forgetPendingUpdate() {
  return savePendingUpdate(null);
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
  // 偏好是异步读回来的：先等它，否则刚启动时会看不到待核对的记录。
  try {
    await loadUiPreferences();
  } catch {
    // 服务不可达：这次启动读不到记录，留给下一次。
    return;
  }
  const pending = readPendingUpdate();
  if (!pending) return;
  let actual: string;
  try {
    actual = await getVersion();
  } catch {
    // 浏览器预览等非桌面环境读不到版本：保留记录，留给真正的启动路径判定。
    return;
  }
  await forgetPendingUpdate();
  outcome = pendingUpdateOutcome(pending.from, pending.to, actual);
  listeners.forEach((listener) => listener());
  if (outcome.applied) {
    message(t("上次更新已生效：{from} → {to}", { from: pending.from, to: pending.to }));
  } else if (actual !== pending.from) {
    message.error(t("上次更新目标为 {to}，但重启后是 {version}。", { to: pending.to, version: actual }), { duration: 10_000 });
  } else {
    message.error(t("上次更新未生效：重启后仍是 {version}", { version: actual }), { duration: 10_000 });
  }
}

export function pendingUpdateOutcome(from: string, to: string, actual: string): PendingUpdateOutcome {
  return { from, to, actual, applied: actual === to };
}

function readPendingUpdate(): PendingUpdateRecord | null {
  const pending = getUiPreferences().pendingUpdate;
  return pending && typeof pending.from === "string" && typeof pending.to === "string" ? pending : null;
}
