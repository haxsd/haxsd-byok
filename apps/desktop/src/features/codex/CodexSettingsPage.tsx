import { useEffect, useState } from "react";
import { api, type CodexSettings } from "../../shared/api";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { TitledCard } from "../../shared/ui/TitledCard";
import { StatusHero } from "../../shared/ui/StatusHero";
import { PageTitle } from "../../shared/ui/PageTitle";
import { useMessage } from "../../shared/ui/message";
import { PageContent } from "../../shell/layout/PageContent";
import { PageActions } from "../../shell/PageActions";
import { CodexSettingsForm } from "./CodexSettingsForm";
import { codexConnectionView } from "./codexConnectionView";
import styles from "./CodexSettingsPage.module.scss";

export function CodexSettingsPage() {
  const { models, codexStatus } = useAppStore();
  const [saved, setSaved] = useState<CodexSettings | null>(null);
  const [draft, setDraft] = useState<CodexSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const message = useMessage();
  useEffect(() => {
    let disposed = false;
    void api.codexSettings().then(settings => { if (!disposed) { setSaved(settings); setDraft(settings); } })
      .catch(cause => { if (!disposed) setError(String(cause)); });
    return () => { disposed = true; };
  }, []);
  const view = codexConnectionView(codexStatus);
  const dirty = JSON.stringify(saved) !== JSON.stringify(draft);
  const locked = Boolean(codexStatus?.configured || codexStatus?.config_conflict);
  const action = async (operation: () => Promise<void>) => {
    setBusy(true); setError(null);
    try { await operation(); await appStore.refresh(); }
    catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setBusy(false); }
  };
  const save = () => action(async () => {
    if (!draft) return;
    const settings = await api.setCodexSettings(draft);
    setSaved(settings); setDraft(settings); message(t("Codex 设置已保存"));
  });
  const apply = () => action(async () => { await api.applyCodex(); message(t("Codex 配置已应用，请重新打开 Codex 或新建会话")); });
  const restore = () => action(async () => { await api.restoreCodex(); message(t("Codex 配置已恢复")); });
  return <>
    <PageActions>
      <Button disabled={busy || !dirty || locked || !draft} onClick={() => void save()}>{t("保存")}</Button>
      <Button disabled={busy || dirty || !saved?.enabled || !codexStatus?.listening || codexStatus.config_conflict} onClick={() => void apply()}>{t("应用到 Codex")}</Button>
      <Button disabled={busy || !codexStatus?.configured} onClick={() => void restore()}>{t("撤销接管")}</Button>
    </PageActions>
    <PageContent title={<PageTitle title="Codex" />} sections={[
      { key: "status", estimatedHeight: 240, content: <StatusHero connected={view.state === "taken-over"} title={view.label}
        description={t("保留原版 Codex 的工具、终端、审批、沙箱和任务循环，只替换模型推理。")}
        stages={[
          { key: "codex", label: "Codex", detail: codexStatus?.configured ? t("配置已应用") : t("尚未应用配置"), state: codexStatus?.configured ? "up" : "down" },
          { key: "gateway", label: t("本机网关"), detail: codexStatus?.listening ? t("运行中") : t("未监听"), state: codexStatus?.listening ? "up" : "down" },
          { key: "model", label: t("自配模型"), detail: t("从共享模型库选择"), state: saved?.model_hashes.length ? "up" : "down" },
        ]} steps={[]} footnotes={<div className={styles.facts}>
          <span>{t("配置目录")}：{codexStatus?.home ?? t("读取中…")}</span>
          <span>{t("已记录 {count} 次 Codex 调用", { count: codexStatus?.calls ?? 0 })}</span>
        </div>} /> },
      { key: "settings", estimatedHeight: 460, content: <TitledCard title={t("接入设置")}>
        {draft ? <CodexSettingsForm draft={draft} models={models} locked={locked || busy} onChange={setDraft} /> : <p>{error ? t("读取失败") : t("加载中…")}</p>}
      </TitledCard> },
      { key: "boundary", estimatedHeight: 240, content: <TitledCard title={t("使用边界")}>
        <p>{t("支持 Responses、Chat Completions 和 Anthropic API 模型。工具仍由 Codex 执行，不会改动登录、MCP、技能或审批设置。")}</p>
        <p>{t("上下文压缩由 Codex 在本地发起；官方托管搜索、远程压缩和实时语音不包含在此接入中。")}</p>
        <p>{t("切换模型或端口前先撤销接管，再保存并重新应用。配置冲突时保留外部修改，不强制覆盖。")}</p>
        {(error || codexStatus?.last_error) && <p className={styles.error} role="alert">{error || codexStatus?.last_error}</p>}
      </TitledCard> },
    ]} />
  </>;
}
