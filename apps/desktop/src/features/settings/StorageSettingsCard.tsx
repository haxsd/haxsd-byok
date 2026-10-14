import { useEffect, useState } from "react";
import { api, type StatisticsStorage, type StatisticsStorageScope } from "../../shared/api";
import { appStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { Select } from "../../shared/ui/Select";
import { TitledCard } from "../../shared/ui/TitledCard";
import styles from "./SettingsPage.module.scss";

export function StorageSettingsCard() {
  const [storage, setStorage] = useState<StatisticsStorage | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [open, setOpen] = useState(false);
  const [scope, setScope] = useState<StatisticsStorageScope>("details");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let active = true;
    setError(null);
    void api.statisticsStorage().then(value => { if (active) setStorage(value); }).catch(cause => { if (active) setError(String(cause)); });
    return () => { active = false; };
  }, [attempt]);
  const clear = async () => {
    setBusy(true);
    setError(null);
    try {
      setStorage(await api.clearStatisticsStorage(scope));
      setOpen(false);
      await appStore.refresh();
    } catch (cause) { setError(String(cause)); }
    finally { setBusy(false); }
  };
  const bytes = (value: number) => {
    if (value < 1024) return `${value} B`;
    const index = Math.min(3, Math.floor(Math.log(value) / Math.log(1024)));
    return `${(value / 1024 ** index).toFixed(1)} ${["B", "KB", "MB", "GB"][index]}`;
  };
  return <>
    <TitledCard title={t("存储管理")}>
      <div className={styles.settingRow}>
        <div><strong>{storage ? bytes(storage.bytes) : error ? t("读取失败") : t("计算中…")}</strong>
          {storage && <small>{t("{calls} 次调用 · {traces} 条追踪", { calls: storage.call_count, traces: storage.trace_count })}</small>}
        </div>
        <Button size="small" disabled={!storage || busy} onClick={() => { setScope("details"); setError(null); setOpen(true); }}>{t("清理存储空间")}</Button>
      </div>
      {error && !open && <div className={styles.inlineError} role="alert"><span>{error}</span><Button size="small" onClick={() => setAttempt(value => value + 1)}>{t("重试")}</Button></div>}
    </TitledCard>
    <ConfirmDialog open={open} title={scope === "all" ? t("确定要清理全部统计数据吗？") : t("确定要清理详细记录吗？")} busy={busy} cancelLabel={t("取消")} confirmLabel={t("确认清理")} onCancel={() => setOpen(false)} onConfirm={() => void clear()}>
      <div className={styles.confirmContent}>
        <Select value={scope} ariaLabel={t("清理范围")} options={[{ value: "details", label: t("仅清理详细记录") }, { value: "all", label: t("清理全部统计数据") }]} onChange={value => setScope(value as StatisticsStorageScope)} />
        <small>{scope === "all" ? t("所有调用汇总、详细内容和追踪记录都会被删除。模型配置、CA 和应用设置不会受到影响，此操作无法撤销。") : t("仅删除请求、响应和追踪附件等详细内容，保留调用汇总、统计指标和配置。")}</small>
        {error && <small role="alert">{error}</small>}
      </div>
    </ConfirmDialog>
  </>;
}
