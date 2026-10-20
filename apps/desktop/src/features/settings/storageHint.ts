import type { ClearProgress, StatisticsStorage, StatisticsStorageScope } from "../../shared/api";

/**
 * 「存储管理」这张卡的两句提示都由数字决定，所以判定单独放这里，便于测试。
 *
 * 关键事实：这张卡的清理只删**观测数据**（调用明细、请求/响应正文、追踪附件），
 * 数据库文件的大头通常是对话内容与内容块（为前缀缓存保留，清理不碰），而且 SQLite
 * 只会复用释放页、文件不会因此变小。所以两种提示必须分开说，不能合成一句
 * "磁盘占用大，点这里清理"——那会让用户以为点一下能腾出 1 GB。
 */

/** 观测数据超过这个量就值得清理一次。 */
export const CLEANUP_HINT_BYTES = 256 * 1024 * 1024;
/** 数据库文件超过这个量就值得解释一句它为什么大、清理能做什么。 */
export const DATABASE_HINT_BYTES = 1024 * 1024 * 1024;

/**
 * 日志文件的保留天数，与服务端 `diagnostics.rs` 的 `LOG_RETENTION` 一致。
 *
 * 卡片要把这件事说出来，否则用户看到数据目录里一堆按天滚动的日志会以为又是无底洞。
 */
export const LOG_RETENTION_DAYS = 7;

export type StorageHint = "clean-observability" | "conversation-data" | null;

export function storageHint(storage: Pick<StatisticsStorage, "bytes" | "database_bytes">): StorageHint {
  if (storage.bytes >= CLEANUP_HINT_BYTES) return "clean-observability";
  if (storage.database_bytes >= DATABASE_HINT_BYTES) return "conversation-data";
  return null;
}

/** 把阈值写成界面上的说法，避免提示与实际判定不一致。 */
export function storageHintThreshold(hint: Exclude<StorageHint, null>): number {
  return hint === "clean-observability" ? CLEANUP_HINT_BYTES : DATABASE_HINT_BYTES;
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
export function formatBytes(value: number): string {
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

