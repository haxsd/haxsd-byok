import { useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { api, type AppInfo } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { TitledCard } from "../../shared/ui/TitledCard";
import { forgetPendingUpdate, rememberPendingUpdate, usePendingUpdateOutcome } from "./pendingUpdate";
import { NOT_APPLIED, classifyUpdateFailure, describeUpdateFailure, type UpdateFailure } from "./updateFailure";
import styles from "./SettingsPage.module.scss";

/** 卡片底部的状态行：一句已翻译好的说明，或一次带分类的失败。 */
type UpdateStatus =
  | { kind: "none" }
  | { kind: "notice"; text: string }
  | { kind: "failure"; phase: "download" | "install"; failure: UpdateFailure };

/**
 * In-app updates. The signed release manifest lives next to the installer in the
 * product's own repository, so the app can update itself without sending the user
 * back to a download page.
 *
 * 下载与安装分成两步（`download()` + `install()`），因为两者失败的原因完全不同：
 * 下载失败是到 GitHub 的连接问题，安装失败常常是 Windows 没放行未签名的安装包
 * （Smart App Control / 代码完整性，实测见 HANDOFF 第六节）。合成一步时这两种原因
 * 都只剩一句英文系统错误，用户既看不出原因，也看不出下一步该换代理还是改系统策略。
 */
export function UpdateCard() {
  const [version, setVersion] = useState<string | null>(null);
  const [versionError, setVersionError] = useState<string | null>(null);
  const [appInfo, setAppInfo] = useState<AppInfo | null>(null);
  const [appInfoError, setAppInfoError] = useState<string | null>(null);
  const [update, setUpdate] = useState<Update | null>(null);
  const [downloaded, setDownloaded] = useState(false);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<number | null>(null);
  const [status, setStatus] = useState<UpdateStatus>({ kind: "none" });
  const lastApply = usePendingUpdateOutcome();

  // 版本与"跑的哪一份"都在挂载时读：这是卡片唯一总能回答的问题。
  useEffect(() => {
    let cancelled = false;
    void getVersion()
      .then((value) => {
        if (!cancelled) setVersion(value);
      })
      .catch((cause) => {
        if (!cancelled) setVersionError(cause instanceof Error ? cause.message : String(cause));
      });
    // 本机可以同时存在两份安装（`D:\...` 与 `%LOCALAPPDATA%\...`），
    // 只看版本号分不清更新装到了哪一份、跑起来的又是哪一份。
    void api.appInfo()
      .then((value) => {
        if (!cancelled) setAppInfo(value);
      })
      .catch((cause) => {
        if (!cancelled) setAppInfoError(cause instanceof Error ? cause.message : String(cause));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const checkForUpdate = async () => {
    setBusy(true);
    setStatus({ kind: "none" });
    setUpdate(null);
    setDownloaded(false);
    setProgress(null);
    try {
      const current = version ?? await getVersion();
      const found = await check();
      if (!found) {
        setStatus({ kind: "notice", text: t("已是最新版本（{version}）", { version: current }) });
        return;
      }
      setUpdate(found);
    } catch (cause) {
      // 开发模式与非 Tauri 环境下没有更新通道，据实说明而不是装作已是最新。
      setStatus({ kind: "notice", text: cause instanceof Error ? cause.message : String(cause) });
    } finally {
      setBusy(false);
    }
  };

  /** 下载：失败只可能是连接或配额问题，说清"下载失败"就够。 */
  const downloadStep = async (target: Update) => {
    let total = 0;
    let received = 0;
    try {
      await target.download((event) => {
        if (event.event === "Started") {
          total = event.data.contentLength ?? 0;
          received = 0;
          setProgress(0);
        } else if (event.event === "Progress") {
          received += event.data.chunkLength;
          setProgress(total > 0 ? Math.min(100, Math.round((received / total) * 100)) : null);
        } else if (event.event === "Finished") {
          setProgress(100);
        }
      });
      setDownloaded(true);
      return true;
    } catch (cause) {
      setProgress(null);
      setStatus({ kind: "failure", phase: "download", failure: classifyUpdateFailure(cause) });
      return false;
    }
  };

  /** 安装：Windows 上这一句之后本进程就结束了，所以期望值与提示都要先落下。 */
  const installStep = async (target: Update) => {
    rememberPendingUpdate(target.currentVersion, target.version);
    setStatus({ kind: "notice", text: t("安装程序已启动，应用将退出；重启后会自动核对是否装上了") });
    try {
      await target.install();
    } catch (cause) {
      forgetPendingUpdate();
      setStatus({ kind: "failure", phase: "install", failure: classifyUpdateFailure(cause) });
      return;
    }
    await relaunch();
  };

  const applyUpdate = async () => {
    if (!update) return;
    setBusy(true);
    setStatus({ kind: "none" });
    try {
      // 已经下载过的包不重下：安装失败后可以直接重试安装。
      if (!downloaded && !(await downloadStep(update))) return;
      await installStep(update);
    } finally {
      setBusy(false);
    }
  };

  const shownVersion = version ?? appInfo?.version ?? null;
  /** 更新行的说明：下载中显示进度，下载完成后说明可以直接重试安装。 */
  const updateHint = () => {
    if (downloaded) return t("安装包已下载，可以直接重试安装");
    if (!busy) return t("安装程序会退出本应用，装好后重新启动它");
    return progress === null ? t("下载中…") : t("下载中 {percent}%", { percent: progress });
  };

  return <TitledCard
    title={t("软件更新")}
    action={<Button size="small" variant="primary" disabled={busy} onClick={() => void checkForUpdate()}>{busy ? t("检查中…") : t("检查更新")}</Button>}
  >
    {/* 运行的是哪一份：本机有两份安装时，"更新完还是旧版本"和"更新装到别处去了"
        长得一模一样，所以版本、可执行文件和数据目录都要能看到。 */}
    <div className={styles.runtimeRow}>
      <strong>{t("当前版本")}</strong>
      <small title={versionError ?? undefined}>{shownVersion ?? (versionError ? t("读取失败") : t("读取中…"))}</small>
    </div>
    <div className={styles.runtimeRow}>
      <strong>{t("可执行文件")}</strong>
      <small title={appInfoError ?? undefined}>{appInfo?.executable_path ?? (appInfoError ? t("读取失败") : t("读取中…"))}</small>
    </div>
    <div className={styles.runtimeRow}>
      <strong>{t("数据目录")}</strong>
      <small title={appInfoError ?? undefined}>{appInfo?.data_dir ?? (appInfoError ? t("读取失败") : t("读取中…"))}</small>
    </div>
    {update && <div className={styles.settingRow}>
      <div>
        <strong>{t("可更新到 {version}", { version: update.version })}</strong>
        <small>{updateHint()}</small>
      </div>
      <Button size="small" disabled={busy} onClick={() => void applyUpdate()}>
        {busy ? (downloaded ? t("安装中…") : t("下载中…")) : downloaded ? t("重试安装") : t("下载并安装")}
      </Button>
    </div>}
    {status.kind === "failure" && <div className={styles.statusRow} data-tone="bad">
      <strong>{status.phase === "download" ? t("下载失败") : t("安装失败")}</strong>
      <small>{describeUpdateFailure(status.failure)}</small>
      {/* 分类给出中文解释，原文仍然照原样留着：转发给开发者时要的是这一行。 */}
      {status.failure.kind !== "unknown" && status.failure.raw && <small>{status.failure.raw}</small>}
    </div>}
    {status.kind === "notice" && <div className={styles.statusRow}>
      <small>{status.text}</small>
    </div>}
    {lastApply && <div className={styles.statusRow} data-tone={lastApply.applied ? undefined : "bad"}>
      <small>{lastApply.applied
        ? t("上次更新已生效：{from} → {to}", { from: lastApply.from, to: lastApply.to })
        : t("上次更新未生效：重启后仍是 {version}。{reason}", { version: lastApply.from, reason: describeUpdateFailure(NOT_APPLIED) })}</small>
    </div>}
  </TitledCard>;
}
