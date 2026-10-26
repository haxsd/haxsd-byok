import { useEffect, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { api } from "../../shared/api";
import type { CursorMode } from "../../shared/api";
import { CursorCaGate } from "./CursorGates";
import { startCursorCaWaitPoll } from "./caWaitPoll";
import { CursorToolsSettings } from "../cursor/CursorToolsSettings";
import { cursorSettingsSection, TOOLS_SECTION, type CursorSettingsSection } from "../cursor/toolsSection";
import styles from "./CursorSettings.module.scss";
import { PageActions } from "../../shell/PageActions";
import { PageContent } from "../../shell/layout/PageContent";
import { Button } from "../../shared/ui/Button";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import type { PathStage } from "../../shared/ui/ConnectionPath";
import { PageTitle } from "../../shared/ui/PageTitle";
import { Segmented } from "../../shared/ui/Segmented";
import { StatusHero } from "../../shared/ui/StatusHero";
import { StatusPill } from "../../shared/ui/StatusPill";
import { TitledCard } from "../../shared/ui/TitledCard";
import { TooltipTrigger } from "../../shared/ui/TooltipTrigger";
import { Icon } from "../../shared/ui/Icon";
import { copyIcon, keyIcon, shieldIcon, terminalIcon } from "../../shared/ui/icons";
import { useMessage } from "../../shared/ui/message";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { HarnessCapabilities } from "../harness/HarnessCapabilities";

/** 触发终端后的等待节奏：最长 30 秒，够一次 UAC 授权或一次密码输入。 */
const CA_READY_POLL_INTERVAL_MS = 2_000;
const CA_READY_POLL_ATTEMPTS = 15;

/**
 * Cursor's own surface, split in two sections on one page.
 *
 * 「接入」is the model source mode, the local proxy and the CA that makes HTTPS
 * interception possible. 「功能设置」is what Cursor's own features need from us:
 * the TAB completion endpoint and the commit-message model. The section lives in
 * the URL, so the settings page can link straight to the tools.
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
  const [pendingMode, setPendingMode] = useState<CursorMode | null>(null);

  const mode: CursorMode = cursorHarness?.mode ?? "official";
  const account = cursorHarness?.account;
  const catalog = cursorHarness?.catalog;
  const caReady = cursorHarness?.ca === "ready";
  const takesOver = mode !== "official";
  const cursorTakenOver = cursorHarness?.settings_applied ?? false;
  const foreignConfiguration = cursorHarness?.foreign_configuration ?? false;
  // 官方模型只有"混合 + 客户端真的登录了自己的账号"时才拿得到。
  const officialModelsUsable = mode === "mixed" && account?.state === "official";
  // 官方目录是不是正处于失败之后：界面要把静默降级说出来。
  const catalogDegraded = catalog != null
    && catalog.last_failure_ms != null
    && (catalog.last_success_ms == null || catalog.last_failure_ms > catalog.last_success_ms);
  // 运行中的 Cursor 内存里还指着本机代理：跨官方边界必须结束它才能重新读取。
  const pendingEndsCursor = pendingMode != null && (mode === "official" || pendingMode === "official");
  const windows = cursorHarness?.platform === "windows";
  // 分区写在 URL 上：设置页的「Tab 补全」「提交信息模型」直达 ?section=tools，刷新也留在原分区。
  const [params, setParams] = useSearchParams();
  const section = cursorSettingsSection(params);
  const selectSection = (next: CursorSettingsSection) => setParams(
    next === TOOLS_SECTION ? { section: TOOLS_SECTION } : {},
    { replace: true },
  );

  useEffect(() => {
    if (caCommand) void api.copyCursorText(caCommand);
  }, [caCommand]);

  /**
   * 打开终端之后自动等结果：每 2 秒问一次状态，装好即停，最多等 30 秒。
   * 超时不是失败——授权可能还在等待，所以保留手动刷新。
   *
   * 循环本体在 `caWaitPoll.ts`：递归 setTimeout（上一拍 settle 之后才排下一拍），
   * 而且等待期间只读 Cursor 接管状态这一个接口，不再整批刷新十二个接口。
   */
  useEffect(() => {
    if (!waitingForCaRefresh) return;
    return startCursorCaWaitPoll(() => setWaitingForCaRefresh(false), {
      intervalMs: CA_READY_POLL_INTERVAL_MS,
      maxAttempts: CA_READY_POLL_ATTEMPTS,
    });
  }, [waitingForCaRefresh]);

  const initializeCa = async () => {
    const status = await appStore.initializeCursorCa();
    if (status?.ca === "untrusted" && status.ca_install_command) setCaCommand(status.ca_install_command);
  };
  const refreshCa = async () => {
    await appStore.refresh();
    if (appStore.getSnapshot().cursorHarness?.ca !== "ready") setWaitingForCaRefresh(false);
  };
  const openCaTerminal = () => {
    if (caCommand) void api.openCursorCaInstallTerminal().catch((cause) => message.error(cause));
    setCaCommand(null);
    setWaitingForCaRefresh(true);
  };

  /**
   * 切换模式。跨官方边界要结束 Cursor（它只在启动时读代理配置），本地切到混合还要
   * 清掉我们注入的本地账号，这两件事都先说清楚再动手；本地↔混合的其余情况直接切。
   */
  const requestMode = (next: CursorMode) => {
    if (next === mode) return;
    if (next !== "official" && !caReady) {
      message(t("先初始化并信任本地 CA，再切换到本地或混合模式。"));
      return;
    }
    const clearsLocalAccount = next === "mixed" && account?.state === "local";
    if (next === "official" || mode === "official" || clearsLocalAccount) {
      setPendingMode(next);
      return;
    }
    void appStore.setCursorMode(next);
  };
  const switchWarning = pendingMode === "official"
    ? t("切换到官方模式会结束所有正在运行的 Cursor 进程（未保存的编辑会丢失），并清除本应用注入的本地账号，之后 Cursor 直连官方。请先保存工作。")
    : pendingMode == null
      ? ""
      : mode === "official"
        ? t("开启本地或混合模式会强制结束所有正在运行的 Cursor 进程，未保存的编辑内容会丢失。配置写入后需要你手动重新打开 Cursor。是否继续？")
        : t("混合模式要求 Cursor 登录你自己的账号，本应用会先清除自己注入的本地账号。切换后请在 Cursor 里登录。是否继续？");

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
  const modeLabel: Record<CursorMode, string> = {
    official: t("官方"),
    local: t("本地"),
    mixed: t("混合"),
  };
  const modeDetail: Record<CursorMode, string> = {
    official: t("完全交给 Cursor：不写代理配置、不注入账号，本应用看不到它的请求，也没有调用记录，不会压缩上下文。"),
    local: t("只用模型库里的模型：官方模型不进列表，不在库里的模型会被明确拒绝；没有账号时本应用补一个本地账号。"),
    mixed: t("官方模型和本机模型同时可用，按模型分流；上下文压缩只作用于由本机模型回答的请求。"),
  };
  const accountLabel = !account
    ? t("读取中…")
    : account.state === "missing"
      ? t("未登录")
      : account.state === "local"
        ? t("本地账号（本应用注入）")
        : account.email
          ? t("你的账号（{email}）", { email: account.email })
          : t("你的账号");
  const catalogLabel = mode === "local"
    ? t("本地模式不使用官方目录")
    : catalogDegraded
      ? t("失败（{reason}）", { reason: catalog?.last_failure ?? t("未知原因") })
      : catalog?.last_success_ms != null
        ? t("正常")
        : t("尚未获取");
  const catalogTime = catalog?.last_success_ms != null
    ? new Date(catalog.last_success_ms).toLocaleTimeString()
    : null;
  const stages: PathStage[] = [
    {
      key: "cursor",
      label: "Cursor",
      detail: !takesOver
        ? t("官方模式：直连")
        : cursorTakenOver
          ? t("已指向本机代理")
          : t("仍连官方服务"),
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
  const connected = takesOver && cursorTakenOver && caReady && models.length > 0;
  const outstanding = (!takesOver
    ? [{ key: "mode", label: t("选择本地或混合模式"), hint: t("官方模式下本应用不介入；要用自己的模型就选这两个模式之一。") }]
    : [
      !models.length && { key: "models", label: t("往模型库里加一个模型"), hint: t("Cursor 的请求需要一个本地模型来回答。") },
      !caReady && { key: "ca", label: t("初始化并信任本地 CA"), hint: t("没有证书就无法解析 Cursor 的 HTTPS 请求。") },
      caReady && !cursorTakenOver && {
        key: "takeover",
        label: t("重新打开 Cursor"),
        hint: foreignConfiguration
          ? t("Cursor 的代理配置现在由另一个同类软件管着；切换模式会强制结束 Cursor 并覆盖对方那份配置。")
          : t("写入本地代理配置后，需要手动重启 Cursor。"),
      },
      mode === "mixed" && account?.state !== "official" && {
        key: "account",
        label: t("在 Cursor 里登录你自己的账号"),
        hint: t("混合模式的官方模型来自你的账号；未登录时列表里只有本机模型。"),
      },
    ]).filter(Boolean) as Array<{ key: string; label: string; hint: string }>;

  // 两个分区都保持挂载：切到「接入」再看功能设置，编辑中的草稿还在。用 hidden 隐藏
  // 而不是卸载——卸载会把没保存的编辑一起丢掉。
  const accessCards = <>
    <TitledCard
      title={t("接入状态")}
      action={<Button size="small" onClick={() => void navigate("/calls")}>{t("查看调用记录")}</Button>}
    >
      <StatusHero
        connected={connected}
        title={!takesOver ? t("官方模式") : connected ? t("已接管") : t("还没有接管")}
        description={!takesOver
          ? t("Cursor 直连官方：本应用不写代理配置、不注入账号，也不记录它的请求。")
          : connected
            ? t("代理配置已写入；重启 Cursor 之后，请求会由模型库里的 {count} 个模型回答。", { count: models.length })
            : foreignConfiguration
              ? t("Cursor 的代理配置由另一个同类软件写入，本应用没有覆盖它。要继续用本应用接管，请先关闭对方的接管，或在上方选择模式。")
              : outstanding[0]?.label ?? t("读取中…")}
        stages={stages}
        steps={outstanding}
        footnotes={connected
          ? t("Cursor 只在启动时读取这份配置：如果它是在切换模式之前打开的，需要重启一次。")
          : undefined}
        aside={<>
          {foreignConfiguration && <StatusPill tone="warn">{t("配置冲突")}</StatusPill>}
          <StatusPill tone={caTone}>{t("本地 CA：{state}", { state: caLabel })}</StatusPill>
        </>}
      />
    </TitledCard>

    <TitledCard
      title={t("模型来源")}
      description={t("决定 Cursor 的模型列表里有什么，以及请求由谁回答。")}
      badge={<StatusPill tone={mode === "official" ? "idle" : officialModelsUsable ? "ok" : "warn"}>{modeLabel[mode]}</StatusPill>}
    >
      <div className={styles.caBody}>
        <div className={styles.modeRow}>
          <Segmented<CursorMode>
            value={mode}
            ariaLabel={t("模型来源")}
            disabled={cursorBusy}
            options={[
              { value: "official", label: t("官方") },
              { value: "local", label: t("本地") },
              { value: "mixed", label: t("混合") },
            ]}
            onChange={(next) => requestMode(next)}
          />
          {!caReady && mode !== "official" && <StatusPill tone="warn">{t("需要先信任本地 CA")}</StatusPill>}
        </div>
        <small className={styles.modeNote}>{modeDetail[mode]}</small>
        <div className={styles.caFact}>
          <Icon icon={keyIcon} size="1.1em" />
          <div>
            <strong>{t("Cursor 账号：{account}", { account: accountLabel })}</strong>
            <small>{mode === "mixed"
              ? t("混合模式要求登录你自己的账号；本应用不会注入本地账号。")
              : mode === "local"
                ? t("本地模式不要求官方账号；库里没有的模型会被明确拒绝，不会静默走官方。")
                : t("官方模式下账号完全由 Cursor 自己管理。")}</small>
          </div>
        </div>
        <div className={styles.caFact}>
          <Icon icon={shieldIcon} size="1.1em" />
          <div>
            <strong>{t("官方模型目录：{catalog}", { catalog: catalogLabel })}</strong>
            <small>{mode === "local"
              ? t("本地模式不向上游取官方目录，模型列表里只有本机的模型。")
              : catalogTime
                ? t("最近一次成功：{time}；失败时官方模型会从列表里消失，这里会显示原因。", { time: catalogTime })
                : t("还没成功取到官方目录；官方模型暂时不会出现在列表里。")}</small>
          </div>
        </div>
        {mode === "mixed" && account?.state !== "official" && <ul className={styles.modeList}>
          <li>{account?.state === "local" ? t("已清除本应用注入的本地账号") : t("尚未登录官方账号")}</li>
          <li>{t("请在 Cursor 里登录你自己的账号，官方模型才会出现在模型列表里")}</li>
        </ul>}
        {catalogDegraded && mode !== "local" && <ul className={styles.modeList}>
          <li>{t("官方模型列表上一次抓取失败，模型列表可能只有本机的模型")}</li>
          <li>{t("如果失败原因是 401，通常是账号未登录或登录已过期")}</li>
        </ul>}
      </div>
    </TitledCard>

    <TitledCard
      title={t("本地证书")}
      description={t("CA 只保存在本机，用于安全解析 Cursor 的 HTTPS 请求。")}
      icon={shieldIcon}
      badge={<StatusPill tone={caTone}>{caLabel}</StatusPill>}
    >
      <CursorCaGate busy={cursorBusy} waitingForRefresh={waitingForCaRefresh} onInitialize={() => void initializeCa()} onRefresh={() => void refreshCa()}>
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
                onClick={() => void api.openCursorCaInstallTerminal()
                  .catch((cause) => message.error(cause))}
              ><Icon icon={terminalIcon} size="1.05em" /></button>
            </TooltipTrigger>
          </div>}
        </div>
      </CursorCaGate>
    </TitledCard>

    <TitledCard
      title={t("模型库")}
      description={t("Cursor 用这里的模型回答请求；它和其他接入模块共用同一份配置。")}
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

    <HarnessCapabilities client="cursor" />
  </>;

  const content = <div className={styles.page}>
    <div className={styles.section} hidden={section !== "access"}>{accessCards}</div>
    <div className={styles.section} hidden={section !== "tools"}><CursorToolsSettings /></div>
  </div>;

  return <>
    <PageActions>
      <Segmented<CursorSettingsSection>
        value={section}
        ariaLabel={t("Cursor 设置分区")}
        options={[{ value: "access", label: t("接入") }, { value: "tools", label: t("功能设置") }]}
        onChange={selectSection}
      />
    </PageActions>
    <PageContent
      title={<PageTitle
        title="Cursor"
        status={<StatusPill tone={!takesOver ? "idle" : connected ? "ok" : "warn"}>{modeLabel[mode]}</StatusPill>}
        meta={t("选择模型来源，让 Cursor 用你自己的模型")}
      />}
      sections={[{ key: "cursor-settings", estimatedHeight: section === "tools" ? 420 : 920, content }]}
    />
    <ConfirmDialog
      open={pendingMode !== null}
      title={t("切换模型来源？")}
      cancelLabel={t("取消")}
      confirmLabel={pendingEndsCursor ? t("结束并切换") : t("切换")}
      onCancel={() => setPendingMode(null)}
      onConfirm={() => {
        const next = pendingMode;
        setPendingMode(null);
        if (next) void appStore.setCursorMode(next);
      }}
    >
      <p>{switchWarning}</p>
    </ConfirmDialog>
    <ConfirmDialog open={caCommand !== null} title={t("安装本地 CA")} cancelLabel={t("关闭")} confirmLabel={t("打开终端")} onCancel={() => setCaCommand(null)} onConfirm={openCaTerminal}>
      <div className={styles.editor}><strong>{t("需要授权安装证书")}</strong><span>{windows
        ? t("安装命令已自动复制。点击「打开终端」，命令会自动执行并弹出 UAC 授权窗口，选「是」即可；装好后本页会自动刷新。")
        : t("安装命令已自动复制。点击「打开终端」，将命令粘贴到终端中执行，并按提示输入密码；装好后本页会自动刷新。")}</span><pre className={styles.command}>{caCommand}</pre></div>
    </ConfirmDialog>
  </>;
}
