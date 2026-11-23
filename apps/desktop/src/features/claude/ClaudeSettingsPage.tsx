import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, type ClaudeDesktopTakeover, type ClaudeSettings, type ClaudeTakeoverConflict } from "../../shared/api";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { Card } from "../../shared/ui/Card";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { PageTitle } from "../../shared/ui/PageTitle";
import { StatusHero } from "../../shared/ui/StatusHero";
import { StatusPill } from "../../shared/ui/StatusPill";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { terminalIcon } from "../../shared/ui/icons";
import { PageContent } from "../../shell/layout/PageContent";
import { ClaudeDesktopTab } from "./ClaudeDesktopTab";
import { ClaudeGatewayCard } from "./ClaudeGatewayCard";
import { HarnessCapabilities } from "../harness/HarnessCapabilities";
import {
  claudeActionGate,
  claudeConnectionView,
  claudeTakeoverDescription,
  claudeTakeoverStateLabel,
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
  const [confirmForce, setConfirmForce] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);
  const desktopInitialized = useRef(false);
  const message = useMessage();
  const navigate = useNavigate();

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
  const modelCount = claudeStatus?.models ?? 0;
  const desktopState = claudeTakeover?.desktop.state ?? null;

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
      message(t("Claude 接管已应用，请重新打开 Claude"));
    });
  };

  const undoDesktop = () => {
    void run({ scope: "desktop", action: "undo" }, async () => {
      const status: ClaudeDesktopTakeover = await api.undoClaudeDesktop();
      const kept = claudeUndoKept(status.conflicts);
      setDesktopKept(kept);
      message(kept.length
        ? t("已撤销；保留了 {count} 项接管后的改动", { count: kept.length })
        : t("Claude 接管已撤销"));
    });
  };

  const forceDesktop = () => {
    setConfirmForce(false);
    void run({ scope: "desktop", action: "apply" }, async () => {
      await api.applyClaudeDesktop(desktopModels.map((model_hash) => ({ model_hash })), true);
      setDesktopKept([]);
      message(t("已强制接管 Claude，请重新打开 Claude"));
    });
  };

  const primaryAction = (() => {
    if (!draft || !saved) return { label: t("读取中…"), disabled: true, run: () => undefined };
    if (dirty) return { label: t("保存"), disabled: busy !== null, run: save };
    if (!saved.enabled || !saved.default_model_hash) {
      return { label: t("保存"), disabled: true, run: () => undefined };
    }
    if (view.state === "taken-over") {
      return { label: t("查看调用记录"), disabled: false, run: () => void navigate("/calls") };
    }
    if (gate.apply) {
      return { label: t("应用到 Claude"), disabled: busy !== null, run: applyDesktop };
    }
    return { label: t("重新检查"), disabled: busy !== null, run: () => void appStore.refresh() };
  })();

  const copy = (text: string, done: string) => {
    if (!text) return;
    void api.copyCursorText(text)
      .then(() => message(done))
      .catch((cause) => message.error(cause));
  };

  const content = <div className={styles.page}>
    <Card as="section">
      <StatusHero
        connected={view.state === "taken-over"}
        title={view.label}
        description={!claudeStatus
          ? t("正在读取网关状态…")
          : !claudeStatus.enabled
            ? t("先启用并保存 Claude 网关。")
            : !claudeStatus.listening
              ? t("网关还没有监听；检查端口后保存设置。")
              : desktopState === "applied"
                ? t("模型请求已走本机网关。")
                : claudeTakeoverDescription(desktopState)}
        stages={[
          {
            key: "claude",
            label: "Claude Desktop",
            detail: desktopState ? claudeTakeoverStateLabel(desktopState) : t("读取中…"),
            state: desktopState === "applied" ? "up" : desktopState === "conflict" || desktopState === "stale" ? "down" : "unknown",
          },
          {
            key: "gateway",
            label: t("本机网关"),
            detail: claudeStatus?.listening ? t("运行中") : claudeStatus?.enabled ? t("未监听") : t("未启用"),
            state: claudeStatus?.listening ? "up" : "down",
          },
          {
            key: "models",
            label: t("模型库"),
            detail: modelCount ? t("{count} 个模型", { count: modelCount }) : t("还没有模型"),
            state: modelCount ? "up" : "unknown",
          },
        ]}
        steps={[]}
        action={<>
          {claudeTakeover?.desktop.state === "conflict" && <Button size="small" disabled={busy !== null || claudeTakeover.desktop.running} onClick={() => setConfirmForce(true)}>{t("强制接管")}</Button>}
          {gate.undo && <Button size="small" disabled={busy !== null} onClick={undoDesktop}>{t("撤销 Claude 接管")}</Button>}
          <Button size="small" variant="primary" disabled={primaryAction.disabled} onClick={primaryAction.run}>{primaryAction.label}</Button>
        </>}
      />
    </Card>
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
      title={t("接管 Claude")}
      description={t("只写 Claude 自己的第三方推理 profile；网关设置共用")}
      icon={terminalIcon}
      collapsible
      storageKey="claude-desktop"
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
    <HarnessCapabilities client="claude" collapsible />
  </div>;

  return <>    <ConfirmDialog
      open={confirmForce}
      destructive
      title={t("强制接管 Claude？")}
      cancelLabel={t("取消")}
      confirmLabel={t("覆盖并接管")}
      busy={busy !== null}
      onCancel={() => setConfirmForce(false)}
      onConfirm={forceDesktop}
    >
      <p>{t("这会覆盖被改动的 Claude 接管 profile，并保留原配置备份；撤销接管时恢复。请先完全退出 Claude。")}</p>
    </ConfirmDialog>
    <PageContent
      title={<PageTitle
        title="Claude"
        status={<StatusPill tone={view.tone}>{view.label}</StatusPill>}
        meta={t("把 Claude 的模型请求接到本机网关")}
      />}
      sections={[{ key: "claude", estimatedHeight: 1080, content }]}
    />
  </>;
}
