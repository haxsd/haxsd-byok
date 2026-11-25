import { useEffect, useRef, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { api } from "../../shared/api";
import type { CursorMode } from "../../shared/api";
import { CursorCaGate } from "./CursorGates";
import { startCursorCaWaitPoll } from "./caWaitPoll";
import { CursorToolsSettings } from "../cursor/CursorToolsSettings";
import { cursorSettingsSection, TOOLS_SECTION } from "../cursor/toolsSection";
import styles from "./CursorSettings.module.scss";
import { PageContent } from "../../shell/layout/PageContent";
import { Button } from "../../shared/ui/Button";
import { Card } from "../../shared/ui/Card";
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
import { cursorAvailableModels } from "./cursorAvailableModels";

/** 触发终端后的等待节奏：最长 30 秒，够一次 UAC 授权或一次密码输入。 */
const CA_READY_POLL_INTERVAL_MS = 2_000;
const CA_READY_POLL_ATTEMPTS = 15;

/**
 * Cursor's own surface on one page.
 *
 * The model source mode, the local proxy and the CA come first; the TAB
 * completion endpoint and the commit-message model follow as ordinary cards.
 * Settings search links to `?section=tools`, which scrolls to that block instead
 * of switching a tab that no longer exists.
 *
 * Model configuration is not here. Models are shared with Devin, so they live in
 * the model library; Cursor and Devin stay parallel modules that meet only in that
 * library and in the call statistics.
 */
export function CursorSettingsPage() {
  const { cursorHarness, cursorBusy, models, plugins } = useAppStore();
  const availableModels = cursorAvailableModels(models, plugins);
  const modelCount = availableModels.length;
  const navigate = useNavigate();
  const message = useMessage();
  const [caCommand, setCaCommand] = useState<string | null>(null);
  const [waitingForCaRefresh, setWaitingForCaRefresh] = useState(false);
  const [pendingMode, setPendingMode] = useState<CursorMode | null>(null);
  const [pendingForce, setPendingForce] = useState(false);
  /**
   * 模式选择先落在草稿里，不立刻写配置、不结束 Cursor。真正生效只由状态卡里的
   * 主动作触发；这样页面只有一条接管路径，切换模式不会在后台偷偷改系统。
   */
  const [draftMode, setDraftMode] = useState<CursorMode>(cursorHarness?.mode ?? "official");

  const mode: CursorMode = cursorHarness?.mode ?? "official";
  useEffect(() => {
    setDraftMode(mode);
  }, [mode]);
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
  const pendingEndsCursor = pendingMode != null && (pendingForce || mode === "official" || pendingMode === "official");
  const windows = cursorHarness?.platform === "windows";
  // 设置页的「Tab 补全」「提交信息模型」链接带 ?section=tools：滚动到功能设置块，
  // 不再用标签页切换。
  const [params] = useSearchParams();
  const [caCopyFailed, setCaCopyFailed] = useState(false);
  const section = cursorSettingsSection(params);
  const toolsRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (section !== TOOLS_SECTION) return;
    const node = toolsRef.current;
    if (!node) return;
    node.scrollIntoView({ block: "start" });
  }, [section]);

  useEffect(() => {
    if (!caCommand) {
      setCaCopyFailed(false);
      return;
    }
    void api.copyCursorText(caCommand)
      .then(() => setCaCopyFailed(false))
      .catch(() => setCaCopyFailed(true));
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
    if (appStore.getSnapshot().cursorHarness?.ca === "ready") setWaitingForCaRefresh(false);
  };
  const openCaTerminal = () => {
    if (caCommand) void api.openCursorCaInstallTerminal().catch((cause) => message.error(cause));
    setCaCommand(null);
    setWaitingForCaRefresh(true);
  };

  /**
   * 选择模式只改草稿。跨官方边界、清本地账号、外部配置这些副作用都留到用户点
   * 状态卡里的主按钮之后，由确认对话框一次说清。
   */
  const requestMode = (next: CursorMode) => {
    if (next === draftMode) return;
    setDraftMode(next);
  };
  /** 状态卡里的唯一下一步：按当前草稿决定是添加模型、装证书、应用接管还是刷新。 */
  const applyTakeover = () => {
    const selectedMode = draftMode === "official" && mode === "official" ? "local" : draftMode;
    if (selectedMode !== "official" && !caReady) {
      message(t("先初始化并信任本地 CA，再切换到本地或混合模式。"));
      return;
    }
    if (selectedMode === "official") {
      if (mode === "official") {
        message(t("当前就是官方模式。"));
        return;
      }
      setPendingForce(false);
      setPendingMode("official");
      return;
    }
    if (selectedMode === mode && cursorTakenOver) {
      void appStore.refresh();
      return;
    }
    const clearsLocalAccount = selectedMode === "mixed" && account?.state === "local";
    if (mode === "official" || clearsLocalAccount || foreignConfiguration) {
      setPendingForce(false);
      setPendingMode(selectedMode);
      return;
    }
    void appStore.setCursorMode(selectedMode);
  };
  /** 撤销接管：回到官方模式。 */
  const revokeTakeover = () => {
    if (mode === "official") return;
    setPendingForce(false);
    setPendingMode("official");
  };
  /** 外部配置挡路时，明确覆盖它；原配置由保管键保存，撤销时恢复。 */
  const forceTakeover = () => {
    if (!caReady) {
      message(t("先初始化并信任本地 CA，再切换到本地或混合模式。"));
      return;
    }
    const selectedMode = draftMode === "official" ? "local" : draftMode;
    setPendingForce(true);
    setPendingMode(selectedMode);
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
      detail: modelCount > 0 ? t("{count} 个模型", { count: modelCount }) : t("还没有模型"),
      state: modelCount > 0 ? "up" : "unknown",
    },
  ];
  const connected = takesOver && cursorTakenOver && caReady && modelCount > 0;
  const outstanding = (!takesOver
    ? [{ key: "mode", label: t("选择本地或混合模式"), hint: t("官方模式下本应用不介入；要用自己的模型就选这两个模式之一。") }]
    : [
      !modelCount && { key: "models", label: t("往模型库里加一个模型"), hint: t("Cursor 的请求需要一个本地模型来回答。") },
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

  /**
   * 状态卡只有一个主动作。草稿模式决定下一页是什么：先补模型、再补证书，
   * 然后应用接管；已经生效后只提供查看记录，避免再出现第二个“应用接管”。
   */
  const draftDiffers = draftMode !== mode;
  const primaryAction = (() => {
    if (modelCount === 0) {
      return { label: t("添加模型"), run: () => void navigate("/models") };
    }
    if (draftMode === "official" && mode !== "official") {
      return { label: t("切换到官方"), run: applyTakeover };
    }
    if (!caReady) {
      return { label: t("安装证书"), run: () => void initializeCa() };
    }
    if (draftMode !== "official" && draftDiffers) {
      return {
        label: mode === "official"
          ? draftMode === "mixed" ? t("开启混合接管") : t("开启本地接管")
          : draftMode === "mixed" ? t("切换到混合模式") : t("切换到本地模式"),
        run: applyTakeover,
      };
    }
    if (mode === "official") {
      return { label: t("开启本地接管"), run: applyTakeover };
    }
    if (!cursorTakenOver) {
      return { label: t("开启接管"), run: applyTakeover };
    }
    return { label: t("查看调用记录"), run: () => void navigate("/calls") };
  })();
  const statusDescription = modelCount === 0
    ? t("模型库为空，Cursor 还没有可回答请求的模型。先添加一个，再继续接管。")
    : !caReady && !takesOver
      ? t("模型已就绪。安装一次本地证书，Cursor 的请求才能安全经过本机代理。")
      : !caReady
        ? t("本地证书未就绪。重新安装后，接管才能继续生效。")
        : draftMode === "official" && mode !== "official"
          ? t("已经选择回到官方模式；点击右侧按钮会撤销接管并结束 Cursor。")
          : draftDiffers
            ? t("已经选择{mode}模式，点击右侧按钮应用；切换模式不会自动改系统。", { mode: modeLabel[draftMode] })
            : !takesOver
              ? t("当前直连官方。点击右侧按钮会开启本地接管，由模型库回答请求。")
              : connected
                ? t("代理配置已写入；重启 Cursor 之后，请求会由模型库里的 {count} 个模型回答。", { count: modelCount })
                : foreignConfiguration
                  ? t("Cursor 的代理配置由另一个同类软件写入，本应用没有覆盖它。要接管请先关闭对方，或用「强制接管」覆盖。")
                  : !cursorTakenOver
                    ? t("本地代理配置尚未写入；点击右侧按钮开启接管。")
                    : outstanding[0]?.label ?? t("读取中…");

  // 两个分区都保持挂载：切到「接入」再看功能设置，编辑中的草稿还在。用 hidden 隐藏
  // 而不是卸载——卸载会把没保存的编辑一起丢掉。
  const accessCards = <>
    <Card as="section" className={styles.statusCard}>
      <StatusHero
        connected={connected}
        title={!takesOver ? t("官方模式") : connected ? t("已接管") : t("还没有接管")}
        description={statusDescription}
        stages={stages}
        steps={[]}
        footnotes={connected
          ? t("Cursor 只在启动时读取这份配置：如果它是在切换模式之前打开的，请完全退出并重新打开。")
          : draftDiffers
            ? t("选择模式不会立即改系统；点上方主按钮才会写入配置。")
            : undefined}
        action={<>
          {foreignConfiguration && <Button size="small" disabled={cursorBusy} onClick={forceTakeover}>{t("强制接管")}</Button>}
          {takesOver && <Button size="small" disabled={cursorBusy} onClick={revokeTakeover}>{t("撤销接管")}</Button>}
          <Button size="small" variant="primary" disabled={cursorBusy} onClick={primaryAction.run}>{primaryAction.label}</Button>
        </>}
        aside={<>
          {foreignConfiguration && <StatusPill tone="warn">{t("配置冲突")}</StatusPill>}
          <StatusPill tone={caTone}>{t("本地 CA：{state}", { state: caLabel })}</StatusPill>
        </>}
      />
    </Card>

    <TitledCard
      title={t("接入配置")}
      description={t("模型来源、证书和模型库；其余说明放在高级区。")}
      badge={draftDiffers ? <StatusPill tone="warn">{t("未应用")}</StatusPill> : undefined}
    >
      <div className={styles.configBody}>
        <section className={styles.configBlock}>
          <div className={styles.configHead}>
            <strong>{t("模型来源")}</strong>
            <StatusPill tone={mode === "official" ? "idle" : mode === "local" || officialModelsUsable ? "ok" : "warn"}>{modeLabel[mode]}</StatusPill>
          </div>
          {cursorHarness?.runtime_mismatch && (
            <small className={styles.draftNote}>{t("检测到正在运行的 Cursor 使用的端口或模式与当前配置不一致，请完全退出并重新打开 Cursor。")}</small>
          )}
          <div className={styles.modeRow}>
            <Segmented<CursorMode>
              value={draftMode}
              ariaLabel={t("模型来源")}
              disabled={cursorBusy}
              options={[
                { value: "official", label: t("官方") },
                { value: "local", label: t("本地") },
                { value: "mixed", label: t("混合") },
              ]}
              onChange={(next) => requestMode(next)}
            />
            {!caReady && draftMode !== "official" && <StatusPill tone="warn">{t("需要先信任本地 CA")}</StatusPill>}
          </div>
          <small className={styles.modeNote}>{modeDetail[draftMode]}</small>
          {draftDiffers && <small className={styles.draftNote}>{t("这是未应用的选择；点上方主按钮才会写入配置。")}</small>}
          <details className={styles.configDetails}>
            <summary>{t("账号与官方模型目录")}</summary>
            <div className={styles.configFacts}>
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
            </div>
            {mode === "mixed" && account?.state !== "official" && <ul className={styles.modeList}>
              <li>{account?.state === "local" ? t("已清除本应用注入的本地账号") : t("尚未登录官方账号")}</li>
              <li>{t("请在 Cursor 里登录你自己的账号，官方模型才会出现在模型列表里")}</li>
            </ul>}
            {catalogDegraded && mode !== "local" && <ul className={styles.modeList}>
              <li>{t("官方模型列表上一次抓取失败，模型列表可能只有本机的模型")}</li>
              <li>{t("如果失败原因是 401，通常是账号未登录或登录已过期")}</li>
            </ul>}
          </details>
        </section>

        <section className={styles.configBlock}>
          <div className={styles.configHead}>
            <strong>{t("本地证书")}</strong>
            <StatusPill tone={caTone}>{caLabel}</StatusPill>
          </div>
          <small className={styles.modeNote}>{caReady
            ? t("证书已就绪，用于安全解析 Cursor 的 HTTPS 请求。")
            : t("安装一次证书，Cursor 的请求才能安全经过本机代理。")}</small>
        </section>

        <section className={styles.configBlock}>
          <div className={styles.configHead}>
            <strong>{t("模型库")}</strong>
            <span className={styles.configCount}>{t("{count} 个模型", { count: modelCount })}</span>
          </div>
          <div className={styles.configLine}>
            <span>{modelCount > 0
              ? t("Cursor 会从这里选择模型回答请求。")
              : t("模型库还是空的。Cursor 需要一个模型才能回答请求。")}</span>
            <Button size="small" onClick={() => void navigate("/models")}>{t("管理模型")}</Button>
          </div>
        </section>
      </div>
    </TitledCard>

    <TitledCard
      title={t("本地证书详情")}
      description={t("安装命令、平台授权方式和证书状态。")}
      icon={shieldIcon}
      badge={<StatusPill tone={caTone}>{caLabel}</StatusPill>}
      collapsible
      storageKey="cursor-ca-details"
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

    <HarnessCapabilities client="cursor" collapsible />
  </>;
  const content = <div className={styles.page}>
    {accessCards}
    <div ref={toolsRef} className={styles.section}><CursorToolsSettings /></div>
  </div>;

  return <>    <PageContent
      title={<PageTitle
        title="Cursor"
        status={<StatusPill tone={!takesOver ? "idle" : connected ? "ok" : "warn"}>{modeLabel[mode]}</StatusPill>}
        meta={t("选择模型来源，让 Cursor 用你自己的模型")}
      />}
      sections={[{ key: "cursor-settings", estimatedHeight: 1340, content }]}
    />
    <ConfirmDialog
      open={pendingMode !== null}
      destructive={pendingEndsCursor}
      title={pendingForce
        ? t("强制接管？")
        : pendingMode === "official" ? t("撤销接管？") : t("应用接管？")}
      cancelLabel={t("取消")}
      confirmLabel={pendingForce
        ? t("结束并强制接管")
        : pendingMode === "official"
          ? t("结束并撤销")
          : pendingEndsCursor ? t("结束并应用") : t("应用")}
      onCancel={() => {
        setPendingMode(null);
        setPendingForce(false);
      }}
      onConfirm={() => {
        const next = pendingMode;
        const force = pendingForce;
        setPendingMode(null);
        setPendingForce(false);
        if (next) void appStore.setCursorMode(next, force);
      }}
    >
      <p>{pendingForce
        ? t("检测到其它工具留下的代理配置。强制接管会覆盖它，并在撤销时恢复原配置；同时会结束正在运行的 Cursor，未保存的编辑会丢失。")
        : switchWarning}</p>
    </ConfirmDialog>
    <ConfirmDialog open={caCommand !== null} title={t("安装本地 CA")} cancelLabel={t("关闭")} confirmLabel={t("打开终端")} onCancel={() => setCaCommand(null)} onConfirm={openCaTerminal}>
      <div className={styles.editor}><strong>{t("需要授权安装证书")}</strong><span>{windows
        ? caCopyFailed
          ? t("请先手动复制下方命令，或点击「打开终端」自动执行并弹出 UAC 授权窗口，选「是」即可；装好后本页会自动刷新。")
          : t("安装命令已自动复制。点击「打开终端」，命令会自动执行并弹出 UAC 授权窗口，选「是」即可；装好后本页会自动刷新。")
        : caCopyFailed
          ? t("未能自动复制命令，请手动复制下方命令；点击「打开终端」，将命令粘贴到终端中执行，并按提示输入密码；装好后本页会自动刷新。")
          : t("安装命令已自动复制。点击「打开终端」，将命令粘贴到终端中执行，并按提示输入密码；装好后本页会自动刷新。")}</span><pre className={styles.command}>{caCommand}</pre></div>
    </ConfirmDialog>
  </>;
}
