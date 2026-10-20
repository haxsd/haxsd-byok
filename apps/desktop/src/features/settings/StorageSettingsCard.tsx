import { useEffect, useState } from "react";
import { api, type StatisticsStorage, type StatisticsStorageScope } from "../../shared/api";
import { appStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { Select } from "../../shared/ui/Select";
import { TitledCard } from "../../shared/ui/TitledCard";
import { browserTimers, type PollTimers } from "../../shared/utils/pollTimers";
import { loadWhenVisible } from "../../shared/utils/visibleLoad";
import { loadStatisticsStorage } from "./settingsLoaders";
import { clearingCounter, formatBytes, hasWebCache, LOG_RETENTION_DAYS, retentionDays, storageHint, storageHintThreshold } from "./storageHint";
import styles from "./SettingsPage.module.scss";

/** 清理在服务端后台分批跑：清完之前每隔这么久问一次进度。 */
const CLEAR_POLL_MS = 1_500;

/** `visible` 来自设置页：没显示这张卡时不取数（卡片仍挂着，草稿与选择不丢）。 */
export function StorageSettingsCard({ visible, timers = browserTimers }: { visible: boolean; timers?: PollTimers }) {
  const [storage, setStorage] = useState<StatisticsStorage | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [open, setOpen] = useState(false);
  const [scope, setScope] = useState<StatisticsStorageScope>("details");
  const [busy, setBusy] = useState(false);
  const [clearingScope, setClearingScope] = useState<StatisticsStorageScope>("details");
  const clearing = storage?.clearing ?? null;

  useEffect(() => {
    let active = true;
    setError(null);
    void loadWhenVisible(visible, loadStatisticsStorage)
      .then(value => { if (active && value) setStorage(value); })
      .catch(cause => { if (active) setError(String(cause)); });
    return () => { active = false; };
  }, [attempt, visible]);

  // 清理是后台分批跑的：服务端返回的 `clearing` 消失才算清完，中间要一直问，否则界面会停在
  // "正在清理"上，或者更糟——拿着清完前的数字告诉用户"没清掉多少"。
  const polling = Boolean(clearing) && visible;
  useEffect(() => {
    if (!polling) return;
    let active = true;
    const handle = timers.setTimeout(() => {
      void loadStatisticsStorage()
        .then(value => { if (active) setStorage(value); })
        .catch(() => { if (active) setError(t("清理进度读取失败，稍后自动重试。")); });
    }, CLEAR_POLL_MS);
    return () => { active = false; timers.clearTimeout(handle); };
  }, [polling, storage, timers]);

  const clear = async () => {
    setBusy(true);
    setError(null);
    try {
      setClearingScope(scope);
      setStorage(await api.clearStatisticsStorage(scope));
      setOpen(false);
      await appStore.refresh();
    } catch (cause) { setError(String(cause)); }
    finally { setBusy(false); }
  };
  const hint = storage ? storageHint(storage) : null;
  const conversationDays = retentionDays(storage?.conversation_retention_days);
  const artifactDays = retentionDays(storage?.artifact_retention_days);
  const clearable = storage ? hasWebCache(storage) : false;
  const progress = clearing ? clearingCounter(clearingScope, clearing) : 0;
  return <>
    <TitledCard title={t("存储管理")}>
      <div className={styles.settingRow}>
        <div><strong>{storage ? formatBytes(storage.bytes) : error ? t("读取失败") : t("计算中…")}</strong>
          {storage && <small>{t("{calls} 次调用 · {traces} 条追踪 · 数据库文件 {database}", { calls: storage.call_count, traces: storage.trace_count, database: formatBytes(storage.database_bytes) })}</small>}
        </div>
        <Button size="small" disabled={!storage || busy} onClick={() => { setScope("details"); setError(null); setOpen(true); }}>{t("清理存储空间")}</Button>
      </div>
      {/* 抓取缓存不在数据库里，服务端分开报，界面也分行显示：把它并进上面那个数字会让
          "清完数据库文件没变小"更难解释，而它其实是唯一能被一键清干净的部分。 */}
      <div className={styles.settingRow}>
        <div><strong>{storage ? formatBytes(storage.web_cache_bytes) : error ? t("读取失败") : t("计算中…")}</strong>
          {storage && <small>{t("抓取缓存 · {files} 个文件（与库内占用分开统计）", { files: storage.web_cache_files })}</small>}
        </div>
        <Button size="small" disabled={!storage || !clearable || busy} onClick={() => { setScope("web_cache"); setError(null); setOpen(true); }}>{t("清除抓取缓存")}</Button>
      </div>
      {clearing && <p className={styles.storageHint} role="status">
        {clearingScope === "web_cache"
          ? t("正在清理抓取缓存：已删除 {files} 个文件…", { files: progress })
          : t("正在清理：已删除 {rows} 行…", { rows: progress })}
      </p>}
      {/* 两种"大"要分开说：观测数据可以清掉，对话内容不能——不然用户会以为点一下能腾出整个数据库。 */}
      {hint && <p className={styles.storageHint} role="status">{hint === "clean-observability"
        ? t("可清理的观测数据已超过 {threshold}：清理详细记录能收回这部分空间，调用列表与用量统计不受影响。", { threshold: formatBytes(storageHintThreshold(hint)) })
        : t("数据库文件已超过 {threshold}，其中大部分是对话内容与内容块（为前缀缓存保留，这里的清理不删它们）；清理只让文件不再继续变大。", { threshold: formatBytes(storageHintThreshold(hint)) })}</p>}
      {/* 对话内容那部分也不是无底洞：服务端自己的副本按闲置窗口自动回收，卡片必须说出这个策略。 */}
      {conversationDays > 0 && <p className={styles.storageHint}>{t("服务端的对话副本闲置 {days} 天后自动回收；对话本身保存在客户端，继续使用旧对话时会按需取回。", { days: conversationDays })}</p>}
      {/* 大工具结果与压缩历史同样占磁盘，但不在库里、也不归"清理存储空间"管：服务端在写入
          路径上按保留期惰性清理，这里只说清"它会自己过期"。 */}
      {storage && <p className={styles.storageHint}>{artifactDays > 0
        ? t("大工具结果与压缩历史：{size}（随会话一起回收；没有引用的文件最多再留 {days} 天）", { size: formatBytes(storage.artifact_bytes), days: artifactDays })
        : t("大工具结果与压缩历史：{size}（随会话一起回收）", { size: formatBytes(storage.artifact_bytes) })}</p>}
      {/* 日志同样按天滚动、不删就只增不减；保留期与代码里的常量一致，别在这里另写一个数字。 */}
      <p className={styles.storageHint}>{t("日志文件保留 {days} 天，过期自动删除。", { days: LOG_RETENTION_DAYS })}</p>
      {error && !open && <div className={styles.inlineError} role="alert"><span>{error}</span><Button size="small" onClick={() => setAttempt(value => value + 1)}>{t("重试")}</Button></div>}
    </TitledCard>
    <ConfirmDialog open={open} title={scope === "all" ? t("确定要清理全部统计数据吗？") : scope === "web_cache" ? t("确定要清除抓取缓存吗？") : t("确定要清理详细记录吗？")} busy={busy} cancelLabel={t("取消")} confirmLabel={t("确认清理")} onCancel={() => setOpen(false)} onConfirm={() => void clear()}>
      <div className={styles.confirmContent}>
        <Select value={scope} ariaLabel={t("清理范围")} options={[{ value: "details", label: t("仅清理详细记录") }, { value: "all", label: t("清理全部统计数据") }, { value: "web_cache", label: t("仅清除抓取缓存") }]} onChange={value => setScope(value as StatisticsStorageScope)} />
        <small>{scope === "all"
          ? t("所有调用汇总、详细内容和追踪记录都会被删除。模型配置、CA 和应用设置不会受到影响，此操作无法撤销。")
          : scope === "web_cache"
            ? t("只删除抓取结果缓存文件（{size} / {files} 个文件），调用记录、统计和配置都不受影响。", { size: formatBytes(storage?.web_cache_bytes ?? 0), files: storage?.web_cache_files ?? 0 })
            : t("仅删除请求、响应和追踪附件等详细内容，保留调用汇总、统计指标和配置。")}</small>
        {error && <small role="alert">{error}</small>}
      </div>
    </ConfirmDialog>
  </>;
}
