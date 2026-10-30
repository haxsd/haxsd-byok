import { useEffect, useRef, useState } from "react";
import type { IconifyIcon } from "@iconify/react/offline";
import KeepAliveRouteOutlet from "keepalive-for-react-router";
import { NavLink, useLocation, useNavigate } from "react-router-dom";
import { PageLayout } from "./layout/PageLayout";
import controls from "../shared/ui/Controls.module.scss";
import { Icon } from "../shared/ui/Icon";
import { StatusPill } from "../shared/ui/StatusPill";
import { TooltipTrigger } from "../shared/ui/TooltipTrigger";
import { navCallsIcon, navHarnessIcon, navModelsIcon, navOverviewIcon, navPluginsIcon, navSettingsIcon, navTutorialIcon } from "../shared/ui/navIcons";
import { refreshIcon, searchIcon } from "../shared/ui/icons";
import { appStore, useAppStore } from "../shared/store/appStore";
import { saveUiPreferences, useUiPreferences } from "../shared/store/uiPreferences";
import { MessageProvider } from "../shared/ui/MessageProvider";
import { useMessage } from "../shared/ui/message";
import { useAvailableUpdate } from "../features/settings/startupUpdateCheck";
import styles from "./AppLayout.module.scss";
import { CommandPalette } from "./CommandPalette";
import { localServiceStatus } from "./connectionStatus";
import { harnessClientAt, harnessClients, harnessEntryStatus } from "../features/harness/harnessClients";
import { HarnessTabs } from "../features/harness/HarnessTabs";
import { OfflineBanner } from "./OfflineBanner";
import { PageActionsTarget } from "./PageActions";
import { PageCacheErrorBoundary, PageErrorBoundary } from "./PageErrorBoundary";

type MenuItem =
  | { kind: "page"; path: string; label: string; icon: IconifyIcon | string; status?: { tone: string; label: string } }
  | { kind: "group"; label: string };

const keptAlivePages = ["/", "/calls", "/models", "/settings", "/harness/cursor", "/harness/devin", "/harness/codex", "/harness/claude", "/plugins", "/tutorial"];
/** 教程从「外部文档」变成产品内页面后，这个标记表示「打开过」。存在服务端，见 uiPreferences.ts。 */

export function AppLayout() {
  const { busy, cursorHarness, devinStatus, codexStatus, claudeStatus, claudeTakeover, models, offline, error } = useAppStore();
  const availableUpdate = useAvailableUpdate();
  const location = useLocation();
  const navigate = useNavigate();
  const [leftActionTarget, setLeftActionTarget] = useState<HTMLDivElement | null>(null);
  const [rightActionTarget, setRightActionTarget] = useState<HTMLDivElement | null>(null);
  const [actionRegion, setActionRegion] = useState<HTMLDivElement | null>(null);
  // 「打开过教程」也存服务端（原来在 localStorage：端口一变，这个标记就没了，教程又变成未读）。
  const tutorialRead = useUiPreferences().tutorialRead === true;

  // 标题带里标题与动作区是同一行的两列，而动作区的宽度随页面动作变化（0~3 个按钮，
  // 每个页面还不一样）。把它的实测宽度写成 CSS 变量后，标题行才能据此让位；否则窄窗口
  // 下页面说明会被右对齐的按钮横向盖住（真实发生过：700px 的 Codex 页重叠约 69px）。
  // 动作区自身没有 padding，所以 border-box 宽度就是要让出的宽度，列间距由
  // --app-title-band-gap 单独给出。
  useEffect(() => {
    if (!actionRegion) return;
    const root = document.documentElement;
    const sync = () => {
      const width = Math.ceil(actionRegion.getBoundingClientRect().width);
      root.style.setProperty("--app-title-actions", `${width}px`);
    };
    sync();
    const observer = new ResizeObserver(sync);
    observer.observe(actionRegion);
    return () => {
      observer.disconnect();
      root.style.removeProperty("--app-title-actions");
    };
  }, [actionRegion]);

  // 全局错误（刷新失败、保存失败…）走顶部提示条。提示条由本布局渲染——它要读
  // --app-content-top 才能落在顶部三条带之下——所以这条接线也放在这里。
  const previousError = useRef<string | null>(null);
  const showMessage = useMessage();
  useEffect(() => {
    if (error && error !== previousError.current) showMessage(error, { tone: "error" });
    previousError.current = error;
  }, [error, showMessage]);

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

  // 客户端状态挂在各自的接入页上（见 HarnessTabs）：Cursor 的接管模式和 Devin、
  // Codex 的网关状态互不代表对方，也都不是「整个应用」的状态。顶栏那一个入口只
  // 汇总成「几个客户端在用本机网关」；只有全接或硬冲突才着色，部分接入保持中性。
  const harnessState = { cursorHarness, devinStatus, codexStatus, claudeStatus, claudeTakeover };
  const harnessEntry = harnessEntryStatus(harnessState);
  // 接入页多一条切换带，窗口顶部的两条带依次往下排（见 .harnessModule / .harnessBar）。
  const currentClient = harnessClientAt(location.pathname);
  // 全局那一颗只说一件事：本地管理服务能不能连上。
  const service = localServiceStatus(offline);

  // Groups stay in the list as separators: a horizontal bar has no room for the
  // section headings a vertical list used to carry.
  const menuItems: MenuItem[] = [
    { kind: "page", path: "/", label: t("概览"), icon: navOverviewIcon },
    { kind: "page", path: "/calls", label: t("调用"), icon: navCallsIcon },
    { kind: "group", label: t("接入模块") },
    // 客户端收成一个入口：接入页里切换，顶栏不随客户端数量变长。
    { kind: "page", path: currentClient?.path ?? harnessClients[0].path, label: t("接入"), icon: navHarnessIcon, status: harnessEntry },
    { kind: "group", label: t("共享") },
    { kind: "page", path: "/models", label: t("模型库"), icon: navModelsIcon },
    { kind: "group", label: t("设置") },
    { kind: "page", path: "/plugins", label: t("插件配置"), icon: navPluginsIcon },
    { kind: "page", path: "/settings", label: t("系统设置"), icon: navSettingsIcon },
    { kind: "page", path: "/tutorial", label: t("使用教程"), icon: navTutorialIcon },
  ];

  // 教程就在应用里，所以「未读」的判定是「还没打开过这一页」。
  useEffect(() => {
    if (location.pathname !== "/tutorial" || tutorialRead) return;
    void saveUiPreferences({ tutorialRead: true });
  }, [location.pathname, tutorialRead]);

  const renderIcon = (icon: IconifyIcon | string) => typeof icon === "string"
    ? <Icon src={icon} size="1.15em" />
    : <Icon icon={icon} size="1.15em" />;

  return <PageLayout className={[styles.root, currentClient && styles.harnessModule].filter(Boolean).join(" ")}>
    <CommandPalette />
    <OfflineBanner />
    {/* 顶部提示条与离线提示一样挂在布局里：它的位置要读外壳的 --app-content-top
        （见 MessageProvider.module.scss），所以不落在 body 上。 */}
    <MessageProvider />
    <div className={styles.topBar}>
      <nav className={styles.navigation} aria-label={t("主菜单")}>
        {menuItems.map((item) => item.kind === "group"
          ? <span key={`group-${item.label}`} className={styles.navDivider} aria-hidden="true" />
          : <NavLink key={item.path} to={item.path} end={item.path === "/"} className={styles.navItem}>
            {renderIcon(item.icon)}
            <span>{item.label}</span>
            {item.path === "/tutorial" && !tutorialRead && <span className={styles.navDot} aria-hidden="true" />}
            {item.status && <span className={styles.navStatus} data-tone={item.status.tone}>{item.status.label}</span>}
          </NavLink>)}
      </nav>
      {/* 常驻状态只回答「本地管理服务连上了没有」。以前这里显示的是 Devin 网关，
          网关没启用就写「已关闭」——看起来像整个应用被关掉了，而实际上 Cursor
          模块可能正常。接入模块自己的状态放在切换带上（见 harnessBar），不在顶栏。
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
    {/* 接入页的客户端切换占导航条下面那条自己的带（.harnessBar）；标题带与页面内容
        由 --app-harness-band 一起下移，所以切换行不会压在页面标题与说明上。 */}
    {currentClient && <div className={styles.harnessBar}><HarnessTabs state={harnessState} /></div>}
    <div ref={setActionRegion} className={styles.actionRegion}>
      <div ref={setLeftActionTarget} className={styles.pageActions} />
      {location.pathname !== "/" && <TooltipTrigger label={t("刷新")}><button className={controls.iconButton} aria-label={t("刷新")} disabled={busy} onClick={() => void appStore.refresh()}>
        <Icon className={busy ? controls.spin : ""} icon={refreshIcon} size="1.1em" />
      </button></TooltipTrigger>}
      <div ref={setRightActionTarget} className={styles.pageActions} />
    </div>
    <main className={styles.content}>
      <PageActionsTarget.Provider value={{ left: leftActionTarget, right: rightActionTarget }}>
        {/* 外层边界只是兜底（缓存容器自身出错的情形）：页面报错由每个缓存节点内部的
            PageCacheErrorBoundary 接住，不会连累其它页面的缓存状态。 */}
        <PageErrorBoundary resetKey={location.pathname}>
          <KeepAliveRouteOutlet
            activeCacheKey={location.pathname}
            include={keptAlivePages}
            max={keptAlivePages.length}
            enableActivity
            errorElement={PageCacheErrorBoundary}
            containerClassName={styles.keepAliveContainer}
            cacheNodeClassName={styles.keepAlivePage}
          />
        </PageErrorBoundary>
      </PageActionsTarget.Provider>
    </main>
  </PageLayout>;
}
