import { useEffect, useMemo, useRef, useState } from "react";
import { api, pluginText, type PluginDescriptor, type PluginImportFile, type PluginRuntimePhase, type PluginRuntimeStatus } from "../../shared/api";
import { useI18n } from "../../i18n/store";
import { PageContent } from "../../shell/layout/PageContent";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { ActionMenu, type ActionMenuItem } from "../../shared/ui/ActionMenu";
import { Card } from "../../shared/ui/Card";
import { EmptyState } from "../../shared/ui/EmptyState";
import { Icon } from "../../shared/ui/Icon";
import { Modal } from "../../shared/ui/Modal";
import { PageTitle } from "../../shared/ui/PageTitle";
import { SearchInput } from "../../shared/ui/SearchInput";
import { ServiceOfflineState } from "../../shared/ui/ServiceOfflineState";
import { StatusPill, type StatusTone } from "../../shared/ui/StatusPill";
import { useMessage } from "../../shared/ui/message";
import { alertCircleIcon, downloadIcon, puzzleIcon, uploadIcon } from "../../shared/ui/icons";
import { PluginAddPanel, PluginSettingsPanel } from "./PluginResourcePanels";
import styles from "./PluginManagementPage.module.scss";
import toolbar from "../../shared/ui/Toolbar.module.scss";

export function PluginManagementPage() {
  const { pluginRuntime, plugins, offline } = useAppStore();
  const [progressOpen, setProgressOpen] = useState(false);
  const [starting, setStarting] = useState(false);
  const [selected, setSelected] = useState<{ pluginId: string; mode: "add" | "settings" } | null>(null);
  const [search, setSearch] = useState("");
  const cancelRequested = useRef(false);
  const selectedPlugin = selected ? plugins.find((plugin) => plugin.id === selected.pluginId) ?? null : null;

  useEffect(() => {
    if (!pluginRuntime) void appStore.refreshPluginRuntime();
  }, [pluginRuntime]);

  useEffect(() => {
    if (pluginRuntime?.state !== "initializing") return;
    if (!cancelRequested.current) setProgressOpen(true);
    const timer = window.setInterval(() => void appStore.refreshPluginRuntime(), 300);
    return () => window.clearInterval(timer);
  }, [pluginRuntime?.state]);

  const initialize = async () => {
    if (starting) return;
    cancelRequested.current = false;
    setStarting(true);
    setProgressOpen(true);
    const status = await appStore.initializePluginRuntime();
    setStarting(false);
    if (!status) {
      setProgressOpen(false);
    } else if (cancelRequested.current && status.state === "initializing") {
      void appStore.cancelPluginRuntimeInitialization();
    }
  };

  const closeProgress = () => {
    setProgressOpen(false);
    cancelRequested.current = true;
    if (pluginRuntime?.state === "initializing") {
      void appStore.cancelPluginRuntimeInitialization();
    }
  };

  const query = search.trim().toLowerCase();
  const visiblePlugins = useMemo(() => query
    ? plugins.filter((plugin) => [plugin.name, plugin.id, plugin.author ?? ""]
      .some((field) => field.toLowerCase().includes(query)))
    : plugins, [plugins, query]);
  const ready = pluginRuntime?.state === "ready";
  const runtimeTone: StatusTone = !pluginRuntime
    ? "idle"
    : pluginRuntime.state === "ready"
      ? "ok"
      : pluginRuntime.state === "failed" || pluginRuntime.state === "unsupported"
        ? "bad"
        : pluginRuntime.state === "initializing"
          ? "info"
          : "warn";
  const runtimeLabel = !pluginRuntime
    ? t("检查中…")
    : pluginRuntime.state === "ready"
      ? t("已就绪")
      : pluginRuntime.state === "initializing"
        ? phaseText(pluginRuntime.phase)
        : pluginRuntime.state === "failed"
          ? t("初始化失败")
          : pluginRuntime.state === "unsupported"
            ? t("不受支持")
            : t("未初始化");
  const accountTotal = plugins.reduce((total, plugin) => total + plugin.resources.reduce((count, resource) => count + resource.resources.length, 0), 0);
  const modelTotal = plugins.reduce((total, plugin) => total + plugin.providers.reduce((count, provider) => count + (provider.configured ? provider.models.filter((model) => model.enabled).length : 0), 0), 0);

  const content = ready
    ? <div className={styles.page}>      <Card className={styles.pluginToolbar}>
        <SearchInput
          className={styles.pluginSearch}
          value={search}
          onValueChange={setSearch}
          ariaLabel={t("搜索插件")}
          placeholder={t("搜插件名称或作者…")}
        />
        <span className={toolbar.spacer} />
        <span className={toolbar.count}>{t("共 {count} 个插件", { count: plugins.length })}</span>
      </Card>
      <PluginCards plugins={visiblePlugins} onOpen={(pluginId, mode) => setSelected({ pluginId, mode })} />
    </div>
    : offline
      ? <ServiceOfflineState />
      : <RuntimeGate status={pluginRuntime} starting={starting} onInitialize={() => void initialize()} />;
  const estimatedHeight = plugins.length > 0
    ? Math.max(320, Math.ceil(plugins.length / 3) * 210 + 120)
    : 360;

  return <>
    <PageContent
      title={<PageTitle
        title={t("插件配置")}
        status={<StatusPill tone={runtimeTone}>{runtimeLabel}</StatusPill>}
        meta={t("插件把第三方账号接入模型库；运行时负责承载它们")}
      />}
      sections={[{
        key: "installed-plugins",
        estimatedHeight,
        content: ready
          ? <>
            <div className={toolbar.facts}>
              <Fact label={t("插件")} value={String(plugins.length)} />
              <Fact label={t("账号")} value={String(accountTotal)} tone={accountTotal > 0 ? "ok" : "none"} />
              <Fact label={t("可用模型")} value={String(modelTotal)} />
              <Fact label={t("运行时")} value={pluginRuntime?.version || "-"} />
            </div>
            {content}
          </>
          : content,
      }]}
    />
    <RuntimeProgressModal
      open={progressOpen}
      status={pluginRuntime}
      starting={starting}
      onClose={closeProgress}
    />
    <Modal
      fullHeight
      open={selectedPlugin !== null}
      title={selected?.mode === "settings"
        ? t("{name} 账号管理", { name: selectedPlugin?.name ?? "" })
        : t("添加 {name} 账号", { name: selectedPlugin?.name ?? "" })}
      onClose={() => setSelected(null)}
      onSubmit={() => setSelected(null)}
      submitLabel={t("确定")}
    >
      {selected?.mode === "add" && selectedPlugin && <PluginAddPanel plugin={selectedPlugin} onConfigured={() => setSelected(null)} />}
      {selected?.mode === "settings" && selectedPlugin && <PluginSettingsPanel plugin={selectedPlugin} />}
    </Modal>
  </>;
}

function Fact({ label, value, tone = "none" }: { label: string; value: string; tone?: "none" | "ok" | "warn" | "bad" }) {
  return <div className={toolbar.fact}>
    <span className={toolbar.factLabel}>{label}</span>
    <span className={toolbar.factValue} data-tone={tone}>{value}</span>
  </div>;
}

/**
 * 插件运行时的引导。
 *
 * 这一步要下载几十兆的东西，所以它必须交代清楚「会发生什么」和「现在到哪一步了」：
 * 三个步骤对应三种状态，下载进度直接画在同一个位置，用户不需要在两个弹窗之间来回看。
 */
function RuntimeGate({ status, starting, onInitialize }: { status: PluginRuntimeStatus | null; starting: boolean; onInitialize: () => void }) {
  const checking = status === null;
  const initializing = starting || status?.state === "initializing";
  const failed = status?.state === "failed";
  const unsupported = status?.state === "unsupported";
  const downloaded = status?.downloaded_bytes ?? 0;
  const total = status?.total_bytes ?? null;
  const percent = total && total > 0 ? Math.min(100, Math.round((downloaded / total) * 100)) : null;

  return <EmptyState
    icon={unsupported || failed ? alertCircleIcon : puzzleIcon}
    tone={unsupported || failed ? "bad" : "info"}
    title={checking
      ? t("正在检查插件运行时")
      : failed
        ? t("插件运行时初始化失败")
        : unsupported
          ? t("当前系统不支持插件运行时")
          : initializing
            ? phaseText(status?.phase ?? null)
            : t("需要先初始化插件运行时")}
    description={failed
      ? status.error ?? t("请重试初始化；下载失败通常是网络问题。")
      : unsupported
        ? status.error ?? t("当前操作系统或 CPU 架构暂不受支持")
        : t("初始化会下载并安装插件运行时；完成之后，装在这里的插件才能被 Cursor 与 Devin 使用。")}
    steps={!failed && !unsupported && !initializing ? [
      { title: t("下载运行时"), detail: t("来自插件的发布源，只下一次。") },
      { title: t("安装并校验"), detail: t("应用会校验下载文件的完整性。") },
      { title: t("插件出现在下方"), detail: t("之后即可添加账号、同步模型。") },
    ] : undefined}
    actions={!unsupported && <>
      {initializing && <span className={styles.progress}>
        <span className={styles.progressBar} role="progressbar" aria-label={t("下载进度")} aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent ?? undefined}>
          <span className={styles.progressFill} style={{ width: `${percent ?? 100}%` }} />
        </span>
        <small>{total ? t("已下载 {downloaded} / {total}", { downloaded: formatBytes(downloaded), total: formatBytes(total) }) : t("已下载 {downloaded}", { downloaded: formatBytes(downloaded) })}</small>
      </span>}
      <button type="button" className={styles.primaryAction} disabled={checking || initializing} onClick={onInitialize}>
        {checking ? t("检查中…") : initializing ? t("初始化中…") : failed ? t("重新初始化插件") : t("初始化插件")}
      </button>
    </>}
  />;
}

function PluginCards({ plugins, onOpen }: {
  plugins: PluginDescriptor[];
  onOpen: (pluginId: string, mode: "add" | "settings") => void;
}) {
  if (plugins.length === 0) {
    return <EmptyState
      icon={puzzleIcon}
      title={t("没有匹配的插件")}
      description={t("换个关键词，或清空搜索框。清空后仍为空，说明还没有安装插件。")}
    />;
  }
  return <div className={styles.pluginGrid}>
    {plugins.map((plugin) => <PluginCard key={plugin.id} plugin={plugin} onOpen={onOpen} />)}
  </div>;
}

function PluginCard({ plugin, onOpen }: {
  plugin: PluginDescriptor;
  onOpen: (pluginId: string, mode: "add" | "settings") => void;
}) {
  const { locale } = useI18n();
  const { ports } = useAppStore();
  const message = useMessage();
  const importInput = useRef<HTMLInputElement>(null);
  const [importing, setImporting] = useState(false);
  const configured = plugin.providers.some((provider) => provider.configured);
  const accountCount = plugin.resources.reduce((count, resource) => count + resource.resources.length, 0);
  const modelCount = plugin.providers.reduce((count, provider) => count + provider.models.length, 0);
  const subtitle = plugin.providers.map((provider) => pluginText(provider.displayName, locale)).join(" · ") || plugin.id;
  const importResource = plugin.resources.find((resource) => resource.import);
  const exportResource = plugin.resources.find((resource) => resource.resources.length > 0);

  const importFiles = async (files: FileList | null) => {
    if (!files?.length || !importResource) return;
    setImporting(true);
    try {
      const entries: PluginImportFile[] = await Promise.all(
        [...files].map(async (file) => ({ name: file.name, content: await file.text() })),
      );
      const result = await api.importPluginResources(plugin.id, importResource.type, entries);
      await appStore.refreshPlugins();
      const summary = t("导入完成：新增 {added}，更新 {updated}", { added: result.added, updated: result.updated });
      if (result.modelSyncError) {
        message(t("账号已保存，但同步模型失败：{error}", { error: result.modelSyncError }), { duration: 5000 });
      } else if (result.warnings.length > 0) {
        message(`${summary} · ${result.warnings.join("; ")}`, { duration: 5000 });
      } else {
        message(summary);
      }
    } catch (cause) {
      message.error(cause, { duration: 5000 });
    } finally {
      setImporting(false);
      if (importInput.current) importInput.current.value = "";
    }
  };

  const moreItems: ActionMenuItem[] = [
    ...(importResource
      ? [
          {
            id: "import",
            label: importing ? t("正在导入…") : t("批量导入"),
            disabled: importing,
            onSelect: () => importInput.current?.click(),
          },
        ]
      : []),
    ...(exportResource
      ? [
          {
            id: "export",
            label: t("批量导出"),
            onSelect: () =>
              void api.openExternalUrl(
                api.pluginResourceExportUrl(
                  ports.service_port,
                  plugin.id,
                  exportResource.type,
                ),
              ),
          },
        ]
      : []),
  ];

  return (
    <Card className={styles.pluginCard}>
      <div className={styles.pluginCardTop}>
        <img className={styles.pluginIcon} src={plugin.icon} alt="" />
        <div className={styles.pluginIdentity}>
          <span className={styles.pluginName}>{plugin.name}</span>
          <span className={styles.pluginId}>{subtitle}</span>
        </div>
        <StatusPill tone={configured ? "ok" : "idle"}>{configured ? t("已配置") : t("未配置")}</StatusPill>
      </div>
      <div className={styles.pluginStats}>
        <span><strong>{accountCount}</strong>{t("个账号")}</span>
        <span><strong>{modelCount}</strong>{t("个模型")}</span>
        {plugin.version && <span className={styles.pluginVersion}>v{plugin.version}</span>}
      </div>
      {plugin.author && <div className={styles.pluginAuthor}>{plugin.author}</div>}
      <div className={styles.cardActions}>
        <button type="button" className={styles.primaryAction} onClick={() => onOpen(plugin.id, "add")}>
          <Icon icon={downloadIcon} size="1em" />{t("添加账号")}
        </button>
        {configured && <button type="button" className={styles.secondaryAction} onClick={() => onOpen(plugin.id, "settings")}>
          <Icon icon={uploadIcon} size="1em" />{t("账号管理")}
        </button>}
        {moreItems.length > 0 && <span className={styles.moreAction}>
          <ActionMenu label={t("更多")} items={moreItems} />
        </span>}
        {importResource && (
          <input
            ref={importInput}
            type="file"
            hidden
            accept={importResource.import?.accept.join(",")}
            multiple={importResource.import?.multiple ?? false}
            onChange={(event) => void importFiles(event.target.files)}
          />
        )}
      </div>
    </Card>
  );
}

function RuntimeProgressModal({ open, status, starting, onClose }: { open: boolean; status: PluginRuntimeStatus | null; starting: boolean; onClose: () => void }) {
  const initializing = starting || status?.state === "initializing";
  const downloaded = status?.downloaded_bytes ?? 0;
  const total = status?.total_bytes ?? null;
  const percent = total && total > 0 ? Math.min(100, Math.round((downloaded / total) * 100)) : null;
  const stage = status?.state === "ready"
    ? t("插件运行时初始化完成")
    : status?.state === "failed"
      ? t("插件运行时初始化失败")
      : phaseText(status?.phase ?? null);

  return <Modal
    open={open}
    title={t("初始化插件运行时")}
    closeLabel={status?.state === "ready" ? t("完成") : initializing ? t("取消") : t("关闭")}
    onClose={onClose}
  >
    <div className={styles.progressContent} aria-live="polite">
      <strong>{stage}</strong>
      {status?.phase === "downloading" && <>
        <div className={styles.progressBar} role="progressbar" aria-label={t("下载进度")} aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent ?? undefined}>
          <div className={styles.progressFill} style={{ width: `${percent ?? 100}%` }} />
        </div>
        <span>
          {total ? t("已下载 {downloaded} / {total}", { downloaded: formatBytes(downloaded), total: formatBytes(total) }) : t("已下载 {downloaded}", { downloaded: formatBytes(downloaded) })}
        </span>
      </>}
      {status?.state === "failed" && <span className={styles.error}>{t("请重试初始化")}</span>}
      {status?.state === "ready" && <span>{t("插件运行时 {version} 已安装，可以开始使用插件。", { version: status.version })}</span>}
    </div>
  </Modal>;
}

function phaseText(phase: PluginRuntimePhase | null) {
  switch (phase) {
    case "checking": return t("正在检查插件运行时");
    case "downloading": return t("正在下载插件运行时");
    case "verifying": return t("正在验证插件运行时下载文件");
    case "installing": return t("正在安装插件运行时");
    case "validating": return t("正在验证插件运行时");
    default: return t("正在准备插件运行时");
  }
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : value.toFixed(0)} ${units[unit]}`;
}
