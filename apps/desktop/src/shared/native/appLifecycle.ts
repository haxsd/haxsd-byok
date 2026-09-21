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
  const settings = await readDesktopSettings();
  await api.setDesktopSettings({ ...settings, silent_start: silentStart });
}

export async function writeDockIconVisibility(visible: boolean): Promise<void> {
  const settings = await readDesktopSettings();
  await setDockVisibility(visible);
  try {
    await api.setDesktopSettings({ ...settings, show_dock_icon: visible });
  } catch (cause) {
    await setDockVisibility(settings.show_dock_icon).catch(() => {});
    throw cause;
  }
}

