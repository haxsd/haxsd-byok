//! Persists content-addressed blobs and their edges.
//! Storage accounting and cleanup for disposable observability data.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sqlx::{Connection, SqliteConnection};

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

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct StatisticsStorage {
    /// 可清理的观测数据（调用行、请求/响应正文、追踪附件）的估算大小。
    pub bytes: i64,
    /// 数据库文件的当前大小：清理不会让它变小（SQLite 复用释放页），所以两个数字要一起给。
    pub database_bytes: i64,
    pub call_count: i64,
    pub trace_count: i64,
    /// 服务端对话副本的闲置回收窗口（天）：见 `retention::CONVERSATION_RETENTION_MS`。
    ///
    /// 只读、不可配置：界面用它说明"这块空间不是无底洞"，而不是给一个可以调小的旋钮
    /// （调小会让旧会话更依赖客户端自己的副本，调大会让数据库重新膨胀）。
    pub conversation_retention_days: i64,
    /// 抓取结果缓存（`<数据目录>/cache/web`）的当前占用。
    ///
    /// 它不在数据库里，所以与 `bytes` 分开报：把文件系统的数字混进"可清理的观测数据"会让
    /// "清理之后数据库文件没变小"这件事更难解释。界面应当把它单列一行，并且只有
    /// `web_cache` 作用域会清它。
    pub web_cache_bytes: i64,
    pub web_cache_files: i64,
    /// 有后台清理在跑时是它此刻的进度：界面据此显示"正在清理"，而不是把"还没清完"
    /// 当成失败。没有任务在跑时是 `None`。
    pub clearing: Option<ClearProgress>,
}

/// 一次后台清理的进度快照。
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct ClearProgress {
    /// 这次清理已经删掉的行数（累计；不含外键级联带走的行）。
    pub deleted_rows: u64,
    /// 这次清理已经删掉的缓存文件数：只有 `web_cache` 作用域会动它，其它作用域一直是 0。
    pub deleted_files: u64,
}

/// 后台清理任务的进度。
///
/// 清理是进程级唯一的动作（一个库只有一条写路径），所以状态挂在 `Store` 上而不是每个
/// 请求里：界面在任务跑完之前会一直问 `statistics_storage`。
#[derive(Default)]
pub(crate) struct ClearState {
    running: AtomicBool,
    deleted_rows: AtomicU64,
    deleted_files: AtomicU64,
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
    /// 可清理观测数据的估算大小。
    ///
    /// 正文类的大块（请求体、响应分片、追踪附件正文）全部读各自已存的 `byte_count`：
    /// 这里是设置页一挂载就请求的接口，逐行 `LENGTH(body_json)` / `LENGTH(data)` /
    /// 回表 `blobs` 取附件正文会让它变成一次全库正文扫描。留下的 `LENGTH(...)` 只覆盖
    /// 两张摘要表自己的短文本列——它们就是"估算"里省不掉的那部分。
    pub async fn statistics_storage(&self) -> Result<StatisticsStorage> {
        let (bytes, database_bytes, call_count, trace_count) = sqlx::query_as::<_, (i64, i64, i64, i64)>(
            r#"
            SELECT
                COALESCE((
                    SELECT SUM(
                        LENGTH(call_id) + LENGTH(run_id) + LENGTH(conversation_id) +
                        LENGTH(provider_type) + LENGTH(provider_url) + LENGTH(request_type) +
                        LENGTH(request_url) + LENGTH(model_id) + LENGTH(display_name) +
                        LENGTH(status) + COALESCE(LENGTH(finish_reason), 0) +
                        COALESCE(LENGTH(usage_json), 0) + COALESCE(LENGTH(error_kind), 0) +
                        COALESCE(LENGTH(error_message), 0) + 256
                    ) FROM llm_calls
                ), 0) +
                COALESCE((SELECT SUM(byte_count + LENGTH(headers_json) + 24) FROM llm_call_requests), 0) +
                COALESCE((SELECT SUM(byte_count + 24) FROM llm_call_response_chunks), 0) +
                COALESCE((
                    SELECT SUM(
                        LENGTH(request_id) + COALESCE(LENGTH(conversation_id), 0) +
                        LENGTH(route) + COALESCE(LENGTH(model_id), 0) + LENGTH(status) +
                        COALESCE(LENGTH(error_message), 0) + 96
                    ) FROM cursor_run_traces
                ), 0) +
                COALESCE((
                    SELECT SUM(
                        byte_count + LENGTH(artifact_type) + LENGTH(source) +
                        LENGTH(metadata_json) + 48
                    ) FROM cursor_run_trace_artifacts
                ), 0),
                COALESCE((SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()), 0),
                (SELECT COUNT(*) FROM llm_calls),
                (SELECT COUNT(*) FROM cursor_run_traces)
            "#,
        )
        .fetch_one(&self.pool)
        .await?;

        // 抓取结果缓存的占用要读目录（几百个 stat），放进阻塞线程池：这是设置页一挂载
        // 就会问的接口，不该让它去占住 async 工作线程。
        let web_cache = tokio::task::spawn_blocking(crate::diagnostics::web_cache_usage)
            .await
            .unwrap_or_default();

        Ok(StatisticsStorage {
            bytes,
            database_bytes,
            call_count,
            trace_count,
            conversation_retention_days: super::retention::CONVERSATION_RETENTION_DAYS,
            web_cache_bytes: web_cache.bytes as i64,
            web_cache_files: web_cache.files as i64,
            clearing: self.clear_progress(),
        })
    }

    /// 此刻是否有后台清理在跑；有就带上进度。
    fn clear_progress(&self) -> Option<ClearProgress> {
        self.clear
            .running
            .load(Ordering::SeqCst)
            .then(|| ClearProgress {
                deleted_rows: self.clear.deleted_rows.load(Ordering::Relaxed),
                deleted_files: self.clear.deleted_files.load(Ordering::Relaxed),
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
            .clear
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::warn!("a statistics clear is already running; not starting a second one");
            return self.statistics_storage().await;
        }
        self.clear.deleted_rows.store(0, Ordering::Relaxed);
        self.clear.deleted_files.store(0, Ordering::Relaxed);
        let store = self.clone();
        tokio::spawn(async move {
            let cleared = match scope {
                StatisticsStorageScope::Details => store.clear_statistics_storage().await,
                StatisticsStorageScope::All => store.clear_all_statistics_storage().await,
                StatisticsStorageScope::WebCache => store.clear_web_cache_at(None).await,
            };
            store.clear.running.store(false, Ordering::SeqCst);
            match cleared {
                Ok(storage) => tracing::info!(
                    deleted_rows = store.clear.deleted_rows.load(Ordering::Relaxed),
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

    pub async fn clear_statistics_storage(&self) -> Result<StatisticsStorage> {
        let mut connection = self.pool.acquire().await?;
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
        self.clear
            .deleted_files
            .fetch_add(cleared.files, Ordering::Relaxed);
        self.statistics_storage().await
    }

    pub async fn clear_all_statistics_storage(&self) -> Result<StatisticsStorage> {
        let mut connection = self.pool.acquire().await?;
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
            self.clear
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
            self.clear
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
            store.clear.deleted_files.load(Ordering::Relaxed),
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

        assert_eq!(store.clear.deleted_files.load(Ordering::Relaxed), 0);
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
            storage.bytes >= 8_000_000,
            "估算必须跟着已存的 byte_count 走: {}",
            storage.bytes
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
            storage.bytes >= 6_000_000,
            "估算必须跟着工件记下的 byte_count 走: {}",
            storage.bytes
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
            storage.bytes >= 4096,
            "刚写的附件正文必须进估算: {}",
            storage.bytes
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
            store.clear.deleted_rows.load(Ordering::Relaxed) > 0,
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
        store.clear.deleted_rows.store(7, Ordering::Relaxed);

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
}
