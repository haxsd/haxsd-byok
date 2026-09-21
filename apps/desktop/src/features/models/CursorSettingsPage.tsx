import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "../../shared/api";
import { CursorCaGate } from "./CursorGates";
import styles from "./CursorSettings.module.scss";
import { PageContent } from "../../shell/layout/PageContent";
import { PageActions } from "../../shell/PageActions";
import { Button } from "../../shared/ui/Button";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { Switch } from "../../shared/ui/Switch";
import { TooltipTrigger } from "../../shared/ui/TooltipTrigger";
import { useMessage } from "../../shared/ui/message";
import { appStore, useAppStore } from "../../shared/store/appStore";

/**
 * Cursor's own surface: taking over the local proxy and trusting the CA that makes
 * HTTPS interception possible.
 *
 * Model configuration is not here. Models are shared with Devin, so they live in
 * the model library; Cursor and Devin stay parallel modules that meet only in that
 * library and in the call statistics.
 */
export function CursorSettingsPage() {
  const { cursorHarness, cursorBusy, models } = useAppStore();
  const navigate = useNavigate();
  const message = useMessage();
  const [caCommand, setCaCommand] = useState<string | null>(null);
  const [waitingForCaRefresh, setWaitingForCaRefresh] = useState(false);
  const [confirmDisableTakeover, setConfirmDisableTakeover] = useState(false);

  const caReady = cursorHarness?.ca === "ready";
  const cursorTakenOver = cursorHarness?.settings_applied ?? false;
  const takeoverLabel = cursorTakenOver ? t("关闭接管Cursor") : t("开启接管Cursor");

  useEffect(() => {
    if (caCommand) void api.copyCursorText(caCommand);
  }, [caCommand]);

  const initializeCa = async () => {
    const status = await appStore.initializeCursorCa();
    if (status?.ca === "untrusted" && status.ca_install_command) setCaCommand(status.ca_install_command);
  };
  const refreshCa = async () => {
    await appStore.refresh();
    if (appStore.getSnapshot().cursorHarness?.ca !== "ready") setWaitingForCaRefresh(false);
  };
  const openCaTerminal = () => {
    if (caCommand) void api.openCursorCaInstallTerminal(caCommand).catch((cause) => message(cause instanceof Error ? cause.message : String(cause)));
    setCaCommand(null);
    setWaitingForCaRefresh(true);
  };

  const content = <div className={styles.page}>
    <CursorCaGate busy={cursorBusy} waitingForRefresh={waitingForCaRefresh} onInitialize={() => void initializeCa()} onRefresh={() => void refreshCa}>
      <div className={styles.editor}>
        <strong>{t("接管状态")}</strong>
        <span>{cursorTakenOver
          ? t("Cursor 的请求正经过本机代理，模型来自模型库。")
          : t("打开上方的开关即可让 Cursor 的请求经过本机代理。")}</span>
        <span>{t("当前模型库里有 {count} 个模型。", { count: models.length })}</span>
      </div>
    </CursorCaGate>
  </div>;

  return <>
    <PageActions position="left">
      <div className={styles.takeoverActions}>
        <span className={styles.takeoverStatus}>{cursorTakenOver ? t("已接管") : t("未接管")}</span>
        <TooltipTrigger label={takeoverLabel}>
          <Switch
            checked={cursorTakenOver}
            disabled={cursorBusy || (!cursorTakenOver && !caReady)}
            label={takeoverLabel}
            onChange={(enabled) => {
              if (enabled) void appStore.setCursorEnabled(true);
              else setConfirmDisableTakeover(true);
            }}
          />
        </TooltipTrigger>
      </div>
    </PageActions>
    <PageActions>
      <Button size="small" onClick={() => navigate("/models")}>{t("管理模型")}</Button>
    </PageActions>
    <PageContent title="Cursor" sections={[{ key: "cursor-settings", estimatedHeight: 380, content }]} />
    <ConfirmDialog
      open={confirmDisableTakeover}
      title={t("关闭接管Cursor？")}
      cancelLabel={t("取消")}
      confirmLabel={t("关闭接管")}
      onCancel={() => setConfirmDisableTakeover(false)}
      onConfirm={() => {
        setConfirmDisableTakeover(false);
        void appStore.setCursorEnabled(false);
      }}
    >
      <p>{t("关闭后将移除 Cursor 本地代理配置。如果你需要登陆官方账号，通常不需要关闭操作，推荐直接登陆你的账号即可(byok模型与官方账号的模型已支持无缝衔接)，是否继续关闭并清理代理？")}</p>
    </ConfirmDialog>
    <ConfirmDialog open={caCommand !== null} title={t("安装本地 CA")} cancelLabel={t("关闭")} confirmLabel={t("打开终端")} onCancel={() => setCaCommand(null)} onConfirm={openCaTerminal}>
      <div className={styles.editor}><strong>{t("需要授权安装证书")}</strong><span>{t("安装命令已自动复制。点击“打开终端”，将命令粘贴到终端中执行，并按提示输入密码。")}</span><pre className={styles.command}>{caCommand}</pre></div>
    </ConfirmDialog>
  </>;
}
