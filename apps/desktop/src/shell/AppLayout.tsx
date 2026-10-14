import { useEffect, useState } from "react";
import type { IconifyIcon } from "@iconify/react/offline";
import KeepAliveRouteOutlet from "keepalive-for-react-router";
import { NavLink, useLocation, useNavigate } from "react-router-dom";
import cursorIconUrl from "../shared/assets/icons/cursor.svg";
import { PageLayout } from "./layout/PageLayout";
import controls from "../shared/ui/Controls.module.scss";
import { Icon } from "../shared/ui/Icon";
import { StatusPill } from "../shared/ui/StatusPill";
import { TooltipTrigger } from "../shared/ui/TooltipTrigger";
import { navCallsIcon, navDevinIcon, navModelsIcon, navOverviewIcon, navPluginsIcon, navSettingsIcon, navTutorialIcon } from "../shared/ui/navIcons";
import { refreshIcon, searchIcon } from "../shared/ui/icons";
import { appStore, useAppStore } from "../shared/store/appStore";
import { useAvailableUpdate } from "../features/settings/startupUpdateCheck";
import styles from "./AppLayout.module.scss";
import { CommandPalette } from "./CommandPalette";
import { cursorEntryStatus, devinEntryStatus, localServiceStatus } from "./connectionStatus";
import { OfflineBanner } from "./OfflineBanner";
import { PageActionsTarget } from "./PageActions";
import { PageErrorBoundary } from "./PageErrorBoundary";

type MenuItem =
  | { kind: "page"; path: string; label: string; icon: IconifyIcon | string }
  | { kind: "group"; label: string };

const keptAlivePages = ["/", "/calls", "/models", "/settings", "/harness/cursor", "/harness/devin", "/plugins", "/tutorial"];
/** 教程从「外部文档」变成产品内页面后，这个键表示「打开过」，不再表示「读过外部网页」。 */
const tutorialReadStorageKey = "haxsd-byok:tutorial-read";

export function AppLayout() {
  const { busy, cursorHarness, devinStatus, models, offline } = useAppStore();
  const availableUpdate = useAvailableUpdate();
  const location = useLocation();
  const navigate = useNavigate();
  const [leftActionTarget, setLeftActionTarget] = useState<HTMLDivElement | null>(null);
  const [rightActionTarget, setRightActionTarget] = useState<HTMLDivElement | null>(null);
  const [tutorialRead, setTutorialRead] = useState(() => {
    try {
      return localStorage.getItem(tutorialReadStorageKey) === "true";
    } catch {
      return false;
    }
  });

  // 窗口不在前台时给 <html> 打上 data-idle：全局样式据此暂停动画（见 styles/globals.scss）。
  // 用失焦 + 可见性两个信号：只按可见性判断的话，被其他窗口盖住但仍"可见"的窗口会照旧空转。
  useEffect(() => {
    const sync = () => {
      document.documentElement.toggleAttribute("data-idle", document.hidden || !document.hasFocus());
    };
    sync();
    window.addEventListener("focus", sync);
    window.addEventListener("blur", sync);
    document.addEventListener("visibilitychange", sync);
    return () => {
      window.removeEventListener("focus", sync);
      window.removeEventListener("blur", sync);
      document.removeEventListener("visibilitychange", sync);
      document.documentElement.removeAttribute("data-idle");
    };
  }, []);

  // 客户端状态挂在各自入口上：Cursor 的接管模式和 Devin 的网关状态互不代表对方，
  // 两者也都不是「整个应用」的状态。两个值都来自共享 store 的刷新，只有一个来源。
  const menuStatus = (path: string) => {
    if (path === "/harness/cursor") return cursorEntryStatus(cursorHarness);
    if (path === "/harness/devin") return devinEntryStatus(devinStatus);
    return null;
  };
  // 全局那一颗只说一件事：本地管理服务能不能连上。
  const service = localServiceStatus(offline);

  // Groups stay in the list as separators: a horizontal bar has no room for the
  // section headings a vertical list used to carry.
  const menuItems: MenuItem[] = [
    { kind: "page", path: "/", label: t("概览"), icon: navOverviewIcon },
    { kind: "page", path: "/calls", label: t("调用"), icon: navCallsIcon },
    { kind: "group", label: t("接入模块") },
    { kind: "page", path: "/harness/cursor", label: "Cursor", icon: cursorIconUrl },
    { kind: "page", path: "/harness/devin", label: "Devin", icon: navDevinIcon },
    { kind: "group", label: t("共享") },
    { kind: "page", path: "/models", label: t("模型"), icon: navModelsIcon },
    { kind: "group", label: t("设置") },
    { kind: "page", path: "/plugins", label: t("插件配置"), icon: navPluginsIcon },
    { kind: "page", path: "/settings", label: t("系统设置"), icon: navSettingsIcon },
    { kind: "page", path: "/tutorial", label: t("使用教程"), icon: navTutorialIcon },
  ];

  // 教程就在应用里，所以「未读」的判定是「还没打开过这一页」。
  useEffect(() => {
    if (location.pathname !== "/tutorial" || tutorialRead) return;
    setTutorialRead(true);
    try {
      localStorage.setItem(tutorialReadStorageKey, "true");
    } catch {
      // Read state remains valid for the current session when storage is unavailable.
    }
  }, [location.pathname, tutorialRead]);

  const renderIcon = (icon: IconifyIcon | string) => typeof icon === "string"
    ? <Icon src={icon} size="1.15em" />
    : <Icon icon={icon} size="1.15em" />;

  return <PageLayout className={styles.root}>
    <CommandPalette />
    <OfflineBanner />
    <div className={styles.topBar}>
      <nav className={styles.navigation} aria-label={t("主菜单")}>
        {menuItems.map((item) => item.kind === "group"
          ? <span key={`group-${item.label}`} className={styles.navDivider} aria-hidden="true" />
          : <NavLink key={item.path} to={item.path} end={item.path === "/"} className={styles.navItem}>
            {renderIcon(item.icon)}
            <span>{item.label}</span>
            {item.path === "/tutorial" && !tutorialRead && <span className={styles.navDot} aria-hidden="true" />}
            {(() => {
              const status = menuStatus(item.path);
              if (!status) return null;
              return <span className={styles.navStatus} data-tone={status.tone}>{status.label}</span>;
            })()}
          </NavLink>)}
      </nav>
      {/* 常驻状态只回答「本地管理服务连上了没有」。以前这里显示的是 Devin 网关，
          网关没启用就写「已关闭」——看起来像整个应用被关掉了，而实际上 Cursor
          模块可能正常。客户端自己的状态放在各自入口上（Cursor / Devin 导航项）。
          调用次数不属于这里：它是一个要按范围读的统计结果（首页的「LLM 调用」卡片
          与「调用」页），常驻状态条里的那个累计值既与页面范围口径不一致，也读不出趋势。 */}
      <div className={styles.systemStatus} aria-label={t("运行状态")}>
        <TooltipTrigger label={t("本地管理服务")}>
          <StatusPill tone={service.tone} title={t("本地管理服务")}>{service.label}</StatusPill>
        </TooltipTrigger>
        <span className={styles.statusDivider} aria-hidden="true" />
        <TooltipTrigger label={t("模型库里可用的模型")}>
          <span className={styles.statusFact}><strong>{models.length}</strong>{t("个模型")}</span>
        </TooltipTrigger>
        {/* 启动时静默检查发现的更新：只留一个小标记，点进设置页的更新卡片。
            不是弹窗、也没有自动下载——是否安装由用户在更新卡片上决定。 */}
        {availableUpdate && <>
          <span className={styles.statusDivider} aria-hidden="true" />
          <TooltipTrigger label={t("有新版本 {version} 可以安装", { version: availableUpdate.version })}>
            <button
              type="button"
              className={styles.updateMarker}
              aria-label={t("有新版本 {version} 可以安装", { version: availableUpdate.version })}
              onClick={() => void navigate("/settings?section=about")}
            ><span className={styles.updateMarkerText}>{t("有新版本")}</span></button>
          </TooltipTrigger>
        </>}
        <span className={styles.statusDivider} aria-hidden="true" />
        {/* A keyboard shortcut nobody can see is a shortcut nobody uses. */}
        <TooltipTrigger label={t("搜索或执行命令")}>
          <button
            type="button"
            className={styles.commandButton}
            aria-label={t("搜索或执行命令")}
            onClick={() => window.dispatchEvent(new Event("byok:open-command-palette"))}
          >
            <Icon icon={searchIcon} size="1em" />
            <kbd>Ctrl K</kbd>
          </button>
        </TooltipTrigger>
      </div>
    </div>
    <div className={styles.actionRegion}>
      <div ref={setLeftActionTarget} className={styles.pageActions} />
      {location.pathname !== "/" && <TooltipTrigger label={t("刷新")}><button className={controls.iconButton} aria-label={t("刷新")} disabled={busy} onClick={() => void appStore.refresh()}>
        <Icon className={busy ? controls.spin : ""} icon={refreshIcon} size="1.1em" />
      </button></TooltipTrigger>}
      <div ref={setRightActionTarget} className={styles.pageActions} />
    </div>
    <main className={styles.content}>
      <PageActionsTarget.Provider value={{ left: leftActionTarget, right: rightActionTarget }}>
        <PageErrorBoundary resetKey={location.pathname}>
          <KeepAliveRouteOutlet
            activeCacheKey={location.pathname}
            include={keptAlivePages}
            max={keptAlivePages.length}
            enableActivity
            containerClassName={styles.keepAliveContainer}
            cacheNodeClassName={styles.keepAlivePage}
          />
        </PageErrorBoundary>
      </PageActionsTarget.Provider>
    </main>
  </PageLayout>;
}
