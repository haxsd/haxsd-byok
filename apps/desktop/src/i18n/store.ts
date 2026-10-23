import { useSyncExternalStore } from "react";
import { setRuntimeLocale, type Locale, type LocalePreference } from "./runtime";
import { saveUiPreferences } from "../shared/store/uiPreferences";

export type { LocalePreference };

type I18nSnapshot = {
  preference: LocalePreference;
  locale: Locale;
};

const listeners = new Set<() => void>();
let initialized = false;
let snapshot: I18nSnapshot = { preference: "system", locale: "en-US" };

export function resolveSystemLocale(
  languages: readonly string[] = navigator.languages.length
    ? navigator.languages
    : [navigator.language],
): Locale {
  const normalized = languages[0]?.toLowerCase() ?? "";
  if (normalized === "zh" || normalized.startsWith("zh-")) return "zh-CN";
  if (normalized === "ru" || normalized.startsWith("ru-")) return "ru-RU";
  // 其余系统语言统一回退到英文。
  return "en-US";
}

function applyPreference(preference: LocalePreference, notify: boolean) {
  const locale = preference === "system" ? resolveSystemLocale() : preference;
  const changed = snapshot.preference !== preference || snapshot.locale !== locale;
  snapshot = { preference, locale };
  setRuntimeLocale(locale);
  document.documentElement.lang = locale;
  if (notify && changed) listeners.forEach((listener) => listener());
}

export function initializeI18n() {
  if (initialized) return;
  initialized = true;
  // 偏好不再从 localStorage 读（那是绑在管理端口 origin 上的，端口一变就丢）：先按跟随系统
  // 渲染，服务端的值到了之后由 `applyRemoteLocalePreference` 接上。
  applyPreference("system", false);
  window.addEventListener("languagechange", () => {
    if (snapshot.preference === "system") applyPreference("system", true);
  });
}

/**
 * 采用服务端记下的语言偏好。
 *
 * 与 `setLocalePreference` 分开：启动时读回来的值不能再写回去（那是自己写给自己的 PUT）。
 */
export function applyRemoteLocalePreference(preference: LocalePreference) {
  applyPreference(preference, true);
}

export function setLocalePreference(preference: LocalePreference) {
  applyPreference(preference, true);
  // 写回服务端；界面不等它（切换要立刻可见）。失败只影响"下次启动还记不记得"。
  void saveUiPreferences({ locale: preference });
}

export function useI18n(): I18nSnapshot {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => snapshot,
    // 服务端渲染（测试里 renderToStaticMarkup）必须给第三个参数，否则 React 直接抛错。
    () => snapshot,
  );
}
