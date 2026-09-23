import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "../../shared/api";
import { CursorCaGate } from "./CursorGates";
import styles from "./CursorSettings.module.scss";
import { PageContent } from "../../shell/layout/PageContent";
import { Button } from "../../shared/ui/Button";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import type { PathStage } from "../../shared/ui/ConnectionPath";
import { PageTitle } from "../../shared/ui/PageTitle";
import { StatusHero } from "../../shared/ui/StatusHero";
import { StatusPill } from "../../shared/ui/StatusPill";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { TooltipTrigger } from "../../shared/ui/TooltipTrigger";
import { Icon } from "../../shared/ui/Icon";
import { copyIcon, keyIcon, shieldIcon, terminalIcon } from "../../shared/ui/icons";
import { useMessage } from "../../shared/ui/message";
import { appStore, useAppStore } from "../../shared/store/appStore";

/**
 * Cursor's own surface: taking over the local proxy and trusting the CA that makes
 * HTTPS interception possible.
 *
 * Model configuration is not here. Models are shared with Devin, so they live in
 * the model library; Cursor and Devin stay parallel modules that meet only in that
 * library and in the call statistics.
 *
 * The page is built like Devin's: one verdict, the request path, then what is still
 * missing. The two modules answer the same question about two clients, so they had
 * to stop looking like two different products.
 */
export function CursorSettingsPage() {
  const { cursorHarness, cursorBusy, models } = useAppStore();
  const navigate = useNavigate();
  const message = useMessage();
  const [caCommand, setCaCommand] = useState<string | null>(null);
  const [waitingForCaRefresh, setWaitingForCaRefresh] = useState(false);
  const [confirmEnableTakeover, setConfirmEnableTakeover] = useState(false);
  const [confirmDisableTakeover, setConfirmDisableTakeover] = useState(false);

  const caReady = cursorHarness?.ca === "ready";
  const cursorTakenOver = cursorHarness?.settings_applied ?? false;
  /** Cursor 的代理配置由另一个同类软件写入：我们没去覆盖它，必须把这件事说出来。 */
  const foreignConfiguration = cursorHarness?.foreign_configuration ?? false;
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
    if (caCommand) void api.openCursorCaInstallTerminal(caCommand).catch((cause) => message.error(cause));
    setCaCommand(null);
    setWaitingForCaRefresh(true);
  };

  const caLabel = !cursorHarness
    ? t("读取中…")
    : cursorHarness.ca === "ready"
      ? t("已就绪")
      : cursorHarness.ca === "untrusted"
        ? t("待系统信任")
        : cursorHarness.ca === "invalid"
          ? t("已损坏")
          : t("未初始化");
  const caTone = cursorHarness?.ca === "ready" ? "ok" as const : cursorHarness?.ca === "missing" ? "idle" as const : "warn" as const;
  const stages: PathStage[] = [
    {
      key: "cursor",
      label: "Cursor",
      detail: cursorTakenOver ? t("已指向本机代理") : t("仍连官方服务"),
      state: cursorTakenOver ? "up" : caReady ? "down" : "unknown",
    },
    {
      key: "gateway",
      label: t("本机代理"),
      detail: cursorHarness?.proxy_url ?? caLabel,
      state: caReady ? "up" : caTone === "warn" ? "down" : "unknown",
    },
    {
      key: "models",
      label: t("模型库"),
      detail: models.length > 0 ? t("{count} 个模型", { count: models.length }) : t("还没有模型"),
      state: models.length > 0 ? "up" : "unknown",
    },
  ];
  const connected = cursorTakenOver && caReady && models.length > 0;
  const outstanding = [
    !models.length && { key: "models", label: t("往模型库里加一个模型"), hint: t("Cursor 的请求需要一个本地模型来回答。") },
    !caReady && { key: "ca", label: t("初始化并信任本地 CA"), hint: t("没有证书就无法解析 Cursor 的 HTTPS 请求。") },
    caReady && !cursorTakenOver && {
      key: "takeover",
      label: t("打开接管开关"),
      hint: foreignConfiguration
        ? t("Cursor 的代理配置现在由另一个同类软件管着；打开开关会强制结束 Cursor 并覆盖对方那份配置。")
        : t("写入本地代理配置后，需要手动重启 Cursor。"),
    },
  ].filter(Boolean) as Array<{ key: string; label: string; hint: string }>;

  const content = <div className={styles.page}>
    <TitledCard
      title={t("接入状态")}
      action={<Button size="small" onClick={() => void navigate("/calls")}>{t("查看调用记录")}</Button>}
    >
      <StatusHero
        connected={connected}
        title={connected ? t("已接管") : t("还没有接管")}
        description={connected
          ? t("代理配置已写入；重启 Cursor 之后，请求会由模型库里的 {count} 个模型回答。", { count: models.length })
          : foreignConfiguration
            ? t("Cursor 的代理配置由另一个同类软件写入，本应用没有覆盖它。要继续用本应用接管，请先关闭对方的接管，或打开右侧开关。")
            : outstanding[0]?.label ?? t("读取中…")}
        stages={stages}
        steps={outstanding}
        footnotes={connected
          ? t("Cursor 只在启动时读取这份配置：如果它是在开启接管之前打开的，需要重启一次。")
          : undefined}
        aside={<>
          {foreignConfiguration && <StatusPill tone="warn">{t("配置冲突")}</StatusPill>}
          <StatusPill tone={caTone}>{t("本地 CA：{state}", { state: caLabel })}</StatusPill>
          <TooltipTrigger label={takeoverLabel}>
            <Switch
              checked={cursorTakenOver}
              disabled={cursorBusy || (!cursorTakenOver && !caReady)}
              label={takeoverLabel}
              onChange={(enabled) => {
                if (enabled) setConfirmEnableTakeover(true);
                else setConfirmDisableTakeover(true);
              }}
            />
          </TooltipTrigger>
        </>}
      />
    </TitledCard>

    <TitledCard
      title={t("本地证书")}
      description={t("CA 只保存在本机，用于安全解析 Cursor 的 HTTPS 请求。")}
      icon={shieldIcon}
      badge={<StatusPill tone={caTone}>{caLabel}</StatusPill>}
    >
      <CursorCaGate busy={cursorBusy} waitingForRefresh={waitingForCaRefresh} onInitialize={() => void initializeCa()} onRefresh={() => void refreshCa}>
        <div className={styles.caBody}>
          <div className={styles.caFact}>
            <Icon icon={keyIcon} size="1.1em" />
            <div>
              <strong>{t("证书已就绪")}</strong>
              <small>{t("解析 Cursor 的 HTTPS 请求靠的就是它；更换证书需要重新启动 Cursor。")}</small>
            </div>
          </div>
          {cursorHarness?.ca_install_command && <div className={styles.caCommand}>
            <code>{cursorHarness.ca_install_command}</code>
            <TooltipTrigger label={t("复制安装命令")}>
              <button
                type="button"
                aria-label={t("复制安装命令")}
                onClick={() => void api.copyCursorText(cursorHarness.ca_install_command ?? "")
                  .then(() => message(t("安装命令已复制")))
                  .catch((cause) => message.error(cause))}
              ><Icon icon={copyIcon} size="1.05em" /></button>
            </TooltipTrigger>
            <TooltipTrigger label={t("在终端中安装")}>
              <button
                type="button"
                aria-label={t("在终端中安装")}
                onClick={() => void api.openCursorCaInstallTerminal(cursorHarness.ca_install_command ?? "")
                  .catch((cause) => message.error(cause))}
              ><Icon icon={terminalIcon} size="1.05em" /></button>
            </TooltipTrigger>
          </div>}
        </div>
      </CursorCaGate>
    </TitledCard>

    <TitledCard
      title={t("模型库")}
      description={t("Cursor 用这里的模型回答请求；它和 Devin 共用同一份配置。")}
      action={<Button size="small" variant="primary" onClick={() => void navigate("/models")}>{t("管理模型")}</Button>}
    >
      <div className={styles.modelSummary}>
        <span className={styles.modelCount}>{t("{count} 个模型", { count: models.length })}</span>
        {models.length > 0
          ? <ul className={styles.modelNames}>
            {models.slice(0, 4).map((model) => <li key={model.model_hash}>{model.display_name}</li>)}
            {models.length > 4 && <li>{t("还有 {count} 个", { count: models.length - 4 })}</li>}
          </ul>
          : <small>{t("模型库还是空的。Cursor 需要一个模型才能回答请求。")}</small>}
      </div>
    </TitledCard>
  </div>;

  return <>
    <PageContent
      title={<PageTitle
        title="Cursor"
        status={<StatusPill tone={connected ? "ok" : cursorTakenOver ? "warn" : "idle"}>{cursorTakenOver ? t("已接管") : t("未接管")}</StatusPill>}
        meta={t("接管本机代理，让 Cursor 用你自己的模型")}
      />}
      sections={[{ key: "cursor-settings", estimatedHeight: 720, content }]}
    />
    <ConfirmDialog
      open={confirmEnableTakeover}
      title={t("开启接管Cursor？")}
      cancelLabel={t("取消")}
      confirmLabel={t("开启接管")}
      onCancel={() => setConfirmEnableTakeover(false)}
      onConfirm={() => {
        setConfirmEnableTakeover(false);
        void appStore.setCursorEnabled(true);
      }}
    >
      <p>{t("这一步会强制结束所有正在运行的 Cursor 进程，未保存的编辑内容会丢失。请先保存工作；配置写入后需要你手动重新打开 Cursor。是否继续？")}</p>
    </ConfirmDialog>
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
