import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, type ClaudeCliTakeover, type ClaudeDesktopTakeover, type ClaudeSettings, type ClaudeTakeoverConflict } from "../../shared/api";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { PageTitle } from "../../shared/ui/PageTitle";
import { StatusPill } from "../../shared/ui/StatusPill";
import { Tabs } from "../../shared/ui/Tabs";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { terminalIcon } from "../../shared/ui/icons";
import { PageActions } from "../../shell/PageActions";
import { PageContent } from "../../shell/layout/PageContent";
import { ClaudeCliTab } from "./ClaudeCliTab";
import { ClaudeDesktopTab } from "./ClaudeDesktopTab";
import { ClaudeGatewayCard } from "./ClaudeGatewayCard";
import {
  claudeActionGate,
  claudeConnectionView,
  claudeTokenRotationGate,
  claudeUndoKept,
} from "./claudeConnectionView";
import { claudeDesktopModelDraft } from "./claudeTakeoverPlan";
import styles from "./ClaudeSettingsPage.module.scss";

type ErrorScope = "gateway" | "cli" | "desktop";
type BusyAction = "save" | "apply" | "undo" | "rotate";
type Busy = { scope: ErrorScope; action: BusyAction };

/**
 * Claude 接入页：一个网关、两个客户端。
 *
 * 网关设置是共享的，两个 tab 只负责各自的接管；设置与接管状态分开读：设置是页面的
 * 草稿（保存前不影响结论），状态与接管结果由 store 统一刷新，顶栏计数也读同一份。
 */
export function ClaudeSettingsPage() {
  const navigate = useNavigate();
  const { models, claudeStatus, claudeTakeover } = useAppStore();
  const [saved, setSaved] = useState<ClaudeSettings | null>(null);
  const [draft, setDraft] = useState<ClaudeSettings | null>(null);
  const [desktopModels, setDesktopModels] = useState<string[]>([]);
  // 撤销不保证全部还原：服务端保留的条目要留在页面上，不能只弹一句「已撤销」。
  const [cliKept, setCliKept] = useState<ClaudeTakeoverConflict[]>([]);
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
  const gate = claudeActionGate("cli", { settings: saved, status: claudeStatus, takeover: claudeTakeover, dirty });
  const desktopGate = claudeActionGate("desktop", {
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
      // 服务端自己把新令牌写回已接管的 CLI（`cli_synced`）；这里不再二次应用接管，
      // 否则会用页面上的旧设置覆盖掉刚轮换的令牌。
      const { cliSynced } = await api.rotateClaudeToken();
      message(cliSynced ? t("token 已重新生成，CLI 接管已同步") : t("token 已重新生成"));
    });
  };

  const applyCli = () => {
    if (!saved) return;
    void run({ scope: "cli", action: "apply" }, async () => {
      await api.applyClaudeCli({
        sonnet_model_hash: saved.sonnet_model_hash ?? undefined,
        opus_model_hash: saved.opus_model_hash ?? undefined,
        haiku_model_hash: saved.haiku_model_hash ?? undefined,
      });
      setCliKept([]);
      message(t("Claude Code 接管已应用，请重新打开 Claude Code"));
    });
  };

  const undoCli = () => {
    void run({ scope: "cli", action: "undo" }, async () => {
      const status: ClaudeCliTakeover = await api.undoClaudeCli();
      const kept = claudeUndoKept(status.conflicts);
      setCliKept(kept);
      message(kept.length
        ? t("已撤销；保留了 {count} 项接管后的改动", { count: kept.length })
        : t("Claude Code 接管已撤销"));
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
      title={t("接管客户端")}
      description={t("两个客户端分别接管；网关设置共用")}
      icon={terminalIcon}
      action={busy?.scope === "gateway"
        ? <StatusPill tone="warn">{t("保存中…")}</StatusPill>
        : dirty ? <StatusPill tone="warn">{t("有未保存的改动")}</StatusPill> : undefined}
    >
      <div className={styles.tabsBody}>
        <Tabs items={[
          {
            value: "cli",
            label: "Claude Code CLI",
            content: <ClaudeCliTab
              saved={saved}
              draft={draft}
              status={claudeStatus}
              takeover={claudeTakeover?.cli ?? null}
              models={models}
              gate={gate}
              busy={busy !== null}
              error={error?.scope === "cli" ? error.cause : null}
              keptAfterUndo={cliKept}
              onChange={setDraft}
              onApply={applyCli}
              onUndo={undoCli}
              onCopy={copy}
              onOpenModels={() => void navigate("/models")}
            />,
          },
          {
            value: "desktop",
            label: "Claude Desktop",
            content: <ClaudeDesktopTab
              draft={draft}
              status={claudeStatus}
              takeover={claudeTakeover?.desktop ?? null}
              models={models}
              desktopModels={desktopModels}
              gate={desktopGate}
              busy={busy !== null}
              error={error?.scope === "desktop" ? error.cause : null}
              keptAfterUndo={desktopKept}
              onDesktopModelsChange={setDesktopModels}
              onApply={applyDesktop}
              onUndo={undoDesktop}
              onCopy={copy}
            />,
          },
        ]} />
      </div>
    </TitledCard>
  </div>;

  return <>
    <PageActions>
      <Button
        disabled={!draft || !dirty || busy !== null}
        onClick={() => void save()}
      >{busy?.action === "save" ? t("保存中…") : t("保存")}</Button>
    </PageActions>
    <PageContent
      title={<PageTitle
        title="Claude"
        status={<StatusPill tone={view.tone}>{view.label}</StatusPill>}
        meta={t("把 Claude Code 与 Claude Desktop 的模型请求接到本机网关")}
      />}
      sections={[{ key: "claude", estimatedHeight: 980, content }]}
    />
  </>;
}
