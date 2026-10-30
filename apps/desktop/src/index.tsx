import ReactDOM from "react-dom/client";
import { I18nRoot } from "./i18n/I18nRoot";
import { initializeI18n } from "./i18n/store";
import { verifyPendingUpdate } from "./features/settings/pendingUpdate";
import { checkForUpdatesAtStartup } from "./features/settings/startupUpdateCheck";
import { appStore } from "./shared/store/appStore";
import { bootstrapUiPreferences } from "./shared/store/bootstrapPreferences";
import { applyTheme } from "./shared/theme/theme";
import "./styles/globals.scss";

initializeI18n();
applyTheme(appStore.getSnapshot().theme);
void appStore.refresh();
// 界面偏好（主题、语言、折叠、教程已读、待核对升级、宿主路径）现在存服务端：启动时读回来，
// 首次会把旧 localStorage 里那份一次性搬上去（见 shared/store/uiPreferences.ts）。
// 管理服务可能比 webview 晚几百毫秒起来；失败后定时重试，语言/主题才不会一直停在默认值，
// 也才能在下一次启动时核对上一次升级。
async function bootPreferences() {
  try {
    await bootstrapUiPreferences();
    void verifyPendingUpdate();
  } catch {
    window.setTimeout(() => void bootPreferences(), 3_000);
  }
}
void bootPreferences();
// 启动时静默查一次更新：有新版只在顶栏留一个小标记，不弹窗、不自动下载。
void checkForUpdatesAtStartup();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <I18nRoot />,
);
