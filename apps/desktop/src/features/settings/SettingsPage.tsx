import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, type ProxySettings, type ProxySettingsInput, type StatisticsStorage, type StatisticsStorageScope, type TabSettings } from "../../shared/api";
import { PageContent } from "../../shell/layout/PageContent";
import { LegacyModelImport } from "../models/LegacyModelImport";
import { AppLifecycleSettingsCard } from "./AppLifecycleSettingsCard";
import { CommitSettingsCard } from "./CommitSettingsCard";
import { PricingSettingsCard } from "./PricingSettingsCard";
import { ProxySettingsCard } from "./ProxySettingsCard";
import { TabSettingsCard } from "./TabSettingsCard";
import { UpdateCard } from "./UpdateCard";
import { Button } from "../../shared/ui/Button";
import { Checkbox } from "../../shared/ui/Checkbox";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { PageTitle } from "../../shared/ui/PageTitle";
import { SectionHeading } from "../../shared/ui/SectionHeading";
import { Select } from "../../shared/ui/Select";
import { TitledCard } from "../../shared/ui/TitledCard";
import { setLocalePreference, useI18n, type LocalePreference } from "../../i18n/store";
import { useMessage } from "../../shared/ui/message";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { themeOptions } from "../../shared/theme/theme";
import { ScrollableContent } from "../../shared/virtual/ScrollableContent";
import styles from "./SettingsPage.module.scss";

type SettingsCardId =
  | "update" | "language" | "theme" | "app"
  | "ports" | "proxy" | "tab"
  | "observability" | "pricing" | "storage"
  | "commit" | "import";

type SettingsGroup = {
  id: string;
  label: string;
  hint: string;
  items: Array<{ id: SettingsCardId; label: string }>;
};

export function SettingsPage() {
  const { detailed, ports, theme } = useAppStore();
  const { preference, locale } = useI18n();
  const message = useMessage();
  const [proxyPort, setProxyPort] = useState(String(ports.proxy_port));
  const [servicePort, setServicePort] = useState(String(ports.service_port));
  const [editingPorts, setEditingPorts] = useState(false);
  const [savingPorts, setSavingPorts] = useState(false);
  const [storage, setStorage] = useState<StatisticsStorage | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [clearScope, setClearScope] = useState<StatisticsStorageScope>("details");
  const [clearing, setClearing] = useState(false);
  const [outboundProxy, setOutboundProxy] = useState<ProxySettings | null>(null);
  const [proxyDraft, setProxyDraft] = useState<ProxySettingsInput>({ mode: "default", address: "", auth_enabled: false, username: "", password: "" });
  const [editingProxy, setEditingProxy] = useState(false);
  const [savingProxy, setSavingProxy] = useState(false);
  const [tabSettings, setTabSettings] = useState<TabSettings | null>(null);
  const [tabDraft, setTabDraft] = useState<TabSettings>({ mode: "public", address: "" });
  const [editingTab, setEditingTab] = useState(false);
  const [savingTab, setSavingTab] = useState(false);
  const [activeCard, setActiveCard] = useState<SettingsCardId>("update");
  const cardRefs = useRef(new Map<SettingsCardId, HTMLElement>());

  useEffect(() => {
    void Promise.all([api.statisticsStorage(), api.proxySettings(), api.tabSettings()]).then(([nextStorage, nextProxy, nextTab]) => {
      setStorage(nextStorage);
      setOutboundProxy(nextProxy);
      setProxyDraft({ mode: nextProxy.mode, address: nextProxy.address, auth_enabled: nextProxy.auth_enabled, username: nextProxy.username, password: "" });
      setTabSettings(nextTab);
      setTabDraft(nextTab);
    }).catch((cause) => message.error(cause));
  }, [message]);
  useEffect(() => {
    setProxyPort(String(ports.proxy_port));
    setServicePort(String(ports.service_port));
  }, [ports.proxy_port, ports.service_port]);

  // 标签随语言变化：依赖 locale 而不是 t 本身（t 是构建期注入的模块级函数，
  // 写成依赖会被当成未定义变量）。
  const groups: SettingsGroup[] = useMemo(() => [
    {
      id: "general",
      label: t("常规"),
      hint: t("更新、外观与启动"),
      items: [
        { id: "update", label: t("软件更新") },
        { id: "language", label: t("语言") },
        { id: "theme", label: t("主题") },
        { id: "app", label: t("应用设置") },
      ],
    },
    {
      id: "network",
      label: t("网络"),
      hint: t("端口与出网方式"),
      items: [
        { id: "ports", label: t("端口设置") },
        { id: "proxy", label: t("代理设置") },
        { id: "tab", label: t("TAB 设置") },
      ],
    },
    {
      id: "usage",
      label: t("用量与统计"),
      hint: t("记录什么、怎么计价、占多少空间"),
      items: [
        { id: "observability", label: t("调用观测") },
        { id: "pricing", label: t("Token 定价") },
        { id: "storage", label: t("存储管理") },
      ],
    },
    {
      id: "models",
      label: t("模型与提交"),
      hint: t("提交信息模型与旧配置导入"),
      items: [
        { id: "commit", label: t("Commit 模型") },
        { id: "import", label: t("导入") },
      ],
    },
  ], [locale]);
  const cards = useMemo(() => groups.flatMap((group) => group.items), [groups]);

  // 卡片在页面里的位置决定导航高亮：观察的是卡片本身，不是滚动距离，
  // 所以窗口大小变化时高亮也不会错位。
  useEffect(() => {
    const observer = new IntersectionObserver((entries) => {
      const visible = entries.filter((entry) => entry.isIntersecting);
      if (visible.length === 0) return;
      const top = visible.reduce((nearest, entry) => entry.boundingClientRect.top < nearest.boundingClientRect.top ? entry : nearest);
      const id = top.target.getAttribute("data-card") as SettingsCardId | null;
      if (id) setActiveCard(id);
    }, { rootMargin: "-15% 0px -60% 0px", threshold: 0 });
    for (const element of cardRefs.current.values()) observer.observe(element);
    return () => observer.disconnect();
  }, [groups]);

  const registerCard = useCallback((id: SettingsCardId) => (element: HTMLElement | null) => {
    if (element) cardRefs.current.set(id, element);
    else cardRefs.current.delete(id);
  }, []);
  const jumpTo = (id: SettingsCardId) => {
    cardRefs.current.get(id)?.scrollIntoView({ block: "start", behavior: "smooth" });
  };

  const parsePort = (value: string, label: string) => {
    const port = Number(value);
    if (!Number.isInteger(port) || port < 0 || port > 65_535) {
      throw new Error(`${label}${t("必须是 0–65535 之间的整数")}`);
    }
    return port;
  };
  const savePorts = async () => {
    try {
      const next = {
        proxy_port: parsePort(proxyPort, t("代理端口")),
        service_port: parsePort(servicePort, t("服务端口")),
      };
      setSavingPorts(true);
      if (await appStore.updatePorts(next)) {
        setEditingPorts(false);
        message(t("端口设置已保存，重启软件后生效"), { duration: 4_000 });
      }
    } catch (cause) {
      message.error(cause);
    } finally {
      setSavingPorts(false);
    }
  };
  const editPorts = () => {
    setProxyPort(String(ports.proxy_port));
    setServicePort(String(ports.service_port));
    setEditingPorts(true);
  };
  const cancelPortEdit = () => {
    setProxyPort(String(ports.proxy_port));
    setServicePort(String(ports.service_port));
    setEditingPorts(false);
  };
  const clearStorage = async () => {
    try {
      setClearing(true);
      setStorage(await api.clearStatisticsStorage(clearScope));
      setConfirmClear(false);
      await appStore.refresh();
      message(clearScope === "all" ? t("全部统计数据已清理") : t("详细记录已清理"));
    } catch (cause) {
      message.error(cause);
    } finally {
      setClearing(false);
    }
  };
  const editProxy = () => {
    if (!outboundProxy) return;
    setProxyDraft({ mode: outboundProxy.mode, address: outboundProxy.address, auth_enabled: outboundProxy.auth_enabled, username: outboundProxy.username, password: "" });
    setEditingProxy(true);
  };
  const cancelProxyEdit = () => {
    if (outboundProxy) {
      setProxyDraft({ mode: outboundProxy.mode, address: outboundProxy.address, auth_enabled: outboundProxy.auth_enabled, username: outboundProxy.username, password: "" });
    }
    setEditingProxy(false);
  };
  const saveProxy = async () => {
    try {
      setSavingProxy(true);
      const saved = await api.setProxySettings({ ...proxyDraft, password: proxyDraft.password || undefined });
      setOutboundProxy(saved);
      setProxyDraft({ mode: saved.mode, address: saved.address, auth_enabled: saved.auth_enabled, username: saved.username, password: "" });
      setEditingProxy(false);
      message(t("代理设置已保存"));
    } catch (cause) {
      message.error(cause);
    } finally {
      setSavingProxy(false);
    }
  };
  const editTab = () => {
    if (!tabSettings) return;
    setTabDraft(tabSettings);
    setEditingTab(true);
  };
  const cancelTabEdit = () => {
    if (tabSettings) setTabDraft(tabSettings);
    setEditingTab(false);
  };
  const saveTab = async () => {
    try {
      if (tabDraft.mode === "custom" && !tabDraft.address.trim()) throw new Error(t("TAB 服务地址不能为空"));
      setSavingTab(true);
      const saved = await api.setTabSettings({ ...tabDraft, address: tabDraft.address.trim() });
      setTabSettings(saved);
      setTabDraft(saved);
      setEditingTab(false);
      message(t("TAB 设置已保存"));
    } catch (cause) {
      message.error(cause);
    } finally {
      setSavingTab(false);
    }
  };
  const formatBytes = (bytes: number) => {
    if (bytes < 1024) return `${bytes} B`;
    const units = ["KB", "MB", "GB", "TB"];
    let value = bytes / 1024;
    let unit = 0;
    while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
    return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
  };
  const clearTitle = clearScope === "all" ? t("确定要清理全部统计数据吗？") : t("确定要清理详细记录吗？");
  const clearDescription = clearScope === "all"
    ? t("所有调用汇总、详细内容和追踪记录都会被删除。模型配置、CA 和应用设置不会受到影响，此操作无法撤销。")
    : t("仅删除请求、响应和追踪附件等详细内容，保留调用汇总、统计指标和配置。");

  const content = (
    <div className={styles.page}>
      <nav className={styles.rail} aria-label={t("设置导航")}>
        {groups.map((group) => <div key={group.id} className={styles.railGroup}>
          <span className={styles.railGroupLabel}>{group.label}</span>
          {group.items.map((item) => <button
            key={item.id}
            type="button"
            className={styles.railItem}
            data-active={item.id === activeCard || undefined}
            aria-current={item.id === activeCard ? "true" : undefined}
            onClick={() => jumpTo(item.id)}
          >{item.label}</button>)}
        </div>)}
      </nav>
      <ScrollableContent className={styles.content} contentClassName={styles.contentInner}>
        <SectionHeading eyebrow={groups[0].label} title={groups[0].hint} id="settings-general" />
        <div className={styles.cards}>
          <div data-card="update" ref={registerCard("update")}><UpdateCard /></div>
          <div data-card="language" ref={registerCard("language")}>
            <TitledCard title={t("语言")} description={t("界面文字与「价值估算」使用的币种都跟着它")}>
              <div className={styles.settingRow}>
                <div>
                  <strong>{t("界面语言")}</strong>
                  <small>{t("默认跟随操作系统；不支持的系统语言使用英文。当前：{language}", { language: locale === "zh-CN" ? "简体中文" : "English" })}</small>
                </div>
                <div className={styles.languageControl}>
                  <Select
                    value={preference}
                    ariaLabel={t("界面语言")}
                    options={[
                      { value: "system", label: t("跟随系统") },
                      { value: "zh-CN", label: "简体中文" },
                      { value: "en-US", label: "English" },
                    ]}
                    onChange={(value) => setLocalePreference(value as LocalePreference)}
                  />
                </div>
              </div>
            </TitledCard>
          </div>
          <div data-card="theme" ref={registerCard("theme")}>
            <TitledCard title={t("主题")} description={t("配色方案；图表与热力图会跟着一起换")}>
              <div className={styles.themeOptions}>
                {themeOptions.map(({ id }) => <button
                  key={id}
                  type="button"
                  className={styles.themeOption}
                  aria-pressed={theme === id}
                  onClick={() => appStore.selectTheme(id)}
                >
                  <span className={styles.themeSwatch} data-theme-id={id} aria-hidden="true" />
                  <span className={styles.themeLabel}>{id === "midnight" ? t("午夜靛蓝") : id === "default-dark" ? t("默认暗色") : t("默认亮色")}</span>
                </button>)}
              </div>
            </TitledCard>
          </div>
          <div data-card="app" ref={registerCard("app")}><AppLifecycleSettingsCard /></div>
        </div>

        <SectionHeading eyebrow={groups[1].label} title={groups[1].hint} id="settings-network" />
        <div className={styles.cards}>
          <div data-card="ports" ref={registerCard("ports")}>
            <TitledCard
              title={t("端口设置")}
              description={t("本机服务监听的端口；修改后需要重启")}
              action={editingPorts ? (
                <div className={styles.cardActions}>
                  <Button size="small" disabled={savingPorts} onClick={cancelPortEdit}>{t("取消")}</Button>
                  <Button variant="primary" size="small" disabled={savingPorts} onClick={() => void savePorts()}>{savingPorts ? t("保存中…") : t("保存")}</Button>
                </div>
              ) : (
                <button type="button" className={styles.textButton} onClick={editPorts}>{t("编辑")}</button>
              )}
            >
              <div className={styles.portSettings}>
                <div className={styles.portFields}>
                  {editingPorts ? <><FormField
                    label={t("代理端口")}
                    hint={t("Cursor 使用的本地代理端口；填写 0 时启动时随机选择。")}
                  >
                    <TextInput
                      type="number"
                      min={0}
                      max={65535}
                      step={1}
                      value={proxyPort}
                      onChange={(event) => setProxyPort(event.target.value)}
                    />
                  </FormField>
                  <FormField
                    label={t("服务端口")}
                    hint={t(
                      "桌面前端连接的本地管理服务端口；填写 0 时启动时随机选择。",
                    )}
                  >
                    <TextInput
                      type="number"
                      min={0}
                      max={65535}
                      step={1}
                      value={servicePort}
                      onChange={(event) => setServicePort(event.target.value)}
                    />
                  </FormField></> : <>
                    <div className={styles.portValue}><strong>{t("代理端口")}</strong><span>{ports.proxy_port}</span></div>
                    <div className={styles.portValue}><strong>{t("服务端口")}</strong><span>{ports.service_port}</span></div>
                  </>}
                </div>
                <div className={styles.portFooter}>
                  <small>
                    {t(
                      "端口被占用时会自动选择新的随机端口并保存。修改后需要重启软件才会生效。",
                    )}
                  </small>
                </div>
              </div>
            </TitledCard>
          </div>
          <div data-card="proxy" ref={registerCard("proxy")}>
            <ProxySettingsCard settings={outboundProxy} draft={proxyDraft} editing={editingProxy} saving={savingProxy} onDraftChange={setProxyDraft} onEdit={editProxy} onCancel={cancelProxyEdit} onSave={() => void saveProxy()} />
          </div>
          <div data-card="tab" ref={registerCard("tab")}>
            <TabSettingsCard settings={tabSettings} draft={tabDraft} editing={editingTab} saving={savingTab} onDraftChange={setTabDraft} onEdit={editTab} onCancel={cancelTabEdit} onSave={() => void saveTab()} />
          </div>
        </div>

        <SectionHeading eyebrow={groups[2].label} title={groups[2].hint} id="settings-usage" />
        <div className={styles.cards}>
          <div data-card="observability" ref={registerCard("observability")}>
            <TitledCard title={t("调用观测")} description={t("决定调用记录保留到什么程度")}>
              <div className={styles.settingRow}>
                <div>
                  <strong>{t("详细模式")}</strong>
                  <small>
                    {t("额外保存完整请求和流响应；默认只保存时间、状态与用量。")}
                  </small>
                </div>
                <Checkbox
                  label={t("详细模式")}
                  checked={detailed}
                  onChange={(checked) => void appStore.updateDetailed(checked)}
                />
              </div>
            </TitledCard>
          </div>
          <div data-card="pricing" ref={registerCard("pricing")}><PricingSettingsCard /></div>
          <div data-card="storage" ref={registerCard("storage")}>
            <TitledCard title={t("存储管理")} description={t("统计数据占用的空间与清理")}>
              <div className={styles.storageRow}>
                <div>
                  <strong>{t("统计数据")}</strong>
                  <small>{storage
                    ? t("{size} · {calls} 次调用 · {traces} 条追踪", {
                      size: formatBytes(storage.bytes),
                      calls: storage.call_count,
                      traces: storage.trace_count,
                    })
                    : t("计算中…")}</small>
                </div>
                <button
                  type="button"
                  className={styles.textButton}
                  onClick={() => { setClearScope("details"); setConfirmClear(true); }}
                >
                  {t("清理存储空间")}
                </button>
              </div>
            </TitledCard>
          </div>
        </div>

        <SectionHeading eyebrow={groups[3].label} title={groups[3].hint} id="settings-models" />
        <div className={styles.cards}>
          <div data-card="commit" ref={registerCard("commit")}><CommitSettingsCard /></div>
          <div data-card="import" ref={registerCard("import")}>
            <LegacyModelImport>{({ busy, previewing, open }) => <TitledCard title={t("导入")} description={t("从本机旧版配置读取模型")}>
              <div className={styles.settingRow}>
                <div>
                  <strong>{t("旧版配置")}</strong>
                  <small>{t("从本机旧版配置读取模型；确认前会显示新增和已存在的模型。")}</small>
                </div>
                <Button size="small" disabled={busy} onClick={open}>
                  {previewing ? t("读取中…") : t("查看并导入")}
                </Button>
              </div>
            </TitledCard>}</LegacyModelImport>
          </div>
        </div>
      </ScrollableContent>
    </div>
  );

  return <>
    <PageContent
      fixed
      title={<PageTitle title={t("设置")} meta={t("{count} 项，按用途分组", { count: cards.length })} />}
      contentClassName={styles.pageContent}
      sections={[{ key: "settings", estimatedHeight: 1200, content }]}
    />
    <ConfirmDialog
      open={confirmClear}
      title={clearTitle}
      busy={clearing}
      cancelLabel={t("取消")}
      confirmLabel={t("确认清理")}
      onCancel={() => setConfirmClear(false)}
      onConfirm={() => void clearStorage()}
    >
      <div className={styles.confirmContent}>
        <Select
          value={clearScope}
          ariaLabel={t("清理范围")}
          options={[
            { value: "details", label: t("仅清理详细记录") },
            { value: "all", label: t("清理全部统计数据") },
          ]}
          onChange={(value) => setClearScope(value as StatisticsStorageScope)}
        />
        <small>{clearDescription}</small>
      </div>
    </ConfirmDialog>
  </>;
}
