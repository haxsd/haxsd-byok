import { useState, type ReactNode } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { PageContent } from "../../shell/layout/PageContent";
import { PageActions } from "../../shell/PageActions";
import { setLocalePreference, useI18n, type LocalePreference } from "../../i18n/store";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { themeOptions } from "../../shared/theme/theme";
import { Button } from "../../shared/ui/Button";
import { PageTitle } from "../../shared/ui/PageTitle";
import { SearchInput } from "../../shared/ui/SearchInput";
import { Select } from "../../shared/ui/Select";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { ScrollableContent } from "../../shared/virtual/ScrollableContent";
import { AppLifecycleSettingsCard } from "./AppLifecycleSettingsCard";
import { DiagnosticsCard } from "./DiagnosticsCard";
import { NetworkSettingsCard } from "./NetworkSettingsCard";
import { PortSettingsCard } from "./PortSettingsCard";
import { PricingSettingsCard } from "./PricingSettingsCard";
import { StorageSettingsCard } from "./StorageSettingsCard";
import { UpdateCard } from "./UpdateCard";
import { settingsDestinations, settingsSections, visibleSettingsSections, type SettingsItemId } from "./settingsNavigation";
import styles from "./SettingsPage.module.scss";

export function SettingsPage() {
  const { detailed, developerMode, theme } = useAppStore();
  const { preference } = useI18n();
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const [saving, setSaving] = useState<"recording" | "developer" | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const sections = settingsSections(developerMode);
  const selected = sections.find(section => section.id === params.get("section"))?.id ?? "general";
  const query = params.get("q") ?? "";
  const shown = visibleSettingsSections(sections, selected, query);
  const visibleItems = new Set(shown.flatMap(section => section.items.map(item => item.id)));
  const select = (section: string) => setParams({ section }, { replace: true });
  const updateRecording = async (enabled: boolean) => {
    setSaving("recording");
    await appStore.updateDetailed(enabled);
    setSaving(null);
  };
  const updateDeveloper = async (enabled: boolean) => {
    setSaving("developer");
    if (await appStore.updateDeveloperMode(enabled)) setStatus(t("已保存，日志级别修改需重启软件。"));
    setSaving(null);
  };
  const items: Record<SettingsItemId, ReactNode> = {
    appearance: <TitledCard title={t("外观与语言")}>
      <div className={styles.settingRow}>
        <strong>{t("界面语言")}</strong>
        <div className={styles.languageControl}><Select value={preference} ariaLabel={t("界面语言")} options={[
          { value: "system", label: t("跟随系统") }, { value: "zh-CN", label: "简体中文" }, { value: "en-US", label: "English" },
        ]} onChange={value => setLocalePreference(value as LocalePreference)} /></div>
      </div>
      <div className={styles.settingRow}>
        <strong>{t("主题")}</strong>
        <div className={styles.themeOptions}>{themeOptions.map(({ id }) => <button type="button" key={id} className={styles.themeOption} aria-pressed={theme === id} onClick={() => appStore.selectTheme(id)}>
          <span className={styles.themeSwatch} data-theme-id={id} aria-hidden="true" />
          <span className={styles.themeLabel}>{id === "midnight" ? t("午夜靛蓝") : id === "default-dark" ? t("默认暗色") : t("默认亮色")}</span>
        </button>)}</div>
      </div>
    </TitledCard>,
    app: <AppLifecycleSettingsCard />,
    proxy: <NetworkSettingsCard />,
    recording: <TitledCard title={t("调用记录")}>
      <div className={styles.settingRow}>
        <div><strong>{t("保存请求与响应")}</strong><small>{t("可能包含对话及敏感内容，并增加磁盘占用。关闭时只保存时间、状态与用量。")}</small></div>
        <Switch checked={detailed} disabled={saving !== null} label={t("保存请求与响应")} onChange={value => void updateRecording(value)} />
      </div>
    </TitledCard>,
    pricing: <PricingSettingsCard />,
    storage: <StorageSettingsCard />,
    developer: <TitledCard title={t("开发者模式")}>
      <div className={styles.settingRow}>
        <div><strong>{t("显示开发者信息")}</strong><small>{t("显示内部标识、协议字段和端口设置；详细日志需重启后生效。")}</small></div>
        <Switch checked={developerMode} disabled={saving !== null} label={t("开发者模式")} onChange={value => void updateDeveloper(value)} />
      </div>
      {status && <div className={styles.inlineStatus} role="status">{status}</div>}
    </TitledCard>,
    ports: <PortSettingsCard />,
    diagnostics: <DiagnosticsCard />,
    update: <UpdateCard />,
  };
  const destinations = settingsDestinations(query);

  const content = <div className={styles.page}>
    <nav className={styles.categories} aria-label={t("设置分类")}>
      {sections.map(section => <button type="button" key={section.id} className={styles.category} aria-current={!query && selected === section.id ? "page" : undefined} onClick={() => select(section.id)}>{section.label}</button>)}
    </nav>
    <ScrollableContent className={styles.content} contentClassName={styles.contentInner} resetKey={`${selected}|${query}`}>
      {/* Keep drafts mounted when changing categories or searching; hidden regions are not visible or focusable. */}
      {sections.map(section => <section key={section.id} hidden={!shown.some(item => item.id === section.id)} className={styles.cards} aria-label={section.label}>
        {query.trim() && <h2 className={styles.resultsHeading}>{section.label}</h2>}
        {section.items.map(item => <div key={item.id} hidden={!visibleItems.has(item.id)}>{items[item.id]}</div>)}
      </section>)}
      {destinations.map(item => <div key={item.label} className={styles.settingRow}><strong>{item.label}</strong><Button size="small" onClick={() => void navigate(item.path)}>{t("打开对应页面")}</Button></div>)}
      {shown.length === 0 && destinations.length === 0 && <div className={styles.empty}><strong>{t("没有匹配的设置")}</strong><Button size="small" onClick={() => setParams({ section: selected }, { replace: true })}>{t("清除筛选")}</Button></div>}
    </ScrollableContent>
  </div>;
  return <>
    <PageActions><SearchInput className={styles.search} value={query} ariaLabel={t("搜索设置")} placeholder={t("搜索设置…")} onValueChange={value => setParams(value ? { section: selected, q: value } : { section: selected }, { replace: true })} /></PageActions>
    <PageContent fixed title={<PageTitle title={t("设置")} />} contentClassName={styles.pageContent} sections={[{ key: "settings", estimatedHeight: 720, content }]} />
  </>;
}
