import { useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { TitledCard } from "../../shared/ui/TitledCard";
import { Button } from "../../shared/ui/Button";
import styles from "./SettingsPage.module.scss";

/**
 * In-app updates. The signed release manifest lives next to the installer in the
 * product's own repository, so the app can update itself without sending the user
 * back to a download page.
 */
export function UpdateCard() {
  const [version, setVersion] = useState<string | null>(null);
  const [versionError, setVersionError] = useState<string | null>(null);
  const [update, setUpdate] = useState<Update | null>(null);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<number | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  // The version is the one thing this card can always show, so it is read on
  // mount rather than on the first click.
  useEffect(() => {
    let cancelled = false;
    void getVersion()
      .then((value) => {
        if (!cancelled) setVersion(value);
      })
      .catch((cause) => {
        if (!cancelled) setVersionError(cause instanceof Error ? cause.message : String(cause));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const checkForUpdate = async () => {
    setBusy(true);
    setMessage(null);
    setUpdate(null);
    try {
      const current = version ?? await getVersion();
      const found = await check();
      if (!found) {
        setMessage(t("已是最新版本（{version}）", { version: current }));
        return;
      }
      setUpdate(found);
    } catch (cause) {
      // 开发模式与非 Tauri 环境下没有更新通道，据实说明而不是装作已是最新。
      setMessage(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  const install = async () => {
    if (!update) return;
    setBusy(true);
    setMessage(null);
    let total = 0;
    let received = 0;
    try {
      await update.downloadAndInstall((event) => {
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
      setMessage(t("更新已安装，正在重启…"));
      await relaunch();
    } catch (cause) {
      setMessage(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  return <TitledCard
    title={t("软件更新")}
    action={<Button size="small" variant="primary" disabled={busy} onClick={() => void checkForUpdate()}>{busy ? t("检查中…") : t("检查更新")}</Button>}
  >
    <div className={styles.settingRow}>
      <div>
        <strong>{t("当前版本")}</strong>
        <small>{version ?? versionError ?? t("读取中…")}</small>
      </div>
      {update && <div>
        <strong>{t("可更新到 {version}", { version: update.version })}</strong>
        <small>{progress === null ? t("下载并安装后会自动重启") : t("下载中 {percent}%", { percent: progress })}</small>
      </div>}
      {update && <Button size="small" disabled={busy} onClick={() => void install()}>{progress === null ? t("下载并安装") : t("安装中…")}</Button>}
    </div>
    {message && <div className={styles.settingRow}><small>{message}</small></div>}
  </TitledCard>;
}
