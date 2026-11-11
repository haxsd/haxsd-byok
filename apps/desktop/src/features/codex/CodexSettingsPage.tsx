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
import { CodexSettingsForm, type CodexLockReason } from "./CodexSettingsForm";
import { codexModelAvailability, codexModelSelectionBlocked } from "./codexModelOptions";
import {
  codexActionGate,
  codexApplyNeedsSignInConfirm,
  codexClientStage,
  codexConnectionView,
  codexLoginView,
  codexOutstandingSteps,
  codexVerdictDescription,
} from "./codexConnectionView";
import { HarnessCapabilities } from "../harness/HarnessCapabilities";
import { providerIcon, settingsIcon } from "../../shared/ui/icons";
import styles from "./CodexSettingsPage.module.scss";

/**
 * 保存/应用/撤销共用的刷新语义：操作失败也要读取一次真实状态，避免卡片停在旧值；
 * 刷新失败只保留/暴露原错误，不能把失败伪装成成功。
 */
export async function runCodexAction(
  operation: () => Promise<void>,
  onError: (cause: unknown) => void,
): Promise<void> {
  try {
    await operation();
    await appStore.refresh();
  } catch (cause) {
    onError(cause);
    await appStore.refresh().catch(() => undefined);
  }
}

/** 页面共用的一条错误提示；测试也通过它验证真实 A→B 失败文案。 */
export function CodexErrorAlert({ cause }: { cause: unknown }) {
  return <p className={styles.error} role="alert">{clientErrorText(cause)}</p>;
}

export function CodexSettingsPage() {
  const { models, plugins, codexStatus } = useAppStore();
  const navigate = useNavigate();
  const [saved, setSaved] = useState<CodexSettings | null>(null);
  const [draft, setDraft] = useState<CodexSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [reloadKey, setReloadKey] = useState(0);
  // 未登录 ChatGPT 时点「应用到 Codex」先确认：Work 界面可能无法新建任务。
  const [confirmApply, setConfirmApply] = useState(false);
  const message = useMessage();
  // 模型库里的插件模型只是为了让设置页说清「为什么它们不能选」，不进入 Codex 目录。
  const pluginModels = configuredPluginModels(plugins);
  useEffect(() => {
    let disposed = false;
    setError(null);
    void api.codexSettings().then(settings => { if (!disposed) { setSaved(settings); setDraft(settings); setError(null); } })
      .catch(cause => { if (!disposed) setError(cause); });
    return () => { disposed = true; };
  }, [reloadKey]);
  const view = codexConnectionView(codexStatus);
  const login = codexLoginView(codexStatus);
  const clientStage = codexClientStage(codexStatus);
  const modelCount = saved?.model_hashes.length ?? 0;
  const modelAvailability = draft ? codexModelAvailability(models, pluginModels, draft.model_hashes) : null;
  const modelSelectionBlocked = modelAvailability ? codexModelSelectionBlocked(modelAvailability) : false;
  const steps = codexOutstandingSteps(codexStatus, modelCount, models.length);
  const dirty = JSON.stringify(saved) !== JSON.stringify(draft);
  const applied = Boolean(codexStatus?.configured && codexStatus.conflict_kind === "none");
  const locked = Boolean(codexStatus && codexStatus.conflict_kind !== "none");
  const lockReason: CodexLockReason = codexStatus?.conflict_kind === "unreadable"
    ? "unreadable"
    : codexStatus?.conflict_kind === "foreign_provider" && !codexStatus.restorable
      ? "foreign"
      : codexStatus?.restorable
        ? "restore"
        : "blocked";
  // 两个按钮的可用条件集中在 codexConnectionView 里：撤销看接管凭证（restorable），
  // 应用只允许在没有冲突/策略覆盖/半写状态时点击。
  const gate = codexActionGate(codexStatus, { dirty, enabled: Boolean(saved?.enabled) });
  const action = async (operation: () => Promise<void>) => {
    setBusy(true); setError(null);
    try { await runCodexAction(operation, setError); }
    catch (cause) { setError(cause); }
    finally { setBusy(false); }
  };
  const save = () => {
    if (!draft || modelSelectionBlocked) return;
    return action(async () => {
      const settings = await api.setCodexSettings(draft);
      setSaved(settings); setDraft(settings); message(t("Codex 设置已保存"));
    });
  };
  const apply = () => action(async () => { await api.applyCodex(); message(t("Codex 配置已应用。请完全退出 Codex 后重开，让客户端读取新的模型配置。")); });
  const requestApply = () => {
    if (codexApplyNeedsSignInConfirm(codexStatus)) {
      setConfirmApply(true);
      return;
    }
    void apply();
  };
  const restore = () => action(async () => { await api.restoreCodex(); message(t("Codex 配置已恢复")); });
  const alertCause = error ?? codexStatus?.last_error ?? codexStatus?.managed_error;
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
          { key: "codex", label: "Codex", detail: clientStage.detail, state: clientStage.state },
          { key: "gateway", label: t("本机网关"), detail: codexStatus?.listening ? t("运行中") : t("未监听"), state: codexStatus?.listening ? "up" : "down" },
          { key: "models", label: t("模型库"), detail: modelCount ? t("{count} 个模型", { count: modelCount }) : t("未选择模型"), state: modelCount ? "up" : "unknown" },
        ]}
        steps={steps}
        aside={<div className={styles.aside}>
          <StatusPill tone={login.tone} title={login.title}>{login.label}</StatusPill>
        </div>}
        footnotes={<div className={styles.facts}>
          <span>{t("已记录 {count} 次调用", { count: codexStatus?.calls ?? 0 })}</span>
          <span className={styles.home} title={codexStatus?.home}>{t("配置目录")}：{codexStatus?.home ?? t("读取中…")}</span>
          {codexStatus && <span title={t("cloud/enterprise 策略未在本地核对")}>{t("受管要求：仅本地系统")}</span>}
        </div>}
      />
      {alertCause && <CodexErrorAlert cause={alertCause} />}
    </TitledCard>
    <CodexConflictCard status={codexStatus} />
    <TitledCard title={t("接入设置")} icon={settingsIcon}>
      {draft
        ? <CodexSettingsForm draft={draft} models={models} pluginModels={pluginModels} locked={locked || busy} lockReason={lockReason} applied={applied} onChange={setDraft} />
        : error
          ? <div className={styles.note}><span>{t("读取失败")}</span><Button size="small" onClick={() => setReloadKey(key => key + 1)}>{t("重试")}</Button></div>
          : <p className={styles.note}>{t("加载中…")}</p>}
    </TitledCard>
    <HarnessCapabilities client="codex" />
  </div>;
  return <>
    <PageActions>
      <Button disabled={busy || !dirty || locked || !draft || modelSelectionBlocked} onClick={() => void save()}>{t("保存")}</Button>
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
      <p>{t("接管只替换模型路由，不修改 Codex 的登录方式或界面模式。未登录时 Work 界面可能无法新建任务，建议先登录。")}</p>
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
