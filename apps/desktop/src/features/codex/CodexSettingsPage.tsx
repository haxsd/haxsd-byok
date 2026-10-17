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
import { clientErrorText } from "../harness/harnessErrorText";
import { CodexConflictCard } from "./CodexConflictCard";
import { CodexSettingsForm } from "./CodexSettingsForm";
import { codexActionGate, codexConnectionView } from "./codexConnectionView";
import styles from "./CodexSettingsPage.module.scss";

export function CodexSettingsPage() {
  const { models, plugins, codexStatus } = useAppStore();
  const navigate = useNavigate();
  const [saved, setSaved] = useState<CodexSettings | null>(null);
  const [draft, setDraft] = useState<CodexSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
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
  const apply = () => action(async () => { await api.applyCodex(); message(t("Codex 配置已应用，请重新打开 Codex 或新建会话")); });
  const restore = () => action(async () => { await api.restoreCodex(); message(t("Codex 配置已恢复")); });
  // 与 Cursor / Devin 接入页同一套：状态、冲突（有才出现）、设置、边界，各自的说明都留在卡片内。
  const content = <div className={styles.page}>
    <TitledCard
      title={t("接入状态")}
      action={<Button size="small" onClick={() => void navigate("/calls")}>{t("查看调用记录")}</Button>}
    >
      <StatusHero
        connected={view.state === "taken-over"}
        title={view.label}
        description={t("保留原版 Codex 的工具、终端、审批、沙箱和任务循环，只替换模型推理。")}
        stages={[
          { key: "codex", label: "Codex", detail: codexStatus?.configured ? t("配置已应用") : t("尚未应用配置"), state: codexStatus?.configured ? "up" : "down" },
          { key: "gateway", label: t("本机网关"), detail: codexStatus?.listening ? t("运行中") : t("未监听"), state: codexStatus?.listening ? "up" : "down" },
          { key: "models", label: t("模型库"), detail: saved?.model_hashes.length ? t("{count} 个模型", { count: saved.model_hashes.length }) : t("还没有模型"), state: saved?.model_hashes.length ? "up" : "unknown" },
        ]}
        steps={[]}
        footnotes={<div className={styles.facts}>
          <span>{t("配置目录")}：{codexStatus?.home ?? t("读取中…")}</span>
          <span>{t("已记录 {count} 次 Codex 调用", { count: codexStatus?.calls ?? 0 })}</span>
        </div>}
      />
    </TitledCard>
    <CodexConflictCard status={codexStatus} />
    <TitledCard title={t("接入设置")}>
      {draft
        ? <CodexSettingsForm draft={draft} models={models} pluginModels={pluginModels} locked={locked || busy} onChange={setDraft} />
        : <p className={styles.note}>{error ? t("读取失败") : t("加载中…")}</p>}
    </TitledCard>
    <TitledCard title={t("使用边界")}>
      <div className={styles.notes}>
        <p>{t("Codex 只发 Responses 请求；本机网关按模型的接口类型，把它翻译成 Chat Completions 或 Anthropic Messages 后转发给上游。工具仍由 Codex 执行，不会改动登录、MCP、技能或审批设置。")}</p>
        <p>{t("上下文压缩由 Codex 在本地发起；官方托管搜索、远程压缩和实时语音不包含在此接入中。")}</p>
        <p>{t("切换模型或端口前先撤销接管，再保存并重新应用。路由被别人占着时保留原文件，不强制覆盖。")}</p>
        {(error || codexStatus?.last_error) && <p className={styles.error} role="alert">{clientErrorText(error ?? codexStatus?.last_error)}</p>}
      </div>
    </TitledCard>
  </div>;
  return <>
    <PageActions>
      <Button disabled={busy || !dirty || locked || !draft} onClick={() => void save()}>{t("保存")}</Button>
      <Button disabled={busy || !gate.apply} onClick={() => void apply()}>{t("应用到 Codex")}</Button>
      <Button disabled={busy || !gate.restore} onClick={() => void restore()}>{t("撤销接管")}</Button>
    </PageActions>
    <PageContent
      title={<PageTitle
        title="Codex"
        status={<StatusPill tone={view.tone}>{view.label}</StatusPill>}
        meta={t("把 Codex 的模型请求指到本机网关，由模型库回答")}
      />}
      sections={[{ key: "codex", estimatedHeight: 900, content }]}
    />
  </>;
}
