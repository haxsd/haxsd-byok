import { useSyncExternalStore } from "react";
import { check } from "@tauri-apps/plugin-updater";
import { classifyUpdateFailure } from "./updateFailure";

/**
 * 启动时静默检查一次更新。
 *
 * 以前只有「设置 → 软件更新」里手点才会查（`UpdateCard` 是唯一的调用方），于是
 * 不主动进设置页的用户永远看不到新版本。这里只检查：不下载、不弹窗、不打扰，
 * 有更新时由顶栏状态区给一个小标记，点进设置页的更新卡片。
 *
 * 失败只写控制台：启动时检查失败多半是没网或代理没走通，不该在启动路径上打扰用户；
 * 分类沿用 `updateFailure.ts`，让这条控制台信息能直接看懂是哪一类问题。
 */
type AvailableUpdate = { version: string };

let available: AvailableUpdate | null = null;
const listeners = new Set<() => void>();

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useAvailableUpdate(): AvailableUpdate | null {
  return useSyncExternalStore(subscribe, () => available);
}

export async function checkForUpdatesAtStartup() {
  try {
    const update = await check();
    if (!update) return;
    // 只留版本号，句柄随用随关：真正的下载/安装仍由设置页的更新卡片发起。
    available = { version: update.version };
    listeners.forEach((listener) => listener());
    // 关不掉也只是多占一个资源，不影响"有更新"这个结论，所以失败不升级为错误。
    await update.close().catch(() => {});
  } catch (cause) {
    const failure = classifyUpdateFailure(cause);
    console.warn(`[update] 启动检查更新失败（${failure.kind}）：${failure.raw || "-"}`);
  }
}
