import { useSyncExternalStore } from "react";
import { api, type DesktopSettings, type DesktopSettingsPatch, type PendingUpdateRecord } from "../api";
import { isThemeId, type ThemeId } from "../theme/theme";
import type { LocalePreference } from "../../i18n/runtime";

/**
 * 界面偏好的服务端持久化（17/P2-1 + 01/P2-3）。
 *
 * 根因：这些偏好以前只存在 webview 的 `localStorage` 里，而 localStorage 按 **origin** 隔离，
 * origin 里带着管理端口。端口一变（产品明说的行为：端口被占用就换一个），新 origin 下读不到
 * 旧值——主题、语言、卡片折叠、教程已读、待核对升级、Devin 宿主路径全部悄悄回到默认。
 *
 * 现在服务端是唯一权威，localStorage 只剩一个用途：**一次性迁移**。启动时读一次服务端：
 * - 服务端已经接管（回答里有任何一个偏好字段，哪怕是 null）→ 用服务端的值，并删掉旧键；
 * - 服务端还没有这些字段（接口未落地 / 旧版本）而旧键有值 → 先用旧值，并把它 PUT 上去，
 *   **只有服务端回显确认收到之后**才删旧键——否则接口没落地时会把用户的偏好弄丢；
 * - 都没有 → 用默认值。
 *
 * 迁移因此只会发生一次：删掉旧键之后，下一次启动读到的只有服务端。
 *
 * 服务端字段语义见 api.ts 的 `DesktopUiPreferences`（缺席 = 没有值，null = 清空）。
 */

/** 旧版存在 webview localStorage 里的键。只在迁移时读一次，之后删除。 */
export const LEGACY_THEME_KEY = "haxsd-byok.theme";
export const LEGACY_LOCALE_KEY = "haxsd-byok.locale";
export const LEGACY_TUTORIAL_KEY = "haxsd-byok:tutorial-read";
export const LEGACY_PENDING_UPDATE_KEY = "haxsd-byok.pending-update";
export const LEGACY_DEVIN_HOST_PATH_KEY = "haxsd-byok.devin.host-path";
export const LEGACY_DISCLOSURE_PREFIX = "haxsd-byok:disclosure:";

export const LEGACY_KEYS = [
  LEGACY_THEME_KEY,
  LEGACY_LOCALE_KEY,
  LEGACY_TUTORIAL_KEY,
  LEGACY_PENDING_UPDATE_KEY,
  LEGACY_DEVIN_HOST_PATH_KEY,
] as const;

/** 界面偏好的界面侧形状。字段缺席 = 没设置过（不是"设成了 false"）。 */
export type UiPreferences = {
  theme?: ThemeId;
  locale?: LocalePreference;
  tutorialRead?: boolean;
  pendingUpdate?: PendingUpdateRecord | null;
  devinHostPath?: string;
  disclosure?: Record<string, boolean>;
};

/** localStorage 的最小接口；`length`/`key` 缺失时无法枚举折叠状态的键（真实浏览器都有）。 */
export type LegacyStorage = Pick<Storage, "getItem" | "removeItem"> & Partial<Pick<Storage, "key" | "length">>;

/** 服务端回答里与界面偏好有关的字段。 */
const PREFERENCE_FIELDS = ["theme", "locale", "tutorial_read", "pending_update", "devin_host_path", "disclosure"] as const;

export function isLocalePreference(value: string | null | undefined): value is LocalePreference {
  return value === "system" || value === "zh-CN" || value === "en-US";
}

function activeStorage(): LegacyStorage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    // 存储被策略拦下时连读都可能抛。
    return null;
  }
}

function readKey(store: LegacyStorage, key: string): string | null {
  try {
    return store.getItem(key);
  } catch {
    return null;
  }
}

function removeKey(store: LegacyStorage, key: string) {
  try {
    store.removeItem(key);
  } catch {
    // 删不掉就留着：下次启动会再迁移一遍，幂等。
  }
}

export function parsePendingUpdate(raw: string | null): PendingUpdateRecord | null {
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as Partial<PendingUpdateRecord>;
    return typeof parsed.from === "string" && typeof parsed.to === "string" ? { from: parsed.from, to: parsed.to } : null;
  } catch {
    return null;
  }
}

function readLegacyDisclosure(store: LegacyStorage): Record<string, boolean> | undefined {
  if (typeof store.length !== "number" || typeof store.key !== "function") return undefined;
  const states: Record<string, boolean> = {};
  for (let index = 0; index < store.length; index += 1) {
    const key = store.key(index);
    if (typeof key !== "string" || !key.startsWith(LEGACY_DISCLOSURE_PREFIX)) continue;
    const value = readKey(store, key);
    if (value !== null) states[key.slice(LEGACY_DISCLOSURE_PREFIX.length)] = value === "open";
  }
  return states;
}

/** 旧 localStorage 里的偏好（迁移来源）。只在启动时调用一次。 */
export function readLegacyPreferences(store: LegacyStorage | null = activeStorage()): UiPreferences {
  if (!store) return {};
  const preferences: UiPreferences = {};
  const theme = readKey(store, LEGACY_THEME_KEY);
  if (isThemeId(theme)) preferences.theme = theme;
  const locale = readKey(store, LEGACY_LOCALE_KEY);
  if (isLocalePreference(locale)) preferences.locale = locale;
  const tutorialRead = readKey(store, LEGACY_TUTORIAL_KEY);
  if (tutorialRead !== null) preferences.tutorialRead = tutorialRead === "true";
  const pendingUpdate = parsePendingUpdate(readKey(store, LEGACY_PENDING_UPDATE_KEY));
  if (pendingUpdate) preferences.pendingUpdate = pendingUpdate;
  const hostPath = readKey(store, LEGACY_DEVIN_HOST_PATH_KEY);
  if (hostPath) preferences.devinHostPath = hostPath;
  const disclosure = readLegacyDisclosure(store);
  if (disclosure && Object.keys(disclosure).length > 0) preferences.disclosure = disclosure;
  return preferences;
}

/** 删掉旧键（服务端已经记住这些值之后才调用）。 */
export function clearLegacyPreferences(store: LegacyStorage | null = activeStorage()) {
  if (!store) return;
  for (const key of LEGACY_KEYS) removeKey(store, key);
  if (typeof store.length !== "number" || typeof store.key !== "function") return;
  const disclosureKeys: string[] = [];
  for (let index = 0; index < store.length; index += 1) {
    const key = store.key(index);
    if (typeof key === "string" && key.startsWith(LEGACY_DISCLOSURE_PREFIX)) disclosureKeys.push(key);
  }
  for (const key of disclosureKeys) removeKey(store, key);
}

/** 服务端回答里的偏好 → 界面侧形状；认不出的值按"没有"处理，不猜。 */
export function preferencesFromSettings(settings: DesktopSettings | null | undefined): UiPreferences {
  if (!settings) return {};
  const preferences: UiPreferences = {};
  if (isThemeId(settings.theme ?? null)) preferences.theme = settings.theme as ThemeId;
  if (isLocalePreference(settings.locale)) preferences.locale = settings.locale as LocalePreference;
  if (typeof settings.tutorial_read === "boolean") preferences.tutorialRead = settings.tutorial_read;
  if (settings.pending_update
    && typeof settings.pending_update.from === "string"
    && typeof settings.pending_update.to === "string") {
    preferences.pendingUpdate = { from: settings.pending_update.from, to: settings.pending_update.to };
  }
  if (typeof settings.devin_host_path === "string" && settings.devin_host_path) preferences.devinHostPath = settings.devin_host_path;
  if (settings.disclosure && typeof settings.disclosure === "object") {
    const states: Record<string, boolean> = {};
    for (const [key, value] of Object.entries(settings.disclosure)) {
      if (typeof value === "boolean") states[key] = value;
    }
    if (Object.keys(states).length > 0) preferences.disclosure = states;
  }
  return preferences;
}

/**
 * 服务端是否已经接管这些偏好。
 *
 * 判据是**回答里有没有这些字段**，而不是"值是不是空的"：接口落地后字段总是会出现（没设置
 * 过就是 null），而已出现 null 说明用户显式清空过——那就更不该拿旧 localStorage 的值去覆盖。
 */
export function serverOwnsPreferences(settings: DesktopSettings | null | undefined): boolean {
  if (!settings) return false;
  return PREFERENCE_FIELDS.some((field) => Object.prototype.hasOwnProperty.call(settings, field));
}

export function hasAnyPreference(preferences: UiPreferences): boolean {
  return Object.values(preferences).some((value) => value !== undefined && value !== null && !(typeof value === "object" && Object.keys(value).length === 0));
}

/** 界面侧形状 → 服务端 patch（字段名与清空语义都在这一个地方对）。 */
export function preferencePatch(preferences: UiPreferences): DesktopSettingsPatch {
  const patch: DesktopSettingsPatch = {};
  if (preferences.theme !== undefined) patch.theme = preferences.theme;
  if (preferences.locale !== undefined) patch.locale = preferences.locale;
  if (preferences.tutorialRead !== undefined) patch.tutorial_read = preferences.tutorialRead;
  if (preferences.pendingUpdate !== undefined) patch.pending_update = preferences.pendingUpdate;
  if (preferences.devinHostPath !== undefined) patch.devin_host_path = preferences.devinHostPath;
  if (preferences.disclosure !== undefined) patch.disclosure = preferences.disclosure;
  return patch;
}

/**
 * 服务端回显里是否真的收到了这次要写的每一个值。
 *
 * 接口没落地时未知字段会被静默忽略，PUT 照样 200——不核对回显就删旧键，等于把偏好丢了。
 * 折叠状态按**上传的那几个键**核对（其它卡片的值不参与）。
 */
export function coversPreferences(echoed: UiPreferences, written: UiPreferences): boolean {
  for (const [key, value] of Object.entries(written)) {
    if (key === "disclosure") {
      const echo = echoed.disclosure ?? {};
      const sent = (value ?? {}) as Record<string, boolean>;
      if (!Object.entries(sent).every(([card, open]) => echo[card] === open)) return false;
      continue;
    }
    if (JSON.stringify((echoed as Record<string, unknown>)[key]) !== JSON.stringify(value)) return false;
  }
  return true;
}

export type PreferenceBootPlan = {
  /** 界面该采用的值：服务端优先，服务端还没有这些字段时才用旧值。 */
  preferences: UiPreferences;
  /** 需要搬上去的旧值（没有要搬的就是 null）。 */
  upload: UiPreferences | null;
  /** 旧键是否已经没用了（服务端接管了），可以在启动时删掉。 */
  clearLegacy: boolean;
};

export function planPreferenceBoot(
  settings: DesktopSettings | null | undefined,
  legacy: UiPreferences,
): PreferenceBootPlan {
  const server = preferencesFromSettings(settings);
  if (serverOwnsPreferences(settings)) {
    return { preferences: server, upload: null, clearLegacy: true };
  }
  if (hasAnyPreference(legacy)) {
    return { preferences: legacy, upload: legacy, clearLegacy: false };
  }
  return { preferences: {}, upload: null, clearLegacy: false };
}

let snapshot: UiPreferences = {};
let loading: Promise<UiPreferences> | null = null;
const listeners = new Set<() => void>();

function publish(next: UiPreferences) {
  snapshot = next;
  listeners.forEach((listener) => listener());
}

export function getUiPreferences(): UiPreferences {
  return snapshot;
}

export function useUiPreferences(): UiPreferences {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => snapshot,
    // 服务端渲染（测试里用 renderToStaticMarkup 画卡片）必须给第三个参数，否则 React 直接抛错。
    () => snapshot,
  );
}

async function runBoot(): Promise<UiPreferences> {
  const settings = await api.desktopSettings();
  const plan = planPreferenceBoot(settings, readLegacyPreferences());
  if (plan.upload) {
    const echoed = preferencesFromSettings(await api.setDesktopSettings(preferencePatch(plan.upload)));
    if (coversPreferences(echoed, plan.upload)) clearLegacyPreferences();
  } else if (plan.clearLegacy) {
    clearLegacyPreferences();
  }
  publish(plan.preferences);
  return plan.preferences;
}

/**
 * 启动时把偏好读回来；同一次启动里多次调用只会发一次请求（`verifyPendingUpdate` 也要等它）。
 * 抛错交给调用方：服务不可达时界面用默认值继续，离线横幅会说明原因。
 */
export function loadUiPreferences(): Promise<UiPreferences> {
  loading ??= runBoot();
  return loading;
}

/** 改一项偏好：立刻生效并广播，同时写回服务端。返回服务端是否确认收到。 */
export async function saveUiPreferences(patch: UiPreferences): Promise<boolean> {
  publish({ ...snapshot, ...patch });
  try {
    const echoed = preferencesFromSettings(await api.setDesktopSettings(preferencePatch(patch)));
    return coversPreferences(echoed, patch);
  } catch {
    return false;
  }
}

/** 测试与登出式的重置用：清掉模块里的记忆。 */
export function resetUiPreferencesForTest() {
  snapshot = {};
  loading = null;
}
