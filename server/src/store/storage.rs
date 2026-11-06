//! Persists content-addressed blobs and their edges.
//! Storage accounting and cleanup for disposable observability data.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use serde::{Deserialize, Serialize};
use sqlx::{Connection, SqliteConnection};
use tokio::sync::Notify;

use crate::Result;

use super::{now_ms, retention::RETENTION_MS, BlobId, Store};

/// 「清理详细记录 / 清理全部统计」每批删除的行数上限。
///
/// 一次事务删完两张最大的明细表会让写锁与 WAL 一直被这一条管理请求占住：桌面端的管理
/// 请求 15 秒超时，请求被取消时整笔删除随事务一起回滚，用户看到"清理失败"而且一行都
/// 没清掉。分批之后每批独立提交、批间放掉写锁：请求即使超时，删掉的批次留下来了，
/// 重试接着往下清（与回收趟"每轮限量"的取舍相同，见
/// `retention::MAX_PRUNED_TRACES_PER_PASS`）。
const CLEAR_DETAILS_BATCH_ROWS: i64 = 4_096;

/// `llm_calls` 一行连同索引的保守估算。这里不逐行 `LENGTH(...)`：设置页每次挂载都会读
/// 统计，61k 行真实库上那条表达式曾让整次读取变成数秒级全表扫描。摘要正文里的长字段很
/// 少见，界面明确标成“约”；请求/响应正文仍按精确的 `byte_count` 单列。
const CALL_RECORD_BYTES_ESTIMATE: i64 = 640;
/// 一条轨迹摘要连同 `received_at_ms` 索引的保守估算。
const TRACE_RECORD_BYTES_ESTIMATE: i64 = 128;
/// 增量回收每次最多归还的页数（4 KiB 页下约 8 MiB）。保持维护循环一次只做一小段，
/// 不让它像一次 `VACUUM` 那样长时间占住写路径。
const INCREMENTAL_VACUUM_PAGES_PER_PASS: i64 = 2_048;
/// 少于这么多空闲页（约 1 MiB）时先不动，避免维护循环为几页反复重排 B 树。
const MIN_FREE_PAGES_TO_RECLAIM: i64 = 256;

/// 设置页“存储管理”的数据。所有 `*_bytes` 都是字节，`*_count` 都是行数/文件数。
///
/// 这里把四个容易被混成一个数字的概念分开：
/// * `database_bytes`：SQLite 文件当前分配的全部页；
/// * `database_used_bytes`：有效页，仍装着数据或索引；
/// * `database_free_bytes`：`DELETE` 后留在 freelist 中、可供复用但尚未归还磁盘的页；
/// * `observability_bytes`：可以手动清理的调用摘要与明细估算。
///
/// 对话消息与内容块、抓取缓存、落盘工件又各自单列：它们的生命周期和清理入口都不同。
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct StatisticsStorage {
    /// 数据库文件的当前大小：清理不会让它变小（SQLite 复用释放页）。
    pub database_bytes: i64,
    /// 数据库文件里仍被有效页占用的部分（含索引与页开销）。
    pub database_used_bytes: i64,
    /// 已释放、等待复用或压缩归还的空闲页。
    pub database_free_bytes: i64,
    /// SQLite 的 `-wal` 独立文件；正常由 checkpoint 自动截断，不属于主库页。
    pub wal_bytes: i64,
    /// `llm_calls` 摘要与轨迹索引的估算大小；清空调用历史会删除它们。
    pub call_record_bytes: i64,
    /// 请求/响应正文和追踪附件的估算大小；清理详细记录只删这一部分。
    pub call_detail_bytes: i64,
    /// `call_record_bytes + call_detail_bytes`，即清空调用历史会回收的估算总量。
    pub observability_bytes: i64,
    pub call_count: i64,
    pub trace_count: i64,
    /// 对话消息正文与内容块正文的估算大小；清理统计不碰它们。
    pub conversation_bytes: i64,
    pub message_count: i64,
    /// 内容块数量（含对话 checkpoint、工具图片与追踪附件引用的块）。
    pub blob_count: i64,
    /// 落盘的大工具结果与压缩历史（`tool-results`、`compaction-history`）合计大小。
    pub artifact_bytes: i64,
    /// 上面那两个目录的保留天数（它们与日志各自按 7 天清理）。
    pub artifact_retention_days: i64,
    /// 服务端对话副本的闲置回收窗口（天）：见 `retention::CONVERSATION_RETENTION_MS`。
    ///
    /// 只读、不可配置：界面用它说明"这块空间不是无底洞"，而不是给一个可以调小的旋钮
    /// （调小会让旧会话更依赖客户端自己的副本，调大会让数据库重新膨胀）。
    pub conversation_retention_days: i64,
    /// 抓取结果缓存（`<数据目录>/cache/web`）的当前占用。
    ///
    /// 它不在数据库里，所以与库内统计分开报：把文件系统的数字混进"可清理的观测数据"会让
    /// "清理之后数据库文件没变小"这件事更难解释。界面应当把它单列一行，并且只有
    /// `web_cache` 作用域会清它。
    pub web_cache_bytes: i64,
    pub web_cache_files: i64,
    /// 滚动日志目录的占用与文件数；它们不在数据库里，单独显示。
    pub log_bytes: i64,
    pub log_files: i64,
    /// 插件目录（运行时、已安装插件、账号数据）的递归占用；不在数据库里。
    pub plugin_bytes: i64,
    pub plugin_files: i64,
    /// 当前数据库是否已启用增量 `auto_vacuum`。已有库第一次启用需要一次整库压缩。
    pub incremental_vacuum_enabled: bool,
    /// 有后台清理在跑时是它此刻的进度：界面据此显示"正在清理"，而不是把"还没清完"
    /// 当成失败。没有任务在跑时是 `None`。
    pub clearing: Option<ClearProgress>,
    /// 已经请求压缩，正在等待所有 Agent 请求结束。
    pub compaction_requested: bool,
    /// 数据库正在被整库重写；此时界面应继续轮询而不是允许第二个任务。
    pub compacting: bool,
    /// 上一次压缩失败。界面据此给出可操作提示，而不是让“排队中”悄悄消失。
    pub compaction_failed: bool,
}

/// 一次后台清理的进度快照。
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct ClearProgress {
    /// 这次清理已经删掉的行数（累计；不含外键级联带走的行）。
    pub deleted_rows: u64,
    /// 这次清理已经删掉的缓存文件数：只有 `web_cache` 作用域会动它，其它作用域一直是 0。
    pub deleted_files: u64,
}

/// 后台清理、压缩与页回收的共享状态。
///
/// 一个库只有一条写路径，所以状态挂在 `Store` 上而不是每个请求里。`wake` 让用户点下
/// “压缩数据库”时立刻叫醒维护循环；没有它只能等下一次 60 秒 tick。
#[derive(Default)]
pub(crate) struct StorageMaintenanceState {
    clearing: AtomicBool,
    deleted_rows: AtomicU64,
    deleted_files: AtomicU64,
    compaction_requested: AtomicBool,
    compacting: AtomicBool,
    compaction_failed: AtomicBool,
    wake: Arc<Notify>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StatisticsStorageScope {
    #[default]
    Details,
    All,
    /// 只清抓取结果缓存（文件系统）：它不进数据库，也就不该被"清理全部统计"顺手带走
    /// ——那是两个不同的东西，用户点哪一个应该清哪一个。
    WebCache,
}

impl Store {
    /// 存储分解。
    ///
    /// 大正文全部读各自已存的 `byte_count`，调用摘要按行数估算：设置页每次挂载都会问
    /// 这个接口，逐行 `LENGTH(body_json)` / `LENGTH(error_message)` 会让 61k 行的真实库
    /// 变成数秒级全表扫描。消息与内容块则只扫正文列本身，用于说明“几十 MB 逻辑数据”
    /// 到底在哪，不把 SQLite 索引和空闲页混进去。
    pub async fn statistics_storage(&self) -> Result<StatisticsStorage> {
        let (
            database_bytes,
            database_free_bytes,
            call_count,
            trace_count,
            call_detail_bytes,
            conversation_bytes,
            message_count,
            blob_count,
            auto_vacuum,
            database_path,
        ) = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64, i64, i64, String)>(
            r#"
            SELECT
                COALESCE((SELECT page_count * page_size
                          FROM pragma_page_count(), pragma_page_size()), 0),
                COALESCE((SELECT freelist_count * page_size
                          FROM pragma_freelist_count(), pragma_page_size()), 0),
                (SELECT COUNT(*) FROM llm_calls),
                (SELECT COUNT(*) FROM cursor_run_traces),
                COALESCE((SELECT SUM(byte_count + LENGTH(headers_json) + 24)
                          FROM llm_call_requests), 0) +
                COALESCE((SELECT SUM(byte_count + 24)
                          FROM llm_call_response_chunks), 0) +
                COALESCE((SELECT SUM(
                              byte_count + LENGTH(artifact_type) + LENGTH(source) +
                              LENGTH(metadata_json) + 48
                          ) FROM cursor_run_trace_artifacts), 0),
                COALESCE((SELECT SUM(LENGTH(payload_json) + 64)
                          FROM messages), 0) +
                COALESCE((SELECT SUM(LENGTH(data) + 32)
                          FROM blobs), 0),
                (SELECT COUNT(*) FROM messages),
                (SELECT COUNT(*) FROM blobs),
                COALESCE((SELECT auto_vacuum FROM pragma_auto_vacuum()), 0),
                COALESCE((SELECT file FROM pragma_database_list() WHERE name = 'main'), '')
            "#,
        )
        .fetch_one(&self.pool)
        .await?;

        // 抓取结果缓存的占用要读目录（几百个 stat），放进阻塞线程池：这是设置页一挂载
        // 就会问的接口，不该让它去占住 async 工作线程。
        let (web_cache, log_usage, plugin_usage, wal_bytes) =
            tokio::task::spawn_blocking(move || {
                let wal_bytes = if database_path.is_empty() {
                    0
                } else {
                    std::fs::metadata(format!("{database_path}-wal"))
                        .map(|metadata| metadata.len())
                        .unwrap_or(0)
                };
                (
                    crate::diagnostics::web_cache_usage(),
                    crate::diagnostics::log_usage(),
                    crate::diagnostics::plugin_usage(),
                    wal_bytes,
                )
            })
            .await
            .unwrap_or_default();
        // 大工具结果与压缩历史同样在文件系统里（每个目录最多几千个文件）：一并放到阻塞
        // 线程池里数，界面才能说清"这块空间也有一天会过期"。
        let artifact_bytes = tokio::task::spawn_blocking(artifact_usage)
            .await
            .unwrap_or(0);

        let database_free_bytes = database_free_bytes.clamp(0, database_bytes);
        let database_used_bytes = database_bytes - database_free_bytes;
        let call_record_bytes =
            call_count * CALL_RECORD_BYTES_ESTIMATE + trace_count * TRACE_RECORD_BYTES_ESTIMATE;
        Ok(StatisticsStorage {
            database_bytes,
            database_used_bytes,
            database_free_bytes,
            wal_bytes: wal_bytes as i64,
            call_record_bytes,
            call_detail_bytes,
            observability_bytes: call_record_bytes + call_detail_bytes,
            call_count,
            trace_count,
            conversation_bytes,
            message_count,
            blob_count,
            artifact_bytes: artifact_bytes as i64,
            artifact_retention_days: crate::model::ARTIFACT_RETENTION_DAYS,
            conversation_retention_days: super::retention::CONVERSATION_RETENTION_DAYS,
            web_cache_bytes: web_cache.bytes as i64,
            web_cache_files: web_cache.files as i64,
            log_bytes: log_usage.bytes as i64,
            log_files: log_usage.files as i64,
            plugin_bytes: plugin_usage.bytes as i64,
            plugin_files: plugin_usage.files as i64,
            incremental_vacuum_enabled: auto_vacuum == 2,
            clearing: self.clear_progress(),
            compaction_requested: self
                .storage_maintenance
                .compaction_requested
                .load(Ordering::SeqCst),
            compacting: self.storage_maintenance.compacting.load(Ordering::SeqCst),
            compaction_failed: self
                .storage_maintenance
                .compaction_failed
                .load(Ordering::SeqCst),
        })
    }

    /// 此刻是否有后台清理在跑；有就带上进度。
    fn clear_progress(&self) -> Option<ClearProgress> {
        self.storage_maintenance
            .clearing
            .load(Ordering::SeqCst)
            .then(|| ClearProgress {
                deleted_rows: self
                    .storage_maintenance
                    .deleted_rows
                    .load(Ordering::Relaxed),
                deleted_files: self
                    .storage_maintenance
                    .deleted_files
                    .load(Ordering::Relaxed),
            })
    }

    /// 启动一次后台清理并立刻返回；删除在独立任务里分批跑。
    ///
    /// 库大的时候几百个批次要跑几十秒，而桌面端的管理请求 15 秒就超时：同步等它跑完的
    /// 那个请求先失败，用户看到的是"清理失败"（旧写法里甚至可能一行都没删完就被取消）。
    /// 这里只把任务启动起来，返回的估算值还是清理前的，`clearing` 说明"已经在跑"——界面
    /// 轮询 `statistics_storage` 直到它变回 `None`，那时看到的才是清理后的数字。
    ///
    /// 同一个进程只跑一个清理任务：重复点按只是把"正在清理"再说一遍，不会叠出第二条
    /// 删除流水（两条流水互相抢写锁，进度也会互相覆盖）。
    pub async fn start_clear_statistics_storage(
        &self,
        scope: StatisticsStorageScope,
    ) -> Result<StatisticsStorage> {
        if self
            .storage_maintenance
            .compaction_requested
            .load(Ordering::SeqCst)
            || self.storage_maintenance.compacting.load(Ordering::SeqCst)
        {
            tracing::warn!("storage compaction is active; not starting a clear");
            return self.statistics_storage().await;
        }
        if self
            .storage_maintenance
            .clearing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::warn!("a statistics clear is already running; not starting a second one");
            return self.statistics_storage().await;
        }
        self.storage_maintenance
            .deleted_rows
            .store(0, Ordering::Relaxed);
        self.storage_maintenance
            .deleted_files
            .store(0, Ordering::Relaxed);
        let store = self.clone();
        tokio::spawn(async move {
            let cleared = match scope {
                StatisticsStorageScope::Details => store.clear_statistics_storage().await,
                StatisticsStorageScope::All => store.clear_all_statistics_storage().await,
                StatisticsStorageScope::WebCache => store.clear_web_cache_at(None).await,
            };
            store
                .storage_maintenance
                .clearing
                .store(false, Ordering::SeqCst);
            match cleared {
                Ok(storage) => tracing::info!(
                    deleted_rows = store
                        .storage_maintenance
                        .deleted_rows
                        .load(Ordering::Relaxed),
                    call_count = storage.call_count,
                    trace_count = storage.trace_count,
                    "cleared statistics storage"
                ),
                Err(error) => tracing::warn!(
                    error = %crate::diagnostics::error_chain(&error),
                    "clearing statistics storage failed"
                ),
            }
        });
        self.statistics_storage().await
    }

    /// 维护循环等待这个 `Notify`：用户请求压缩时立即醒来，不必空等下一个 60 秒 tick。
    pub(crate) fn storage_wake(&self) -> Arc<Notify> {
        self.storage_maintenance.wake.clone()
    }

    /// 请求一次整库压缩；真正执行仍在维护循环里，只有确认没有活跃 Agent 请求时才开始。
    ///
    /// 接口必须立即返回：1 GiB 级数据库的 `VACUUM` 会重写整个文件，远超桌面端请求超时。
    /// `compaction_requested` 让界面先显示“等待空闲”，维护循环开始后切到“正在压缩”。
    pub async fn start_storage_compaction(&self) -> Result<StatisticsStorage> {
        if self.storage_maintenance.clearing.load(Ordering::SeqCst) {
            tracing::warn!("a statistics clear is running; not starting compaction");
            return self.statistics_storage().await;
        }
        if self.storage_maintenance.compacting.load(Ordering::SeqCst) {
            return self.statistics_storage().await;
        }
        self.storage_maintenance
            .compaction_requested
            .store(true, Ordering::SeqCst);
        self.storage_maintenance
            .compaction_failed
            .store(false, Ordering::SeqCst);
        self.storage_maintenance.wake.notify_one();
        self.statistics_storage().await
    }

    /// 维护循环在确认没有活跃请求后调用。返回是否真的执行了一次压缩。
    pub(crate) async fn compact_if_requested(&self) -> Result<bool> {
        if self.storage_maintenance.clearing.load(Ordering::SeqCst)
            || self.storage_maintenance.compacting.load(Ordering::SeqCst)
            || !self
                .storage_maintenance
                .compaction_requested
                .load(Ordering::SeqCst)
        {
            return Ok(false);
        }
        let _write = self.writes.lock().await;
        if self.storage_maintenance.clearing.load(Ordering::SeqCst)
            || self.storage_maintenance.compacting.load(Ordering::SeqCst)
            || !self
                .storage_maintenance
                .compaction_requested
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Ok(false);
        }

        self.storage_maintenance
            .compacting
            .store(true, Ordering::SeqCst);
        let result = self.vacuum_database().await;
        self.storage_maintenance
            .compacting
            .store(false, Ordering::SeqCst);
        self.storage_maintenance
            .compaction_failed
            .store(result.is_err(), Ordering::SeqCst);
        result?;
        tracing::info!("database compaction completed");
        Ok(true)
    }

    async fn vacuum_database(&self) -> Result<()> {
        let mut connection = self.pool.acquire().await?;
        let auto_vacuum: i64 = sqlx::query_scalar("PRAGMA auto_vacuum")
            .fetch_one(&mut *connection)
            .await?;
        if auto_vacuum != 2 {
            // 只有 `VACUUM` 能让这个持久设置对既有库生效；之后每次维护只归还一小批页，
            // 不需要再整库重写。
            sqlx::query("PRAGMA auto_vacuum = INCREMENTAL")
                .execute(&mut *connection)
                .await?;
        }
        sqlx::query("VACUUM").execute(&mut *connection).await?;
        // `VACUUM` 会先把重写后的页写进 WAL；自动 checkpoint 只限制后续增长，不会立刻
        // 把这个临时大水槽截回。主库已经空闲，这里主动截断；短读事务暂时占用时只记警告，
        // 不把已经成功的主库压缩判成失败。
        let (busy, log_frames, checkpointed): (i64, i64, i64) =
            sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
                .fetch_one(&mut *connection)
                .await?;
        if busy != 0 {
            tracing::warn!(
                log_frames,
                checkpointed,
                "database compacted, but WAL truncation was busy"
            );
        }
        Ok(())
    }

    /// 已启用增量 `auto_vacuum` 时，每次空闲维护最多归还 8 MiB，让后续删除不再长期
    /// 堆积成第二个 1 GiB 空闲页池。第一次启用仍由用户触发的整库压缩完成。
    pub(crate) async fn reclaim_free_pages(&self) -> Result<()> {
        if self.storage_maintenance.clearing.load(Ordering::SeqCst)
            || self
                .storage_maintenance
                .compaction_requested
                .load(Ordering::SeqCst)
            || self.storage_maintenance.compacting.load(Ordering::SeqCst)
        {
            return Ok(());
        }
        let (auto_vacuum, freelist): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE((SELECT auto_vacuum FROM pragma_auto_vacuum()), 0),
                    COALESCE((SELECT freelist_count FROM pragma_freelist_count()), 0)",
        )
        .fetch_one(&self.pool)
        .await?;
        if auto_vacuum != 2 || freelist < MIN_FREE_PAGES_TO_RECLAIM {
            return Ok(());
        }

        let _write = self.writes.lock().await;
        let freelist: i64 =
            sqlx::query_scalar("SELECT freelist_count FROM pragma_freelist_count()")
                .fetch_one(&self.pool)
                .await?;
        if freelist < MIN_FREE_PAGES_TO_RECLAIM {
            return Ok(());
        }
        sqlx::query(&format!(
            "PRAGMA incremental_vacuum({INCREMENTAL_VACUUM_PAGES_PER_PASS})"
        ))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn clear_statistics_storage(&self) -> Result<StatisticsStorage> {
        let mut connection = self.maintenance_pool.acquire().await?;
        self.clear_details(&mut connection).await?;
        drop(connection);
        self.statistics_storage().await
    }

    /// 清空抓取结果缓存（文件系统，不在库里）。
    ///
    /// 目录作为参数只有一个原因：`WebCache` 作用域与用例共用同一条实现（同
    /// `diagnostics::prune_logs_in` 的做法），用例必须对着临时目录跑，不能去删用户的真实缓存。
    async fn clear_web_cache_at(
        &self,
        directory: Option<std::path::PathBuf>,
    ) -> Result<StatisticsStorage> {
        let cleared = tokio::task::spawn_blocking(move || match directory {
            Some(directory) => crate::diagnostics::clear_web_cache_in(&directory),
            None => crate::diagnostics::clear_web_cache(),
        })
        .await
        .map_err(|error| crate::Error::Store(format!("web cache clear task failed: {error}")))??;
        self.storage_maintenance
            .deleted_files
            .fetch_add(cleared.files, Ordering::Relaxed);
        self.statistics_storage().await
    }

    pub async fn clear_all_statistics_storage(&self) -> Result<StatisticsStorage> {
        let mut connection = self.maintenance_pool.acquire().await?;
        self.clear_details(&mut connection).await?;
        // 摘要行在明细之后删：级联（`llm_call_requests` / 响应分片 / 追踪工件）带走的那
        // 部分已经空了，每批要干的活只剩摘要行自己。
        for statement in [
            "DELETE FROM llm_calls WHERE rowid IN (SELECT rowid FROM llm_calls LIMIT ?)",
            "DELETE FROM cursor_run_traces WHERE rowid IN (SELECT rowid FROM cursor_run_traces LIMIT ?)",
        ] {
            self.delete_in_batches(&mut connection, statement).await?;
        }
        drop(connection);
        self.statistics_storage().await
    }

    /// 可观测明细：请求/响应正文、追踪工件，以及只被这些工件引用的内容块。
    async fn clear_details(&self, connection: &mut SqliteConnection) -> Result<()> {
        for statement in [
            "DELETE FROM llm_call_requests WHERE rowid IN (SELECT rowid FROM llm_call_requests LIMIT ?)",
            "DELETE FROM llm_call_response_chunks WHERE rowid IN (SELECT rowid FROM llm_call_response_chunks LIMIT ?)",
        ] {
            self.delete_in_batches(connection, statement).await?;
        }
        self.clear_trace_artifacts(connection).await
    }

    /// 分批删完 `statement` 选中的行：每批一个独立事务，批间释放写锁，直到没有行可删。
    ///
    /// 返回实际执行的批次数，测试用它确认清理真的是"一批一个事务"，而不是一笔记账
    /// 拖到最后。
    async fn delete_in_batches(
        &self,
        connection: &mut SqliteConnection,
        statement: &'static str,
    ) -> Result<u64> {
        let mut batches = 0;
        loop {
            let deleted = {
                let _write = self.writes.lock().await;
                let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
                let deleted = sqlx::query(statement)
                    .bind(CLEAR_DETAILS_BATCH_ROWS)
                    .execute(&mut *transaction)
                    .await?
                    .rows_affected();
                transaction.commit().await?;
                deleted
            };
            if deleted == 0 {
                return Ok(batches);
            }
            self.storage_maintenance
                .deleted_rows
                .fetch_add(deleted, Ordering::Relaxed);
            batches += 1;
        }
    }

    /// 删掉追踪工件，以及"只被这些工件引用"的内容块。
    ///
    /// 工件表里存的不全是观测载荷：`blob_sync` 把会话内容块的读写也记成工件
    /// （`blob_get` / `blob_set`），所以判定"这个块是不是观测垃圾"要排除掉所有还在被
    /// 别处引用的（会话根、blob 图、消息里的工具图片），以及观测窗口内刚写进来的
    /// ——它们可能还没被 checkpoint 钉住（同 `retention::UNROOTED_BLOB_MS` 的理由）。
    /// 删掉仍在被使用的内容块，用户看到的就不是"清理统计"而是下次续聊报错。
    ///
    /// 每批一个独立事务：先把这一批的 `blob_id` 抄进临时表，删掉工件行，再删批里
    /// 已经没人引用的内容块。临时表是连接级的，所以整趟清理固定跑在一条连接上
    /// （`clear_statistics_storage` 与 `clear_all_statistics_storage` 都是这样）。
    async fn clear_trace_artifacts(&self, connection: &mut SqliteConnection) -> Result<()> {
        self.stage_clear_details(connection).await?;
        let created_before_ms = now_ms() - RETENTION_MS;
        loop {
            let deleted = {
                let _write = self.writes.lock().await;
                let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
                sqlx::query("DELETE FROM clear_statistics_batch")
                    .execute(&mut *transaction)
                    .await?;
                sqlx::query(
                    "INSERT INTO clear_statistics_batch(artifact_rowid, blob_id)
                     SELECT rowid, blob_id FROM cursor_run_trace_artifacts LIMIT ?",
                )
                .bind(CLEAR_DETAILS_BATCH_ROWS)
                .execute(&mut *transaction)
                .await?;
                let deleted = sqlx::query(
                    "DELETE FROM cursor_run_trace_artifacts
                     WHERE rowid IN (SELECT artifact_rowid FROM clear_statistics_batch)",
                )
                .execute(&mut *transaction)
                .await?
                .rows_affected();
                sqlx::query(
                    "DELETE FROM blobs
                     WHERE blob_id IN (SELECT blob_id FROM clear_statistics_batch)
                       AND created_at_ms < ?
                       AND NOT EXISTS (
                           SELECT 1 FROM cursor_run_trace_artifacts a WHERE a.blob_id = blobs.blob_id
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM conversation_blob_roots r WHERE r.blob_id = blobs.blob_id
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM blob_edges e
                           WHERE e.parent_blob_id = blobs.blob_id OR e.child_blob_id = blobs.blob_id
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM clear_statistics_image_blobs i WHERE i.blob_id = blobs.blob_id
                       )",
                )
                .bind(created_before_ms)
                .execute(&mut *transaction)
                .await?;
                transaction.commit().await?;
                deleted
            };
            if deleted == 0 {
                return Ok(());
            }
            self.storage_maintenance
                .deleted_rows
                .fetch_add(deleted, Ordering::Relaxed);
        }
    }

    /// 建好这一趟清理用的两张临时表，并把消息里的工具图片解成 BlobID 放进去。
    ///
    /// 图片在 `message_image_blobs` 里存的是消息 payload 那份 base64（SQLite 没有 base64
    /// 解码函数），解一次放进临时表，后面的 blob 判定就能与 `blobs.blob_id` 直接比较。
    async fn stage_clear_details(&self, connection: &mut SqliteConnection) -> Result<()> {
        let mut transaction = connection.begin().await?;
        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS clear_statistics_batch(
                 artifact_rowid INTEGER PRIMARY KEY,
                 blob_id BLOB NOT NULL
             )",
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS clear_statistics_image_blobs(blob_id BLOB PRIMARY KEY)",
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query("DELETE FROM clear_statistics_image_blobs")
            .execute(&mut *transaction)
            .await?;
        let images: Vec<String> = sqlx::query_scalar("SELECT blob_id FROM message_image_blobs")
            .fetch_all(&mut *transaction)
            .await?;
        for image in images {
            let id = BlobId::from_base64(&image)?;
            sqlx::query("INSERT OR IGNORE INTO clear_statistics_image_blobs(blob_id) VALUES (?)")
                .bind(id.as_bytes().as_slice())
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }
}

/// 落盘的大工具结果与压缩历史合计多大。
///
/// 两个目录都在数据目录下、都按 [`crate::model::ARTIFACT_RETENTION_DAYS`] 天惰性清理；
/// 这里只数不删（清理发生在写入路径上，见 `tool_result_spill`）。
fn artifact_usage() -> u64 {
    match crate::config::managed_data_dir() {
        Ok(root) => artifact_usage_in(&root),
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the data directory for artifact usage");
            0
        }
    }
}

/// 数据目录下两个落盘目录的合计字节数；目录名只从 `tool_result_spill` 读，避免两处各写一份。
fn artifact_usage_in(root: &std::path::Path) -> u64 {
    [
        crate::model::SPILL_DIR,
        crate::model::COMPACTION_ARCHIVE_DIR,
    ]
    .iter()
    .map(|name| directory_usage(&root.join(name)))
    .sum()
}

/// 一个目录里直接子文件的字节数；读不到就是 0（统计不该因为缺目录而失败）。
///
/// 只数一层：两个目录目前都是平铺的（见 `tool_result_spill` 的文件名约定），嵌套目录既
/// 不占用统计，也不在保留期清理的扫描范围内。
fn directory_usage(directory: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::retention::{CONVERSATION_RETENTION_DAYS, CONVERSATION_RETENTION_MS};

    async fn temp_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        (directory, store)
    }

    /// 把内容块拨回观测窗口之前：只有这时它们才是"清理详细记录"的候选，
    /// 否则窗口内那道保护会让用例证明不了它想证明的那条守卫。
    async fn age_past_the_observability_window(store: &Store) {
        sqlx::query("UPDATE blobs SET created_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
    }

    /// 把一个内容块挂成某条追踪的工件（清理时它会成为"只被工件引用"的候选）。
    async fn link_artifact(store: &Store, request_id: &str, seq: i64, blob: &BlobId) {
        sqlx::query(
            "INSERT OR IGNORE INTO cursor_run_traces(request_id, route, status, received_at_ms)
             VALUES (?, 'local_byok', 'completed', 1)",
        )
        .bind(request_id)
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cursor_run_trace_artifacts(
                request_id, seq, artifact_type, source, blob_id, metadata_json, byte_count, created_at_ms
             ) VALUES (?, ?, 'run_sse_chunk', 'cursor_client', ?, '{}', 0, 1)",
        )
        .bind(request_id)
        .bind(seq)
        .bind(blob.as_bytes().as_slice())
        .execute(store.pool())
        .await
        .unwrap();
    }

    /// 批量造 `count` 条调用摘要，每条都带请求正文与一个响应分片（清理明细要删的两张表）。
    async fn seed_call_details(store: &Store, count: i64) {
        sqlx::query(
            "WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?)
             INSERT INTO llm_calls(call_id, run_id, conversation_id, provider_call_index,
                 provider_type, provider_url, request_type, request_url, model_id, display_name,
                 status, created_at_ms, message_count, tool_count, detailed)
             SELECT 'bulk-' || n, 'run', 'conv', 0, 'openai', 'url', 'chat', 'url', 'model',
                 'Model', 'completed', 1, 1, 0, 1 FROM seq",
        )
        .bind(count)
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO llm_call_requests(call_id, headers_json, body_json, byte_count)
             SELECT call_id, '{}', '{\"a\":1}', 7 FROM llm_calls",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO llm_call_response_chunks(call_id, seq, received_offset_ms, data, byte_count)
             SELECT call_id, 0, 0, X'01', 1 FROM llm_calls",
        )
        .execute(store.pool())
        .await
        .unwrap();
    }

    async fn count(store: &Store, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(store.pool())
            .await
            .unwrap()
    }

    /// 界面靠这个字段说明"服务端对话副本闲置多久会被回收"，所以它必须一直跟着
    /// 回收窗口走，不能是写死的第二个数字。
    #[tokio::test]
    async fn statistics_report_the_conversation_retention_window_the_recycler_uses() {
        let (_directory, store) = temp_store().await;
        let storage = store.statistics_storage().await.unwrap();
        assert_eq!(
            storage.conversation_retention_days,
            CONVERSATION_RETENTION_DAYS
        );
        assert_eq!(
            storage.conversation_retention_days,
            CONVERSATION_RETENTION_MS / (24 * 60 * 60 * 1000)
        );
        assert!(
            storage.conversation_retention_days > 0,
            "a zero window would tell the user their data is deleted immediately"
        );
    }

    /// 抓取结果缓存必须进统计：它以前既不进统计也不在界面上，用户看不到自己磁盘上
    /// 多了什么，也没有任何清除入口（回收只挂在写入之后，一小时最多扫一次）。
    #[tokio::test]
    async fn statistics_include_the_fetched_web_cache() {
        let (_directory, store) = temp_store().await;

        let storage = store.statistics_storage().await.unwrap();
        let usage = crate::diagnostics::web_cache_usage();

        assert_eq!(storage.web_cache_files, usage.files as i64);
        assert_eq!(storage.web_cache_bytes, usage.bytes as i64);
        assert!(storage.web_cache_files >= 0 && storage.web_cache_bytes >= 0);
        let logs = crate::diagnostics::log_usage();
        assert_eq!(storage.log_files, logs.files as i64);
        assert_eq!(storage.log_bytes, logs.bytes as i64);
    }

    /// `web_cache` 作用域清缓存目录，不碰数据库：清理是后台跑的（分批复用同一套
    /// 协调状态），进度里的 `deleted_files` 记的是删掉的缓存文件数。
    #[tokio::test]
    async fn the_web_cache_scope_clears_the_cache_without_touching_the_database() {
        let (_directory, store) = temp_store().await;
        seed_call_details(&store, 3).await;
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(cache.path().join("page-a.txt"), b"aaaa").unwrap();
        std::fs::write(cache.path().join("page-b.txt"), b"bbbbbb").unwrap();

        let storage = store
            .clear_web_cache_at(Some(cache.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_dir(cache.path()).unwrap().count(),
            0,
            "缓存文件要真的被删掉（目录本身留着）"
        );
        assert!(
            cache.path().is_dir(),
            "目录要被保留：服务端还要往里写新的抓取结果"
        );
        assert_eq!(
            store
                .storage_maintenance
                .deleted_files
                .load(Ordering::Relaxed),
            2,
            "进度要记下删了几个文件"
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM llm_calls").await,
            3,
            "清缓存不许动数据库里的调用记录"
        );
        assert_eq!(storage.call_count, 3);
    }

    /// 目录不存在（从没抓过东西）不是错误：点"清空缓存"不该在设置页上报失败。
    #[tokio::test]
    async fn clearing_the_web_cache_without_a_directory_is_not_an_error() {
        let (_directory, store) = temp_store().await;
        let cache = tempfile::tempdir().unwrap();
        let missing = cache.path().join("never-created");

        let storage = store.clear_web_cache_at(Some(missing)).await.unwrap();

        assert_eq!(
            store
                .storage_maintenance
                .deleted_files
                .load(Ordering::Relaxed),
            0
        );
        assert!(storage.web_cache_files >= 0);
    }

    /// 回归:"清理详细记录"不能删掉仍被会话根引用的内容块。
    /// 这条路径过去只保护 trace 工件与"有边的"内容块;活跃会话的根既没有边、也可能
    /// 只被即将被删的工件引用,于是被当成垃圾删掉,与存储卡片的承诺相反,而且客户端
    /// 手里没有那份内容时续聊会直接失败。
    #[tokio::test]
    async fn clearing_details_keeps_blobs_that_conversation_roots_reference() {
        let (_directory, store) = temp_store().await;
        let root = store.put_blob(b"conversation root", &[]).await.unwrap();
        let artifact = store.put_blob(b"trace chunk", &[]).await.unwrap();
        sqlx::query(
            "INSERT INTO conversation_blob_roots(conversation_id, blob_id) VALUES ('c1', ?)",
        )
        .bind(root.as_bytes().as_slice())
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cursor_run_traces(request_id, route, status, received_at_ms)
             VALUES ('r1', 'local_byok', 'completed', 1)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        for (seq, blob) in [(0_i64, &root), (1, &artifact)] {
            sqlx::query(
                "INSERT INTO cursor_run_trace_artifacts(
                    request_id, seq, artifact_type, source, blob_id, metadata_json, created_at_ms
                 ) VALUES ('r1', ?, 'run_sse_chunk', 'cursor_client', ?, '{}', 1)",
            )
            .bind(seq)
            .bind(blob.as_bytes().as_slice())
            .execute(store.pool())
            .await
            .unwrap();
        }
        age_past_the_observability_window(&store).await;

        store.clear_statistics_storage().await.unwrap();

        let remaining: Vec<Vec<u8>> = sqlx::query_scalar("SELECT blob_id FROM blobs")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert!(
            remaining.contains(&root.as_bytes().to_vec()),
            "会话根内容块不能被清理详细记录删掉"
        );
        assert!(
            !remaining.contains(&artifact.as_bytes().to_vec()),
            "只被 trace 工件引用的内容块应被清理"
        );
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .is_empty());
    }

    /// 回归:工具图片只被消息 payload 引用(没有边、也不是会话根),而读写它的 `blob_get` /
    /// `blob_set` 会把同一个内容块登记成追踪工件。清理详细记录过去只看会话根与边,于是
    /// 把图片当观测垃圾删掉,消息还指着它——续聊时 `hydrate_tool_images` 直接报
    /// "Read image Blob is missing"。
    #[tokio::test]
    async fn clearing_details_keeps_blobs_a_message_image_references() {
        let (_directory, store) = temp_store().await;
        let conversation = crate::model::ConversationId("image-conversation".into());
        store.ensure_conversation(&conversation).await.unwrap();
        let image = store.put_blob(b"tool screenshot", &[]).await.unwrap();
        let artifact = store.put_blob(b"trace chunk", &[]).await.unwrap();
        let message = crate::model::CanonicalMessage {
            message_id: "image-message".into(),
            role: crate::model::Role::Tool,
            origin: crate::model::Origin::Tool,
            content: crate::model::MessageContent::ToolResult(crate::model::ToolResultContent {
                call_id: "image-call".into(),
                name: "Read".into(),
                content: "Read image file: a.png".into(),
                is_error: false,
                image: Some(crate::model::ToolImageReference {
                    blob_id: image.to_base64(),
                    mime_type: "image/png".into(),
                    path: "a.png".into(),
                }),
                provider_parts: Vec::new(),
            }),
            runtime_event_id: None,
        };
        let mut transaction = store.pool().begin().await.unwrap();
        Store::put_message_tx(&mut transaction, &conversation, &message)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        link_artifact(&store, "image-trace", 0, &image).await;
        link_artifact(&store, "image-trace", 1, &artifact).await;
        age_past_the_observability_window(&store).await;

        store.clear_statistics_storage().await.unwrap();

        let remaining: Vec<Vec<u8>> = sqlx::query_scalar("SELECT blob_id FROM blobs")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert!(
            remaining.contains(&image.as_bytes().to_vec()),
            "被消息引用的图片内容块不能被清理详细记录删掉"
        );
        assert!(
            !remaining.contains(&artifact.as_bytes().to_vec()),
            "只被追踪工件引用的内容块照旧清掉"
        );
    }

    /// 观测窗口之内写进来的内容块一律不动:它可能刚被写进库、还没被 checkpoint 钉到
    /// 会话根上(同 `retention::UNROOTED_BLOB_MS` 的理由),"清理观测数据"不该把这种
    /// 中间态当成观测垃圾。
    #[tokio::test]
    async fn clearing_details_keeps_blobs_written_inside_the_observability_window() {
        let (_directory, store) = temp_store().await;
        let fresh = store.put_blob(b"just written", &[]).await.unwrap();
        let stale = store.put_blob(b"long gone", &[]).await.unwrap();
        link_artifact(&store, "window-trace", 0, &fresh).await;
        link_artifact(&store, "window-trace", 1, &stale).await;
        // 只把"早就写完的"那个拨回窗口之前,另一个保持刚写入。
        sqlx::query("UPDATE blobs SET created_at_ms = 1 WHERE blob_id = ?")
            .bind(stale.as_bytes().as_slice())
            .execute(store.pool())
            .await
            .unwrap();

        store.clear_statistics_storage().await.unwrap();

        let remaining: Vec<Vec<u8>> = sqlx::query_scalar("SELECT blob_id FROM blobs")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert!(
            remaining.contains(&fresh.as_bytes().to_vec()),
            "窗口内刚写进来的内容块不能被当成观测垃圾"
        );
        assert!(
            !remaining.contains(&stale.as_bytes().to_vec()),
            "窗口之前、又没人引用的内容块照旧清掉"
        );
    }

    /// 分批删到清空:一批删不完的明细表必须在多笔事务里删完,否则一条管理请求会把写锁
    /// 一直占到自己被 15 秒超时掐掉,而超时取消会让整笔(包括已经删掉的行)回滚。
    #[tokio::test]
    async fn clearing_details_removes_every_row_across_bounded_batches() {
        let (_directory, store) = temp_store().await;
        seed_call_details(&store, 2 * CLEAR_DETAILS_BATCH_ROWS + 1).await;

        store.clear_statistics_storage().await.unwrap();

        for table in ["llm_call_requests", "llm_call_response_chunks"] {
            assert_eq!(
                count(&store, &format!("SELECT COUNT(*) FROM {table}")).await,
                0,
                "{table} 必须被清空"
            );
        }
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM llm_calls").await,
            2 * CLEAR_DETAILS_BATCH_ROWS + 1,
            "清理详细记录不碰调用摘要"
        );
    }

    /// 每批一个事务:行数超过一批上限时,删除语句必须真的被拆成多笔(测试直接数批次数,
    /// 因为它就是"批间释放写锁"这条性质的观测量)。
    #[tokio::test]
    async fn the_detail_delete_commits_each_batch_as_its_own_transaction() {
        let (_directory, store) = temp_store().await;
        seed_call_details(&store, CLEAR_DETAILS_BATCH_ROWS + 1).await;
        let mut connection = store.pool().acquire().await.unwrap();

        let batches = store
            .delete_in_batches(
                &mut connection,
                "DELETE FROM llm_call_requests WHERE rowid IN (SELECT rowid FROM llm_call_requests LIMIT ?)",
            )
            .await
            .unwrap();

        assert_eq!(
            batches,
            2,
            "{} 行只能分两笔记账,一次事务删完会让写锁一直被别人等",
            CLEAR_DETAILS_BATCH_ROWS + 1
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM llm_call_requests").await,
            0
        );
    }

    /// 估算读的是已存的 `byte_count`,不是正文本身:请求正文只有 7 字节,已存列记着
    /// 800 万,估算必须跟着已存列走——它证明这条路径不再逐行扫正文。
    #[tokio::test]
    async fn statistics_estimate_reads_stored_byte_counts_instead_of_payload_bodies() {
        let (_directory, store) = temp_store().await;
        seed_call_details(&store, 1).await;
        sqlx::query("UPDATE llm_call_requests SET byte_count = 8000000")
            .execute(store.pool())
            .await
            .unwrap();

        let storage = store.statistics_storage().await.unwrap();

        assert!(
            storage.call_detail_bytes >= 8_000_000,
            "估算必须跟着已存的 byte_count 走: {}",
            storage.call_detail_bytes
        );
    }

    /// 追踪附件的正文同样读工件自己记下的 `byte_count`(迁移 0018):内容块只有 1 字节,
    /// 工件记着 600 万,估算必须跟着工件走——过去这里要回表 blobs 逐行
    /// `SUM(LENGTH(data))`。
    #[tokio::test]
    async fn statistics_estimate_reads_trace_artifact_byte_count_instead_of_blob_bodies() {
        let (_directory, store) = temp_store().await;
        let blob = store.put_blob(b"x", &[]).await.unwrap();
        link_artifact(&store, "estimate-trace", 0, &blob).await;
        sqlx::query("UPDATE cursor_run_trace_artifacts SET byte_count = 6000000")
            .execute(store.pool())
            .await
            .unwrap();

        let storage = store.statistics_storage().await.unwrap();

        assert!(
            storage.call_detail_bytes >= 6_000_000,
            "估算必须跟着工件记下的 byte_count 走: {}",
            storage.call_detail_bytes
        );
    }

    /// 写入侧必须把附件正文大小记下来:迁移只回填历史行,新数据得由写入路径自己填,
    /// 否则估算会在迁移之后随新数据越来越小。
    #[tokio::test]
    async fn appending_a_trace_artifact_records_the_body_size_it_accounts_for() {
        let (_directory, store) = temp_store().await;
        sqlx::query(
            "INSERT INTO cursor_run_traces(request_id, route, status, received_at_ms)
             VALUES ('write-trace', 'local_byok', 'running', 1)",
        )
        .execute(store.pool())
        .await
        .unwrap();

        store
            .append_cursor_trace_artifact(
                "write-trace",
                "run_sse_chunk",
                "cursor_client",
                &vec![7_u8; 4096],
                &serde_json::json!({}),
            )
            .await
            .unwrap();

        let byte_count: i64 = sqlx::query_scalar(
            "SELECT byte_count FROM cursor_run_trace_artifacts WHERE request_id = 'write-trace'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(byte_count, 4096);
        let storage = store.statistics_storage().await.unwrap();
        assert!(
            storage.call_detail_bytes >= 4096,
            "刚写的附件正文必须进估算: {}",
            storage.call_detail_bytes
        );
    }

    /// 接口必须在批次跑完之前就返回"已在清理"。
    ///
    /// 库大的时候几百个批次要跑几十秒，桌面端的管理请求 15 秒就超时：同步等它跑完的那个
    /// 请求先失败，用户看到的是"清理失败"，而库其实正在被清。
    #[tokio::test]
    async fn starting_a_clear_returns_before_the_batches_finish() {
        let (_directory, store) = temp_store().await;
        seed_call_details(&store, 9_000).await;
        assert!(
            count(&store, "SELECT COUNT(*) FROM llm_call_requests").await > 4_096,
            "要跨多批，这条用例才说明得了问题"
        );
        // 占住写锁：后台任务的第一批会停在这里，"立刻返回"于是可以确定地断言。
        let guard = store.writes.lock().await;

        let storage = store
            .start_clear_statistics_storage(StatisticsStorageScope::Details)
            .await
            .unwrap();

        let progress = storage.clearing.expect("响应必须说清清理已经在跑");
        assert_eq!(progress.deleted_rows, 0, "此刻一批都还没提交");
        assert!(
            count(&store, "SELECT COUNT(*) FROM llm_call_requests").await > 0,
            "行还在：接口没有等它删完"
        );
        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while store.clear_progress().is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("后台清理必须自己收尾");

        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM llm_call_requests").await,
            0,
            "交给后台之后还是要真的删掉"
        );
        assert!(
            store
                .storage_maintenance
                .deleted_rows
                .load(Ordering::Relaxed)
                > 0,
            "进度要真的走"
        );
        assert!(
            store.statistics_storage().await.unwrap().clearing.is_none(),
            "跑完之后不再有进度"
        );
    }

    /// 重复点按不会叠出第二条删除流水：任务还在跑时，第二次请求只是把"正在清理"再说一遍。
    #[tokio::test]
    async fn a_second_clear_request_does_not_start_a_second_pass() {
        let (_directory, store) = temp_store().await;
        seed_call_details(&store, 100).await;
        let guard = store.writes.lock().await;

        let first = store
            .start_clear_statistics_storage(StatisticsStorageScope::All)
            .await
            .unwrap();
        assert!(first.clearing.is_some());
        // 假装第一批已经提交：第二次请求必须沿用这份进度，而不是重置成一次新任务的样子。
        store
            .storage_maintenance
            .deleted_rows
            .store(7, Ordering::Relaxed);

        let second = store
            .start_clear_statistics_storage(StatisticsStorageScope::All)
            .await
            .unwrap();

        assert_eq!(
            second.clearing.expect("任务仍在跑").deleted_rows,
            7,
            "第二次请求不许重置进度（那就意味着起了第二条流水）"
        );
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while store.clear_progress().is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("后台清理必须自己收尾");
    }

    /// 1 GiB 文件的解释与真正缩小：已删除的行先进入 freelist，统计必须把有效页和空闲页
    /// 分开；用户请求压缩后，`VACUUM` 既归还文件，也把后续库切到增量回收。
    #[tokio::test]
    async fn compaction_reclaims_free_pages_and_enables_incremental_reclaim() {
        let (_directory, store) = temp_store().await;
        let mut transaction = store.pool().begin().await.unwrap();
        sqlx::query(
            "WITH RECURSIVE seq(n) AS (
                 SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < 2048
             )
             INSERT INTO blobs(blob_id, data, created_at_ms, last_used_at_ms)
             SELECT randomblob(32), randomblob(4096), 1, 1 FROM seq",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        transaction.commit().await.unwrap();
        sqlx::query("DELETE FROM blobs")
            .execute(store.pool())
            .await
            .unwrap();

        let before = store.statistics_storage().await.unwrap();
        assert_eq!(
            before.database_used_bytes + before.database_free_bytes,
            before.database_bytes,
            "有效页与空闲页必须解释完整个文件"
        );
        assert!(
            before.database_free_bytes > 0,
            "删除约 8 MiB 内容块后，SQLite 应留下可回收空闲页"
        );

        let requested = store.start_storage_compaction().await.unwrap();
        assert!(
            requested.compaction_requested,
            "接口要立刻告诉界面压缩已经排队"
        );
        assert!(store.compact_if_requested().await.unwrap());

        let after = store.statistics_storage().await.unwrap();
        assert!(
            after.database_bytes < before.database_bytes,
            "整库压缩后文件必须真的变小"
        );
        assert_eq!(after.database_free_bytes, 0, "压缩后不应继续背空闲页");
        assert!(
            after.incremental_vacuum_enabled,
            "压缩同时应让后续维护能小步归还空闲页"
        );
        assert!(
            after.wal_bytes <= 4096,
            "压缩结束后要把 VACUUM 撑大的 WAL 截断，实际还有 {} 字节",
            after.wal_bytes
        );
    }

    /// 存储卡片多报的那行数字只数两个落盘目录里的直接子文件：子目录不是工具结果或压缩
    /// 历史（写入路径从不建子目录），它的内容不该混进来；目录不存在（从没溢出过）时是 0，
    /// 而不是让整张存储卡片读失败。
    #[test]
    fn directory_usage_counts_only_direct_file_bytes() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("spill-a.txt"), b"aaaa").unwrap();
        std::fs::write(directory.path().join("spill-b.txt"), b"bbbbbbbb").unwrap();
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("ignored.txt"), vec![0_u8; 1_000]).unwrap();

        assert_eq!(
            directory_usage(directory.path()),
            12,
            "只数两个直接子文件的字节，子目录里的 1000 字节不计入"
        );
        assert_eq!(
            directory_usage(&directory.path().join("never-created")),
            0,
            "目录不存在时是 0：没溢出过不是错误"
        );
    }

    /// 两个目录用 `tool_result_spill` 的常量拼路径：改名时统计跟着走，不会悄悄数错地方。
    #[test]
    fn artifact_usage_sums_the_spill_and_compaction_directories() {
        let root = tempfile::tempdir().unwrap();
        let spill = root.path().join(crate::model::SPILL_DIR);
        let archive = root.path().join(crate::model::COMPACTION_ARCHIVE_DIR);
        std::fs::create_dir(&spill).unwrap();
        std::fs::create_dir(&archive).unwrap();
        std::fs::write(spill.join("result.txt"), b"12345").unwrap();
        std::fs::write(archive.join("run.jsonl"), b"1234567").unwrap();

        assert_eq!(artifact_usage_in(root.path()), 12);
        assert_eq!(
            artifact_usage_in(&root.path().join("missing-root")),
            0,
            "两个目录都不存在时合计是 0"
        );
    }
}
