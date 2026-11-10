import { useEffect, useRef, useState } from "react";
import { api, type ClaudeDesktopTakeover, type ClaudeSettings, type ClaudeTakeoverConflict } from "../../shared/api";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { PageTitle } from "../../shared/ui/PageTitle";
import { StatusPill } from "../../shared/ui/StatusPill";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { terminalIcon } from "../../shared/ui/icons";
import { PageActions } from "../../shell/PageActions";
import { PageContent } from "../../shell/layout/PageContent";
import { ClaudeDesktopTab } from "./ClaudeDesktopTab";
import { ClaudeGatewayCard } from "./ClaudeGatewayCard";
import { HarnessCapabilities } from "../harness/HarnessCapabilities";
import {
  claudeActionGate,
  claudeConnectionView,
  claudeTokenRotationGate,
  claudeUndoKept,
} from "./claudeConnectionView";
import { claudeDesktopModelDraft } from "./claudeTakeoverPlan";
import styles from "./ClaudeSettingsPage.module.scss";

type ErrorScope = "gateway" | "desktop";
type BusyAction = "save" | "apply" | "undo" | "rotate";
type Busy = { scope: ErrorScope; action: BusyAction };

/**
 * Claude 接入页：一个本机 Anthropic 网关，一个 Claude Desktop 接管。
 *
 * 网关设置是共享的；Desktop 接管只写它自己的 configLibrary profile。设置与接管状态
 * 分开读：设置是页面的草稿（保存前不影响结论），状态与接管结果由 store 统一刷新。
 */
export function ClaudeSettingsPage() {
  const { models, claudeStatus, claudeTakeover } = useAppStore();
  const [saved, setSaved] = useState<ClaudeSettings | null>(null);
  const [draft, setDraft] = useState<ClaudeSettings | null>(null);
  const [desktopModels, setDesktopModels] = useState<string[]>([]);
  // 撤销不保证全部还原：服务端保留的条目要留在页面上，不能只弹一句「已撤销」。
  const [desktopKept, setDesktopKept] = useState<ClaudeTakeoverConflict[]>([]);
  const [loadError, setLoadError] = useState<unknown>(null);
  const [error, setError] = useState<{ scope: ErrorScope; cause: unknown } | null>(null);
  const [busy, setBusy] = useState<Busy | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const desktopInitialized = useRef(false);
  const message = useMessage();

  useEffect(() => {
    let disposed = false;
    setLoadError(null);
    void api.claudeSettings()
      .then((settings) => {
        if (disposed) return;
        setSaved(settings);
        setDraft(settings);
      })
      .catch((cause) => { if (!disposed) setLoadError(cause); });
    return () => { disposed = true; };
  }, [reloadKey]);

  // Desktop 的模型顺序没有独立持久化字段：只在第一次读到设置时用它初始化，之后
  // 保存网关不会把用户调整过的顺序重置。
  useEffect(() => {
    if (!saved || desktopInitialized.current) return;
    desktopInitialized.current = true;
    setDesktopModels(claudeDesktopModelDraft(saved));
  }, [saved]);

  const dirty = Boolean(saved && draft && JSON.stringify(saved) !== JSON.stringify(draft));
  const view = claudeConnectionView(claudeStatus, claudeTakeover);
  const rotation = claudeTokenRotationGate(claudeTakeover);
  const gate = claudeActionGate({
    settings: saved,
    status: claudeStatus,
    takeover: claudeTakeover,
    dirty,
    desktopModels,
  });

  const run = async (next: Busy, operation: () => Promise<void>) => {
    setBusy(next);
    setError(null);
    try {
      await operation();
      await appStore.refresh();
    } catch (cause) {
      setError({ scope: next.scope, cause });
    } finally {
      setBusy(null);
    }
  };

  const save = () => {
    if (!draft) return;
    void run({ scope: "gateway", action: "save" }, async () => {
      const next = await api.setClaudeSettings(draft);
      setSaved(next);
      setDraft(next);
      message(t("Claude 设置已保存"));
    });
  };

  const rotateToken = () => {
    void run({ scope: "gateway", action: "rotate" }, async () => {
      await api.rotateClaudeToken();
      message(t("token 已重新生成"));
    });
  };

  const applyDesktop = () => {
    void run({ scope: "desktop", action: "apply" }, async () => {
      await api.applyClaudeDesktop(desktopModels.map((model_hash) => ({ model_hash })));
      setDesktopKept([]);
      message(t("Claude Desktop 接管已应用，请重新打开 Claude Desktop"));
    });
  };

  const undoDesktop = () => {
    void run({ scope: "desktop", action: "undo" }, async () => {
      const status: ClaudeDesktopTakeover = await api.undoClaudeDesktop();
      const kept = claudeUndoKept(status.conflicts);
      setDesktopKept(kept);
      message(kept.length
        ? t("已撤销；保留了 {count} 项接管后的改动", { count: kept.length })
        : t("Claude Desktop 接管已撤销"));
    });
  };

  const copy = (text: string, done: string) => {
    if (!text) return;
    void api.copyCursorText(text)
      .then(() => message(done))
      .catch((cause) => message.error(cause));
  };

  const content = <div className={styles.page}>
    <ClaudeGatewayCard
      saved={saved}
      draft={draft}
      status={claudeStatus}
      models={models}
      loadError={loadError}
      writeError={error?.scope === "gateway" ? error.cause : null}
      busy={busy?.scope === "gateway"}
      onChange={setDraft}
      onRetry={() => setReloadKey((key) => key + 1)}
      onCopy={copy}
      onRotate={rotateToken}
      rotating={busy?.scope === "gateway" && busy.action === "rotate"}
      rotateBlockedReason={rotation.reason}
    />
    <TitledCard
      title={t("接管 Claude Desktop")}
      description={t("只写 Desktop 自己的第三方推理 profile；网关设置共用")}
      icon={terminalIcon}
      action={busy?.scope === "gateway"
        ? <StatusPill tone="warn">{t("保存中…")}</StatusPill>
        : dirty ? <StatusPill tone="warn">{t("有未保存的改动")}</StatusPill> : undefined}
    >
      <div className={styles.settingsBody}>
        <ClaudeDesktopTab
          draft={draft}
          status={claudeStatus}
          takeover={claudeTakeover?.desktop ?? null}
          models={models}
          desktopModels={desktopModels}
          gate={gate}
          busy={busy !== null}
          error={error?.scope === "desktop" ? error.cause : null}
          keptAfterUndo={desktopKept}
          onDesktopModelsChange={setDesktopModels}
          onCopy={copy}
        />
      </div>
    </TitledCard>
    <HarnessCapabilities client="claude" />
  </div>;

  return <>
    <PageActions>
      <Button
        disabled={!draft || !dirty || busy !== null}
        onClick={() => void save()}
      >{busy?.action === "save" ? t("保存中…") : t("保存")}</Button>
      <Button
        disabled={busy !== null || !gate.apply}
        title={gate.applyBlockedReason ?? undefined}
        onClick={applyDesktop}
      >{t("应用到 Claude Desktop")}</Button>
      <Button
        disabled={busy !== null || !gate.undo}
        onClick={undoDesktop}
      >{t("撤销 Claude Desktop 接管")}</Button>
    </PageActions>
    <PageContent
      title={<PageTitle
        title="Claude"
        status={<StatusPill tone={view.tone}>{view.label}</StatusPill>}
        meta={t("把 Claude Desktop 的模型请求接到本机网关")}
      />}
      sections={[{ key: "claude", estimatedHeight: 1080, content }]}
    />
  </>;
}
