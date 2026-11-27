import { useEffect, useRef, useState, type ReactNode } from "react";
import { api, type StatisticsStorage, type StatisticsStorageScope } from "../../shared/api";
import { appStore, useAppStoreSelector } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { StatusPill } from "../../shared/ui/StatusPill";
import { TitledCard } from "../../shared/ui/TitledCard";
import { browserTimers, type PollTimers } from "../../shared/utils/pollTimers";
import { loadWhenVisible } from "../../shared/utils/visibleLoad";
import { loadStatisticsStorage } from "./settingsLoaders";
import { clearingCounter, formatBytes, hasWebCache, LOG_RETENTION_DAYS, retentionDays } from "./storageHint";
import styles from "./SettingsPage.module.scss";

/** 清理或压缩在服务端后台跑：完成之前每隔这么久问一次进度。 */
const CLEAR_POLL_MS = 1_500;

export function StorageSettingsCard({ visible, timers = browserTimers }: { visible: boolean; timers?: PollTimers }) {
  const developerMode = useAppStoreSelector((state) => state.developerMode);
  const [storage, setStorage] = useState<StatisticsStorage | null>(null);
  const [error, setError] = useState<string | null>(null);
  // 轮询失败与操作失败分开：轮询成功后要能自己消失，不能把上一次操作的真实错误一起清掉。
  const [pollError, setPollError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [pollNonce, setPollNonce] = useState(0);
  const [dialog, setDialog] = useState<StatisticsStorageScope | null>(null);
  const [compactOpen, setCompactOpen] = useState(false);
  // "重试"要重放失败的那个动作；只是重新读一遍数据会让用户以为清理/压缩又跑了一次。
  const [failedAction, setFailedAction] = useState<
    null | { kind: "clear"; scope: StatisticsStorageScope } | { kind: "compact" }
  >(null);
  const [busy, setBusy] = useState(false);
  const [clearingScope, setClearingScope] = useState<StatisticsStorageScope>("details");
  const clearing = storage?.clearing ?? null;

  useEffect(() => {
    let active = true;
    setError(null);
    setPollError(null);
    void loadWhenVisible(visible, loadStatisticsStorage)
      .then(value => { if (active && value) setStorage(value); })
      .catch(cause => { if (active) setError(cause instanceof Error ? cause.message : String(cause)); });
    return () => { active = false; };
  }, [attempt, visible]);

  // 清理和整库压缩都在后台跑。`compaction_requested` 可能先等活跃请求结束，`compacting`
  // 才是真正重写文件；两者都消失才停止轮询。
  const polling = visible && Boolean(clearing || storage?.compaction_requested || storage?.compacting);
  useEffect(() => {
    if (!polling) return;
    let active = true;
    const handle = timers.setTimeout(() => {
      void loadStatisticsStorage()
        .then(value => {
          if (active) {
            setStorage(value);
            setPollError(null);
          }
        })
        .catch(() => {
          if (active) {
            setPollError(t("存储进度读取失败，稍后自动重试。"));
            setPollNonce(value => value + 1);
          }
        });
    }, CLEAR_POLL_MS);
    return () => { active = false; timers.clearTimeout(handle); };
  }, [polling, storage, timers, pollNonce]);

  const clear = async (scope: StatisticsStorageScope) => {
    setBusy(true);
    setError(null);
    setPollError(null);
    try {
      setClearingScope(scope);
      setStorage(await api.clearStatisticsStorage(scope));
      setDialog(null);
      setFailedAction(null);
      await appStore.refresh();
    } catch (cause) {
      setFailedAction({ kind: "clear", scope });
      setError(cause instanceof Error ? cause.message : String(cause));
    }
    finally { setBusy(false); }
  };

  const compact = async () => {
    setBusy(true);
    setError(null);
    setPollError(null);
    try {
      setStorage(await api.compactStatisticsStorage());
      setCompactOpen(false);
      setFailedAction(null);
      await appStore.refresh();
    } catch (cause) {
      setFailedAction({ kind: "compact" });
      setError(cause instanceof Error ? cause.message : String(cause));
    }
    finally { setBusy(false); }
  };

  const conversationDays = retentionDays(storage?.conversation_retention_days);
  const callDetailDays = retentionDays(storage?.call_detail_retention_days);
  const artifactDays = retentionDays(storage?.artifact_retention_days);
  const progress = clearing ? clearingCounter(clearingScope, clearing) : 0;
  const maintenanceActive = Boolean(clearing || storage?.compaction_requested || storage?.compacting);
  const wasMaintenanceActiveRef = useRef(false);
  useEffect(() => {
    if (wasMaintenanceActiveRef.current && !maintenanceActive) {
      void appStore.refresh();
    }
    wasMaintenanceActiveRef.current = maintenanceActive;
  }, [maintenanceActive]);
  const actionBlocked = busy || maintenanceActive;
  const canCompact = Boolean(storage && Number.isFinite(storage.database_free_bytes) && storage.database_free_bytes > 0 && !actionBlocked);
  const usedBytes = storage && Number.isFinite(storage.database_used_bytes) ? storage.database_used_bytes : 0;
  const totalBytes = storage && Number.isFinite(storage.database_bytes) ? storage.database_bytes : 0;
  const usedPercent = totalBytes > 0
    ? Math.min(100, Math.max(0, Math.round((usedBytes / totalBytes) * 100)))
    : 0;
  const freeBytes = storage?.database_free_bytes ?? 0;
  const conversationDetail = conversationDays > 0
    ? t("{messages} 条消息 · {blobs} 个内容块；客户端保留主副本，服务端续聊缓存闲置 {days} 天后自动回收", {
      messages: storage?.message_count ?? 0,
      blobs: storage?.blob_count ?? 0,
      days: conversationDays,
    })
    : t("{messages} 条消息 · {blobs} 个内容块", {
      messages: storage?.message_count ?? 0,
      blobs: storage?.blob_count ?? 0,
    });
  const callStatsDetail = t("{calls} 次调用 · {traces} 条追踪；概览、费用和用量统计，不会自动清理", {
    calls: storage?.call_count ?? 0,
    traces: storage?.trace_count ?? 0,
  });
  const callDetail = callDetailDays > 0
    ? t("请求/响应明细保留 {days} 天后自动回收；清理它不会影响统计", { days: callDetailDays })
    : t("请求/响应明细；清理它不会影响统计");
  const artifactDetail = artifactDays > 0
    ? t("随会话一起回收；没有引用的文件最多再留 {days} 天", { days: artifactDays })
    : t("随会话一起回收");

  const dialogTitle = dialog === "web_cache"
    ? t("确定清除抓取缓存吗？")
    : t("确定清理详细记录吗？");
  const dialogConfirm = dialog === "web_cache"
    ? t("清除抓取缓存")
    : t("清理详细记录");

  return <>
    <TitledCard
      title={t("存储管理")}
      description={t("占用、清理与数据库维护")}
      collapsible
      storageKey="settings-storage"
    >
      <div className={styles.storageSummary}>
        <div>
          <span>{t("数据库文件")}</span>
          <strong>{storage ? formatBytes(storage.database_bytes) : error ? t("读取失败") : t("计算中…")}</strong>
          {storage && <small>{t("有效数据 {used} · 可回收空闲页 {free}", {
            used: formatBytes(storage.database_used_bytes),
            free: formatBytes(storage.database_free_bytes),
          })}</small>}
        </div>
        <div className={styles.storageSummaryActions}>
          {storage && <StatusPill tone="ok">{t("自动维护已开启")}</StatusPill>}
          {canCompact && <Button size="small" onClick={() => { setError(null); setCompactOpen(true); }}>
            {t("压缩数据库")}
          </Button>}
        </div>
      </div>

      {storage && <>
        <div className={styles.storageMeter} role="img" aria-label={t("有效数据占数据库文件 {percent}%", { percent: usedPercent })}>
          <span style={{ width: `${usedPercent}%` }} />
        </div>
        <div className={styles.storageLegend}>
          <span>{t("有效数据 {size}", { size: formatBytes(storage.database_used_bytes) })}</span>
          <span>{t("空闲页 {size}", { size: formatBytes(storage.database_free_bytes) })}</span>
          <span>{t("WAL 文件 {size}", { size: formatBytes(storage.wal_bytes) })}</span>
        </div>
      </>}

      <p className={styles.storageMaintenance}>
        {t("空闲时每 60 秒检查一次：过期调用明细、闲置会话、日志和临时文件会自动回收；空闲页超过 512 MB 时自动压缩数据库。")}
      </p>

      <div className={styles.storageBreakdown}>
        <div className={styles.storageSectionLabel}>{t("空间去向")}</div>
        <StorageRow
          label={t("对话与附件")}
          value={formatBytes(storage?.conversation_bytes ?? 0)}
          detail={conversationDetail}
          tag={t("续聊缓存")}
          tagTone="idle"
        />
        <StorageRow
          label={t("统计与用量")}
          value={formatBytes(storage?.call_record_bytes ?? 0)}
          detail={callStatsDetail}
          tag={t("保留")}
          tagTone="idle"
        />
        <StorageRow
          label={t("请求、响应正文与追踪附件")}
          value={formatBytes(storage?.call_detail_bytes ?? 0)}
          detail={callDetail}
          tag={t("自动清理")}
          tagTone="info"
          action={<Button size="small" disabled={!storage || actionBlocked || (storage?.call_detail_bytes ?? 0) === 0} onClick={() => { setError(null); setDialog("details"); }}>{t("立即清理")}</Button>}
        />
        {freeBytes > 0 && <StorageRow
          label={t("数据库空闲页")}
          value={formatBytes(freeBytes)}
          detail={t("不是仍在使用的数据；压缩数据库后会归还磁盘。")}
          tag={t("可回收")}
          tagTone="warn"
        />}
        <StorageRow
          label={t("抓取缓存")}
          value={formatBytes(storage?.web_cache_bytes ?? 0)}
          detail={t("{files} 个文件 · 位于文件系统，不占用数据库文件", { files: storage?.web_cache_files ?? 0 })}
          tag={t("可清理")}
          tagTone="warn"
          action={<Button size="small" disabled={!storage || actionBlocked || !hasWebCache(storage)} onClick={() => { setError(null); setDialog("web_cache"); }}>{t("清除抓取缓存")}</Button>}
        />
        <StorageRow
          label={t("运行日志")}
          value={formatBytes(storage?.log_bytes ?? 0)}
          detail={`${t("保留 {days} 天", { days: LOG_RETENTION_DAYS })} · ${t("{files} 个文件 · 位于文件系统，不占用数据库文件", { files: storage?.log_files ?? 0 })}`}
          tag={t("自动清理")}
          tagTone="info"
        />
        <StorageRow
          label={t("大工具结果与压缩历史")}
          value={formatBytes(storage?.artifact_bytes ?? 0)}
          detail={artifactDetail}
          tag={t("自动清理")}
          tagTone="info"
        />
        <StorageRow
          label={t("插件运行时与插件数据")}
          value={formatBytes(storage?.plugin_bytes ?? 0)}
          detail={t("{files} 个文件 · 插件运行时、已安装插件和账号数据；在插件页管理", { files: storage?.plugin_files ?? 0 })}
          tag={t("手动管理")}
          tagTone="idle"
        />
      </div>

      {developerMode && <details className={styles.storageAdvanced}>
        <summary>{t("技术详情")}</summary>
        <div className={styles.storageAdvancedBody}>
          <div className={styles.storageTechnical}>
            <div className={styles.storageSectionLabel}>{t("数据库内部明细（估算，不与文件大小相加）")}</div>
            <StorageRow
              label={t("调用摘要与用量")}
              value={t("约 {size}", { size: formatBytes(storage?.call_record_bytes ?? 0) })}
              detail={t("{calls} 次调用 · {traces} 条追踪；概览、费用和用量的数据来源，不会自动清理", {
                calls: storage?.call_count ?? 0,
                traces: storage?.trace_count ?? 0,
              })}
            />
            <StorageRow
              label={t("请求、响应正文与追踪附件")}
              value={formatBytes(storage?.call_detail_bytes ?? 0)}
              detail={t("清理详细记录只删这部分；调用摘要、用量和对话都会保留")}
            />
            <StorageRow
              label="WAL"
              value={formatBytes(storage?.wal_bytes ?? 0)}
              detail={t("SQLite 的独立日志文件；正常由 checkpoint 自动截断。")}
            />
            <div className={styles.storageSectionLabel}>{t("文件系统（独立于数据库）")}</div>
            <StorageRow
              label={t("抓取缓存")}
              value={formatBytes(storage?.web_cache_bytes ?? 0)}
              detail={t("{files} 个文件", { files: storage?.web_cache_files ?? 0 })}
            />
          </div>
        </div>
      </details>}

      {storage?.compaction_requested && !storage.compacting && <p className={styles.storageStatus} role="status">
        {t("压缩已排队，正在等待当前 Agent 请求结束；不会打断正在进行的调用。")}
      </p>}
      {storage?.compacting && <p className={styles.storageStatus} role="status">
        {t("正在压缩数据库并归还空闲页；请保持应用运行，完成后界面会自动刷新。")}
      </p>}
      {storage?.compaction_failed && !storage.compacting && <p className={styles.inlineError} role="alert">
        {t("上次压缩未完成。请检查可用磁盘空间；应用日志里有具体原因，处理后可以重试。")}
      </p>}
      {clearing && <p className={styles.storageStatus} role="status">
        {clearingScope === "web_cache"
          ? progress > 0
            ? t("正在清理抓取缓存：已删除 {files} 个文件…", { files: progress })
            : t("正在清理抓取缓存…")
          : t("正在清理：已删除 {rows} 行…", { rows: progress })}
      </p>}
      {error && !dialog && !compactOpen && <div className={styles.inlineError} role="alert"><span>{error}</span><Button size="small" onClick={() => {
        if (failedAction?.kind === "clear") void clear(failedAction.scope);
        else if (failedAction?.kind === "compact") void compact();
        else setAttempt(value => value + 1);
      }}>{t("重试")}</Button></div>}
      {pollError && <div className={styles.inlineError} role="alert"><span>{pollError}</span></div>}
    </TitledCard>

    <ConfirmDialog open={dialog !== null} destructive title={dialogTitle} busy={busy} cancelLabel={t("取消")} confirmLabel={dialogConfirm} onCancel={() => setDialog(null)} onConfirm={() => { if (dialog) void clear(dialog); }}>
      <div className={styles.confirmContent}>
        <small>{dialog === "web_cache"
          ? t("只删除抓取结果缓存文件（{size} / {files} 个文件），调用记录、对话和配置都不受影响。", { size: formatBytes(storage?.web_cache_bytes ?? 0), files: storage?.web_cache_files ?? 0 })
          : t("将删除请求、响应和追踪附件，约 {size}。调用摘要、统计指标、对话消息和配置都会保留；数据库文件不会因此立刻变小。", { size: formatBytes(storage?.call_detail_bytes ?? 0) })}</small>
        {error && <small role="alert">{error}</small>}
      </div>
    </ConfirmDialog>

    <ConfirmDialog open={compactOpen} title={t("压缩数据库？")} busy={busy} cancelLabel={t("取消")} confirmLabel={t("开始压缩")} onCancel={() => setCompactOpen(false)} onConfirm={() => void compact()}>
      <div className={styles.confirmContent}>
        <small>{t("将重写 SQLite 文件，预计归还 {size} 空闲页；有效数据、对话和配置都会保留。", { size: formatBytes(storage?.database_free_bytes ?? 0) })}</small>
        <small>{t("压缩可能需要数十秒并占用额外临时磁盘空间，只在没有活跃 Agent 请求时执行。")}</small>
        {!storage?.incremental_vacuum_enabled && <small>{t("完成后会启用增量页回收，以后清理数据时会在空闲维护窗口小步归还。")}</small>}
        {error && <small role="alert">{error}</small>}
      </div>
    </ConfirmDialog>
  </>;
}

function StorageRow({ label, value, detail, action, tag, tagTone = "idle" }: {
  label: string;
  value: string;
  detail: string;
  action?: ReactNode;
  tag?: string;
  tagTone?: "ok" | "warn" | "info" | "idle";
}) {
  return <div className={styles.storageItem}>
    <div className={styles.storageItemText}>
      <strong>{label}</strong>
      <small>{detail}</small>
    </div>
    <div className={styles.storageItemValue}>
      {tag && <StatusPill tone={tagTone}>{tag}</StatusPill>}
      <span>{value}</span>
      {action}
    </div>
  </div>;
}
