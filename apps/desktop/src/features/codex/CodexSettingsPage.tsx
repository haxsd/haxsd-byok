import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, configuredPluginModels, type CodexSettings } from "../../shared/api";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { TitledCard } from "../../shared/ui/TitledCard";
import { StatusHero } from "../../shared/ui/StatusHero";
import { StatusPill } from "../../shared/ui/StatusPill";
import { PageTitle } from "../../shared/ui/PageTitle";
import { useMessage } from "../../shared/ui/message";
import { PageContent } from "../../shell/layout/PageContent";
import { PageActions } from "../../shell/PageActions";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { clientErrorText } from "../harness/harnessErrorText";
import { CodexConflictCard } from "./CodexConflictCard";
import { CodexSettingsForm } from "./CodexSettingsForm";
import {
  codexActionGate,
  codexApplyNeedsSignInConfirm,
  codexClientModeView,
  codexConnectionView,
  codexLoginView,
  codexOutstandingSteps,
  codexVerdictDescription,
} from "./codexConnectionView";
import { HarnessCapabilities } from "../harness/HarnessCapabilities";
import { providerIcon, settingsIcon } from "../../shared/ui/icons";
import styles from "./CodexSettingsPage.module.scss";

export function CodexSettingsPage() {
  const { models, plugins, codexStatus } = useAppStore();
  const navigate = useNavigate();
  const [saved, setSaved] = useState<CodexSettings | null>(null);
  const [draft, setDraft] = useState<CodexSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  // 未登录 ChatGPT 时点「应用到 Codex」先确认：接管后客户端可能不再提供登录入口。
  const [confirmApply, setConfirmApply] = useState(false);
  const message = useMessage();
  // 模型库里的插件模型只是为了让设置页说清「为什么它们不能选」，不进入 Codex 目录。
  const pluginModels = configuredPluginModels(plugins);
  useEffect(() => {
    let disposed = false;
    void api.codexSettings().then(settings => { if (!disposed) { setSaved(settings); setDraft(settings); } })
      .catch(cause => { if (!disposed) setError(cause); });
    return () => { disposed = true; };
  }, []);
  const view = codexConnectionView(codexStatus);
  const login = codexLoginView(codexStatus);
  const clientMode = codexClientModeView(codexStatus);
  const modelCount = saved?.model_hashes.length ?? 0;
  const steps = codexOutstandingSteps(codexStatus, modelCount, models.length);
  const dirty = JSON.stringify(saved) !== JSON.stringify(draft);
  const locked = Boolean(codexStatus?.configured || codexStatus?.config_conflict);
  // 两个按钮的可用条件集中在 codexConnectionView 里：撤销看接管凭证（restorable），
  // 应用只被「路由被别人占着」挡住，受管字段被改过不挡。
  const gate = codexActionGate(codexStatus, { dirty, enabled: Boolean(saved?.enabled) });
  const action = async (operation: () => Promise<void>) => {
    setBusy(true); setError(null);
    try { await operation(); await appStore.refresh(); }
    catch (cause) { setError(cause); }
    finally { setBusy(false); }
  };
  const save = () => action(async () => {
    if (!draft) return;
    const settings = await api.setCodexSettings(draft);
    setSaved(settings); setDraft(settings); message(t("Codex 设置已保存"));
  });
  const apply = () => action(async () => { await api.applyCodex(); message(t("Codex 配置已应用。请从“文件 → 退出”完全退出 Codex 后重开；只关闭窗口不会重新读取界面模式。")); });
  const requestApply = () => {
    if (codexApplyNeedsSignInConfirm(codexStatus)) {
      setConfirmApply(true);
      return;
    }
    void apply();
  };
  const restore = () => action(async () => { await api.restoreCodex(); message(t("Codex 配置已恢复")); });
  const content = <div className={styles.page}>
    <TitledCard
      title={t("接入状态")}
      icon={providerIcon}
      action={<Button size="small" onClick={() => void navigate("/calls")}>{t("查看调用记录")}</Button>}
    >
      <StatusHero
        connected={view.state === "taken-over"}
        title={view.label}
        description={codexVerdictDescription(view.state)}
        stages={[
          { key: "codex", label: "Codex", detail: codexStatus?.configured ? t("指向本机网关") : t("未写入配置"), state: codexStatus?.configured ? "up" : "down" },
          { key: "gateway", label: t("本机网关"), detail: codexStatus?.listening ? t("运行中") : t("未监听"), state: codexStatus?.listening ? "up" : "down" },
          { key: "models", label: t("模型库"), detail: modelCount ? t("{count} 个模型", { count: modelCount }) : t("未选择模型"), state: modelCount ? "up" : "unknown" },
        ]}
        steps={steps}
        aside={<div className={styles.aside}>
          <StatusPill tone={login.tone} title={login.title}>{login.label}</StatusPill>
          {clientMode && <StatusPill tone={clientMode.tone} title={clientMode.title}>{clientMode.label}</StatusPill>}
        </div>}
        footnotes={<div className={styles.facts}>
          <span>{t("已记录 {count} 次调用", { count: codexStatus?.calls ?? 0 })}</span>
          <span className={styles.home} title={codexStatus?.home}>{t("配置目录")}：{codexStatus?.home ?? t("读取中…")}</span>
        </div>}
      />
      {(error || codexStatus?.last_error) && <p className={styles.error} role="alert">{clientErrorText(error ?? codexStatus?.last_error)}</p>}
      {clientMode && codexStatus?.local_mode_guard && <p className={styles.note}>{t("如果 Codex 当前显示 Work 或英文界面，请从“文件 → 退出”完全退出后重开；只关闭窗口不会重新读取本地模式。")}</p>}
    </TitledCard>
    <CodexConflictCard status={codexStatus} />
    <TitledCard title={t("接入设置")} icon={settingsIcon}>
      {draft
        ? <CodexSettingsForm draft={draft} models={models} pluginModels={pluginModels} locked={locked || busy} onChange={setDraft} />
        : <p className={styles.note}>{error ? t("读取失败") : t("加载中…")}</p>}
    </TitledCard>
    <HarnessCapabilities client="codex" />
  </div>;
  return <>
    <PageActions>
      <Button disabled={busy || !dirty || locked || !draft} onClick={() => void save()}>{t("保存")}</Button>
      <Button disabled={busy || !gate.apply} onClick={requestApply}>{t("应用到 Codex")}</Button>
      <Button disabled={busy || !gate.restore} onClick={() => void restore()}>{t("撤销接管")}</Button>
    </PageActions>
    <ConfirmDialog
      open={confirmApply}
      title={t("还没登录 ChatGPT")}
      confirmLabel={t("仍然应用")}
      busy={busy}
      onCancel={() => setConfirmApply(false)}
      onConfirm={() => { setConfirmApply(false); void apply(); }}
    >
      <p>{t("接管后客户端可能不再提供登录入口。建议先登录；未登录时自动使用 Codex 本地模式。")}</p>
    </ConfirmDialog>
    <PageContent
      title={<PageTitle
        title="Codex"
        status={<StatusPill tone={view.tone}>{view.label}</StatusPill>}
        meta={t("模型请求走本机网关，工具与审批仍由 Codex 执行")}
      />}
      sections={[{ key: "codex", estimatedHeight: 820, content }]}
    />
  </>;
}
