import type { ClearProgress, StatisticsStorage, StatisticsStorageScope } from "../../shared/api";

/**
 * 「存储管理」这张卡的两句提示都由数字决定，所以判定单独放这里，便于测试。
 *
 * 关键事实：SQLite 的 DELETE 只把页放回 freelist，文件不会因此变小；真正缩小必须压缩。
 * 同时，可清理的观测数据和对话内容也不是一回事。两类提示必须分别指向对应动作，不能
 * 合成一句"磁盘占用大，点这里清理"。
 */

/** 观测数据超过这个量就值得清理一次。 */
export const CLEANUP_HINT_BYTES = 256 * 1024 * 1024;
/** 空闲页超过这个量就值得建议压缩；几十 MB 的波动通常下次写入会自己复用。 */
export const RECLAIM_HINT_BYTES = 128 * 1024 * 1024;

/**
 * 日志文件的保留天数，与服务端 `diagnostics.rs` 的 `LOG_RETENTION` 一致。
 *
 * 卡片要把这件事说出来，否则用户看到数据目录里一堆按天滚动的日志会以为又是无底洞。
 */
export const LOG_RETENTION_DAYS = 7;

export type StorageHint = "reclaim-space" | "clean-observability" | null;

export function storageHint(
  storage: Pick<StatisticsStorage, "observability_bytes" | "database_free_bytes">,
): StorageHint {
  if (storage.database_free_bytes >= RECLAIM_HINT_BYTES) return "reclaim-space";
  if (storage.observability_bytes >= CLEANUP_HINT_BYTES) return "clean-observability";
  return null;
}

/** 把阈值写成界面上的说法，避免提示与实际判定不一致。 */
export function storageHintThreshold(hint: Exclude<StorageHint, null>): number {
  return hint === "reclaim-space" ? RECLAIM_HINT_BYTES : CLEANUP_HINT_BYTES;
}

/**
 * 卡片里印在句子中间的保留天数（对话副本回收窗口、落盘目录保留期）。
 *
 * 窗口由服务端只读字段给出，界面不自己算：数字必须与服务端真正跑的清理常量一致。
 * 字段缺失或不是正数时退回 0，调用方据此不显示数字——宁可不提天数，也不印一个
 * "闲置 0 天"或"NaN 天"。
 */
export function retentionDays(value: number | undefined | null): number {
  return typeof value === "number" && Number.isFinite(value) && value > 0 ? Math.floor(value) : 0;
}

/** 字节数的界面写法：1024 进制，最多一位小数。 */
export function formatBytes(value: number | null | undefined): string {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) return "0 B";
  if (value < 1024) return `${value} B`;
  const index = Math.min(3, Math.floor(Math.log(value) / Math.log(1024)));
  return `${(value / 1024 ** index).toFixed(1)} ${["B", "KB", "MB", "GB"][index]}`;
}

/**
 * 抓取缓存此刻有没有东西可清。
 *
 * 判据是文件数或体积之一非零：文件数比体积更能说明"有没有东西"（一堆空文件也算占用），
 * 而体积可能因为取整显示成 `0 B`。都没有时按钮置灰，而不是让用户点一个什么也不做的按钮。
 */
export function hasWebCache(storage: Pick<StatisticsStorage, "web_cache_bytes" | "web_cache_files">): boolean {
  return storage.web_cache_files > 0 || storage.web_cache_bytes > 0;
}

/**
 * 这次清理的进度该看哪个计数。
 *
 * 服务端只有 `web_cache` 作用域会动 `deleted_files`（其它作用域一直是 0），所以按作用域取
 * 对应的那一个：拿行数去显示"删了 0 个文件"会让人以为清理卡住了。
 */
export function clearingCounter(scope: StatisticsStorageScope, progress: ClearProgress): number {
  return scope === "web_cache" ? progress.deleted_files : progress.deleted_rows;
}

