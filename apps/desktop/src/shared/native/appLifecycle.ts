import { getVersion, setDockVisibility } from "@tauri-apps/api/app";
import { isTauri } from "@tauri-apps/api/core";
import { disable, enable, isEnabled } from "@tauri-apps/plugin-autostart";
import { api, type DesktopSettings } from "../api";
import { desktopPlatform } from "./platform";

export function hasNativeAppLifecycle(): boolean {
  return isTauri();
}

export function hasDockVisibilitySetting(): boolean {
  return hasNativeAppLifecycle() && desktopPlatform() === "macos";
}

export async function currentAppVersion(): Promise<string> {
  return hasNativeAppLifecycle() ? getVersion() : "dev";
}

export async function readAutostart(): Promise<boolean> {
  return isEnabled();
}

export async function writeAutostart(enabled: boolean): Promise<void> {
  await (enabled ? enable() : disable());
}

export async function readDesktopSettings(): Promise<DesktopSettings> {
  return api.desktopSettings();
}

export async function writeSilentStart(silentStart: boolean): Promise<void> {
  await api.setDesktopSettings({ silent_start: silentStart });
}

export async function writeDockIconVisibility(visible: boolean): Promise<void> {
  // Dock 图标先改原生状态，再持久化这一个字段：桌面设置服务端在写锁内合并，
  // 这里不需要（也不允许）先读回整份设置再整份写回。
  await setDockVisibility(visible);
  try {
    await api.setDesktopSettings({ show_dock_icon: visible });
  } catch (cause) {
    // 保存失败时回到服务端仍然保存的值，界面才不会停在一个没有落库的状态。
    // 读不到就把图标放回显示——那也是没有这份设置时的默认。
    const persisted = await api.desktopSettings().catch(() => null);
    await setDockVisibility(persisted?.show_dock_icon ?? true).catch(() => {});
    throw cause;
  }
}

