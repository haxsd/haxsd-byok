import ReactDOM from "react-dom/client";
import { I18nRoot } from "./i18n/I18nRoot";
import { initializeI18n } from "./i18n/store";
import { verifyPendingUpdate } from "./features/settings/pendingUpdate";
import { checkForUpdatesAtStartup } from "./features/settings/startupUpdateCheck";
import { appStore } from "./shared/store/appStore";
import { applyTheme } from "./shared/theme/theme";
import "./styles/globals.scss";

initializeI18n();
applyTheme(appStore.getSnapshot().theme);
void appStore.refresh();
// 上一次应用内升级装没装上，只能在下一次启动时用实际版本回答（Windows 上
// 安装器一启动本进程就结束了）。
void verifyPendingUpdate();
// 启动时静默查一次更新：有新版只在顶栏留一个小标记，不弹窗、不自动下载。
void checkForUpdatesAtStartup();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <I18nRoot />,
);
