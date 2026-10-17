import { applyRemoteLocalePreference } from "../../i18n/store";
import { applyThemePreference } from "./appStore";
import { loadUiPreferences } from "./uiPreferences";

/**
 * 启动时把界面偏好从服务端读回来，并接到各自的落点上。
 *
 * 主题与语言各有自己的 store（一个管 `documentElement.dataset.theme`，一个管全局 `t()`），
 * 其余几项（教程已读、待核对升级、Devin 宿主路径、卡片折叠）直接读 `useUiPreferences()`。
 *
 * 服务不可达时不阻塞启动：界面用默认值继续，离线横幅会说明原因；偏好留到下一次启动再读。
 * 抛错不在这里吞掉——`index.tsx` 的调用点自己决定要不要提示。
 */
export async function bootstrapUiPreferences() {
  const preferences = await loadUiPreferences();
  if (preferences.theme) applyThemePreference(preferences.theme);
  if (preferences.locale) applyRemoteLocalePreference(preferences.locale);
}
