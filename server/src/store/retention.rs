//! Expires inactive runtime state while preserving lifetime usage statistics.
use super::{BlobId, Store};
use crate::{Error, Result};
use std::collections::HashSet;

/// 观测明细（调用请求/响应正文、追踪附件与轨迹摘要）的回收窗口。
pub(crate) const RETENTION_MS: i64 = 3 * 24 * 60 * 60 * 1000;

/// 对话数据（会话、消息、checkpoint、运行、工具轮、blob 根及其 blob 图）闲置多久后整会话回收。
///
/// 为什么可以收得这么紧：**客户端才是会话状态的持久副本**。旧会话被继续使用时，服务端
/// 按客户端带来的 checkpoint 重新水合（`BlobSynchronizer::get` 先查本地、再向客户端要），
/// 重新导入的消息经同一套投影得到**逐字节相同**的前缀，前缀缓存不失效
/// （见 `.agents/skills/cursor-prefix-stability/SKILL.md` 与
/// `server/tests/retention_resume.rs`）。客户端自己也拿不出内容时是明确报错
/// （`missing message Blob`），不会静默缺段。
///
/// 取值理由（2026-10-16 实测，用户真实库 1.19 GB）：
/// * 对话数据是整库绝对大头——blobs 379 MB、messages 248 MB、tool_round* 130 MB，
///   按使用强度约 330 MB/天；窗口每放宽一天，稳态体积就多背一天的量。
/// * 会话年龄实测：115 个会话里 36 个在 24 小时内、79 个在 1–3 天、没有一个超过 3 天。
///   也就是说 24 小时窗能把"昨天用过的"也交给客户端保管，服务端稳态只留最近一天。
/// * 真实环境 17 天日志里没有一次因回收导致的水合失败（无 `missing message Blob`、
///   无 `stable root changed`）。
///
/// 代价说清：闲置超过一天再继续的对话，第一轮要多几次向客户端取内容块的往返；如果
/// 客户端也没有那份内容（清过历史、换机器），那一轮会明确报错。要放宽只改这一个常量——
/// 只读字段 `StatisticsStorage::conversation_retention_days` 与桌面「存储管理」卡片
/// 显示的都是它。
pub(crate) const CONVERSATION_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;
/// [`CONVERSATION_RETENTION_MS`] 的天数，供只读统计接口展示。
pub(crate) const CONVERSATION_RETENTION_DAYS: i64 =
    CONVERSATION_RETENTION_MS / (24 * 60 * 60 * 1000);
/// 每轮最多回收多少个过期会话。
///
/// 一次性把几个月没打开的会话全删掉，会像不加限量的 blob 回收一样长时间占住写锁，
/// 让代理启动和模型调用的落库排队。按最老优先、每轮限量，积压能在几次维护窗口里排空。
///
/// 实测（`measure_conversation_recycle_at_scale`，30 万条消息 + 2 万次运行、debug）：
/// 空趟 3 ms；每趟回收 16 个会话（约 9600 条消息 + 640 次运行 + 640 个工具轮）约 850 ms，
/// 与既有的轨迹明细回收（30 万行过期行约 420 ms）同一量级。维护循环 60 秒一次，
/// 这份开销由所有写入方分摊；一次长假期的积压（几百个会话）在一小时量级内排空。
const MAX_PRUNED_CONVERSATIONS_PER_PASS: i64 = 16;

/// 既没有会话根、也没有追踪引用它的 blob 能活多久。
///
/// 每次 checkpoint 都会把当前 turn 连同它含有的全部 step 重新写一份 blob：新版
/// 本一落库，上一版就只剩"刚被写过"这一个身份。这类 blob 只留到窗口结束，避免
/// 一次长会话里成千上万个被取代的版本长期占着边表。
///
/// 窗口之后删除不会丢会话：客户端才是会话状态的持久副本，本地缺失时
/// `BlobSynchronizer::get` 会按 BlobID 向 Cursor 客户端要；窗口本身负责兜住
/// 写完之后、发布之前崩溃的那种中间态。
const UNROOTED_BLOB_MS: i64 = 60 * 60 * 1000;
const MAX_PRUNED_BLOBS_PER_PASS: i64 = 512;

/// 轨迹摘要在明细被回收之后再保留多少条。
///
/// 调用列表一次最多要 200 条、服务端上限 500 条，所以比"最近 1000 条"更老的轨迹
/// 摘要不可能出现在任何界面列表里；客户端点进某条调用时按 `run_id` 找轨迹，因此
/// 最近调用引用到的轨迹也要留着。这两条保护线让删除的内容对用户不可见：能删的只剩
/// "载荷已被回收、列表也够不着"的计数器行。
const RETAINED_TRACE_ROWS: i64 = 1_000;
/// 每轮回收最多删多少行轨迹摘要。
///
/// 摘要行只有几百字节，理由和 blob 的上限（`MAX_PRUNED_BLOBS_PER_PASS`）不同：这里
/// 的限量更像"别让一次清理吃掉整个维护窗口"。实测（30 万行、debug）这一条语句删
/// 1024 行约 5–12 ms，删 8192 行约 90 ms；同一趟里的既有明细回收语句在 30 万行过期
/// 轨迹上要 420 ms，所以上限取 8192——积压能在一小时量级的运行时间里排空，而多出来
/// 的开销远小于既有的那一段。
const MAX_PRUNED_TRACES_PER_PASS: i64 = 8_192;

impl Store {
    pub(crate) async fn retain_conversation_blobs(
        &self,
        conversation: &str,
        roots: &[BlobId],
    ) -> Result<()> {
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM conversation_blob_roots WHERE conversation_id = ?")
            .bind(conversation)
            .execute(&mut *tx)
            .await?;
        for id in roots {
            sqlx::query("INSERT OR IGNORE INTO conversation_blob_roots(conversation_id, blob_id) VALUES (?, ?)")
                .bind(conversation).bind(id.as_bytes().as_slice()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 清理落盘的大工具结果与压缩归档。
    ///
    /// 判定依据是**引用是否还在**：`tool_round_calls.call_id` 与 `conversations`
    /// 都随会话回收消失，所以"消息还在、文件也还在"；写入路径不再扫目录。文件系统
    /// 操作放在阻塞线程池里，数万行的引用集合也只读两张表各一次。
    pub async fn prune_artifact_files(&self) -> Result<usize> {
        let calls: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT call_id FROM tool_round_calls")
                .fetch_all(&self.pool)
                .await?;
        let conversations: Vec<String> =
            sqlx::query_scalar("SELECT conversation_id FROM conversations")
                .fetch_all(&self.pool)
                .await?;
        let live_calls: HashSet<String> = calls.into_iter().collect();
        let live_conversations: HashSet<String> = conversations.into_iter().collect();
        let removed = tokio::task::spawn_blocking(move || {
            crate::model::prune_artifact_files(&live_calls, &live_conversations)
        })
        .await
        .map_err(|error| Error::Store(format!("artifact prune task failed: {error}")))?;
        Ok(removed)
    }

    /// Caller must exclude live transports, including requests still being compiled.
    ///
    /// `pub` 是因为集成测试要驱动一轮完整的回收（`server/tests/retention_resume.rs`）；
    /// 正常路径的唯一调用方是 `TransportRegistry::maintain_storage_if_idle`。
    pub async fn prune_inactive_storage(&self, now: i64) -> Result<()> {
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let cutoff = now - RETENTION_MS;
        let conversation_cutoff = now - CONVERSATION_RETENTION_MS;
        // Ancient runs left after a crash must not permanently prevent collection.
        let running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM runs WHERE status = 'running' AND updated_at_ms >= ?",
        )
        .bind(conversation_cutoff)
        .fetch_one(&mut *tx)
        .await?;
        if running > 0 {
            return Ok(());
        }
        // 过期的整会话：最老优先、每轮限量（见 `MAX_PRUNED_CONVERSATIONS_PER_PASS`），
        // `active_run_id` 还指着的会话一律不动——它可能正有一轮在跑。
        sqlx::query("CREATE TEMP TABLE expired_conversations AS SELECT conversation_id FROM conversations WHERE updated_at_ms < ? AND active_run_id IS NULL ORDER BY updated_at_ms ASC LIMIT ?")
            .bind(conversation_cutoff)
            .bind(MAX_PRUNED_CONVERSATIONS_PER_PASS)
            .execute(&mut *tx)
            .await?;
        let expired: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM expired_conversations")
            .fetch_one(&mut *tx)
            .await?;
        // Child records must go first; checkpoints refer to their parents.
        for statement in [
            "DELETE FROM conversation_blob_roots WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)",
            "DELETE FROM message_image_blobs WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)",
            "DELETE FROM tool_round_calls WHERE round_id IN (SELECT round_id FROM tool_rounds WHERE run_id IN (SELECT run_id FROM runs WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)))",
            "DELETE FROM tool_rounds WHERE run_id IN (SELECT run_id FROM runs WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations))",
            "DELETE FROM runs WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)",
            "DELETE FROM input_anchors WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)",
            "DELETE FROM checkpoint_messages WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)",
            "UPDATE conversation_checkpoints SET parent_checkpoint_id = NULL WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)",
            "DELETE FROM conversation_checkpoints WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)",
            "DELETE FROM messages WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)",
            "DELETE FROM conversations WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)",
            "DROP TABLE expired_conversations",
        ] {
            sqlx::query(statement).execute(&mut *tx).await?;
        }
        // 会话被回收后，它的压缩记录既没有界面入口（调用详情按会话/run 找），归档
        // 文件也失去了引用。runs 只随会话一起删，所以按会话判断就覆盖了 run 孤儿行。
        // 先把归档路径收出来，事务提交之后再删文件——事务里做文件 IO 会拉长写锁。
        let orphaned_archives: Vec<String> = sqlx::query_scalar(
            "SELECT archive_path FROM compaction_events
             WHERE archive_path IS NOT NULL
               AND conversation_id NOT IN (SELECT conversation_id FROM conversations)",
        )
        .fetch_all(&mut *tx)
        .await?;
        let pruned_compactions = sqlx::query(
            "DELETE FROM compaction_events
             WHERE conversation_id NOT IN (SELECT conversation_id FROM conversations)",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        for statement in [
            "DELETE FROM llm_call_requests WHERE call_id IN (SELECT call_id FROM llm_calls WHERE created_at_ms < ? AND status != 'running')",
            "DELETE FROM llm_call_response_chunks WHERE call_id IN (SELECT call_id FROM llm_calls WHERE created_at_ms < ? AND status != 'running')",
            "DELETE FROM cursor_run_trace_artifacts WHERE request_id IN (SELECT request_id FROM cursor_run_traces WHERE received_at_ms < ? AND status != 'running')",
        ] {
            sqlx::query(statement).bind(cutoff).execute(&mut *tx).await?;
        }
        // 载荷被回收之后，轨迹摘要行只剩计数器，而调用列表只覆盖最近 200 条：这些
        // 行按最老优先、每轮限量删除，把表压回 `RETAINED_TRACE_ROWS` 条以内。
        let pruned_traces = Self::prune_expired_trace_summaries(&mut tx, cutoff).await?;
        let mut references = 0_u64;
        let mut orphans = 0_u64;
        // 到期候选：过了无根窗口、既不是会话根、也没有被追踪工件引用的 blob。只有这些
        // 可能被回收，所以一个候选都没有时，下面整段"重建 blob 闭包 + 扫 blobs"的固定
        // 成本可以整段跳过——这段成本与库规模挂钩，而这一趟是持写锁跑的。
        let candidates = Self::has_expired_blob_candidates(&mut tx, now - UNROOTED_BLOB_MS).await?;
        let mut retained = 0_i64;
        if candidates {
            sqlx::query("CREATE TEMP TABLE retained_blobs(blob_id BLOB PRIMARY KEY)")
                .execute(&mut *tx)
                .await?;
            // 只有会话根与追踪能长期钉住 blob；其余按"最近写过或读过"存活，见
            // `UNROOTED_BLOB_MS`。
            sqlx::query("INSERT OR IGNORE INTO retained_blobs SELECT blob_id FROM blobs WHERE last_used_at_ms >= ? UNION SELECT blob_id FROM cursor_run_trace_artifacts UNION SELECT blob_id FROM conversation_blob_roots")
                .bind(now - UNROOTED_BLOB_MS).execute(&mut *tx).await?;
            // 工具图片可能在保留期之前就写进来了，所以只看"最近用过"会误删仍在被消息引用的
            // 内容块；引用在写消息时就登记在 `message_image_blobs` 里（见 `store::messages`），
            // 不用每趟扫整张 messages 的 payload。
            let images: Vec<String> = sqlx::query_scalar("SELECT blob_id FROM message_image_blobs")
                .fetch_all(&mut *tx)
                .await?;
            for image in images {
                let id = BlobId::from_base64(&image)?;
                sqlx::query("INSERT OR IGNORE INTO retained_blobs VALUES (?)")
                    .bind(id.as_bytes().as_slice())
                    .execute(&mut *tx)
                    .await?;
            }
            // UNION, not UNION ALL: shared descendants and cycles are visited once.
            sqlx::query("WITH RECURSIVE reachable(blob_id) AS (SELECT blob_id FROM retained_blobs UNION SELECT e.child_blob_id FROM blob_edges e JOIN reachable r ON e.parent_blob_id = r.blob_id) INSERT OR IGNORE INTO retained_blobs SELECT blob_id FROM reachable")
                .execute(&mut *tx).await?;
            retained = sqlx::query_scalar("SELECT COUNT(*) FROM retained_blobs")
                .fetch_one(&mut *tx)
                .await?;
            // Bound the write transaction. Deleting millions of edges at once prevents
            // proxy startup and model calls from saving anything until collection ends.
            sqlx::query("CREATE TEMP TABLE expired_blobs(blob_id BLOB PRIMARY KEY)")
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO expired_blobs SELECT blob_id FROM blobs WHERE blob_id NOT IN (SELECT blob_id FROM retained_blobs) LIMIT ?")
                .bind(MAX_PRUNED_BLOBS_PER_PASS).execute(&mut *tx).await?;
            // Both endpoints have indexes, so this visits the selected batch's edges
            // instead of scanning the entire edge table on every pass.
            references = sqlx::query("DELETE FROM blob_edges WHERE parent_blob_id IN (SELECT blob_id FROM expired_blobs) OR child_blob_id IN (SELECT blob_id FROM expired_blobs)").execute(&mut *tx).await?.rows_affected();
            orphans = sqlx::query(
                "DELETE FROM blobs WHERE blob_id IN (SELECT blob_id FROM expired_blobs)",
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
            sqlx::query("DROP TABLE expired_blobs")
                .execute(&mut *tx)
                .await?;
            sqlx::query("DROP TABLE retained_blobs")
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        // 压缩归档是文件系统上的旁路：行删掉之后再删文件，失败只留 warn（下一趟
        // `prune_artifact_files` 也会按失去会话引用的规则收走它们）。删文件不占写锁。
        if !orphaned_archives.is_empty() {
            drop(_write);
            let removed = tokio::task::spawn_blocking(move || {
                crate::model::remove_compaction_archives(&orphaned_archives)
            })
            .await
            .unwrap_or_default();
            tracing::info!(
                deleted_compaction_archives = removed,
                "storage retention removed orphan compaction archives"
            );
        }
        // Freed pages are reused by later writes. Automatic VACUUM would rewrite
        // the whole database while holding the same lock that blocked startup.
        //
        // 这一段每 60 秒跑一次：什么都没收走时记 debug。否则一份正常日志会被
        // "retention pass completed" 填满，用户真正要找的那几行反而找不到。
        if orphans > 0
            || references > 0
            || pruned_traces > 0
            || expired > 0
            || pruned_compactions > 0
        {
            tracing::info!(
                retained_blobs = retained,
                deleted_blobs = orphans,
                deleted_references = references,
                deleted_traces = pruned_traces,
                deleted_compactions = pruned_compactions,
                recycled_conversations = expired,
                "storage retention pass completed"
            );
        } else if !candidates {
            tracing::debug!("storage retention pass found no expired blob candidates");
        } else {
            tracing::debug!(
                retained_blobs = retained,
                "storage retention pass found nothing to collect"
            );
        }
        Ok(())
    }

    /// 有没有"可能被回收"的内容块：过了无根窗口（`UNROOTED_BLOB_MS`），既不是会话根，
    /// 也没有被追踪工件引用。判据比真正的回收更宽松（不看 blob 闭包），所以它只能是
    /// 必要条件：为假时这一趟确定没有任何 blob 会被回收，整段开销可以跳过。
    async fn has_expired_blob_candidates(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        unrooted_cutoff: i64,
    ) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(
                 SELECT 1 FROM blobs b
                 WHERE b.last_used_at_ms < ?
                   AND NOT EXISTS (
                       SELECT 1 FROM conversation_blob_roots r WHERE r.blob_id = b.blob_id
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM cursor_run_trace_artifacts a WHERE a.blob_id = b.blob_id
                   )
             )",
        )
        .bind(unrooted_cutoff)
        .fetch_one(&mut **tx)
        .await?)
    }

    /// 已回收载荷的轨迹摘要行：`cursor_run_traces` 按设计永不删除，长期使用后没人
    /// 给它设上限。调用列表一次最多要 200 条（服务端上限 500），所以只删"早于回收
    /// 窗口、且不在最近 `RETAINED_TRACE_ROWS` 条里"的行——它们不可能出现在任何界面
    /// 列表里；正在跑的轨迹、最近调用按 `run_id` 指向的轨迹一律留下。
    ///
    /// `NOT EXISTS` 明细那一句是保险：外键是级联的，带走摘要就等于带走载荷，所以这条
    /// 语句永远不许碰还有明细的行。
    async fn prune_expired_trace_summaries(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        cutoff: i64,
    ) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM cursor_run_traces
             WHERE request_id IN (
                 SELECT t.request_id
                 FROM cursor_run_traces t
                 WHERE t.status != 'running'
                   AND t.received_at_ms < ?
                   AND NOT EXISTS (
                       SELECT 1 FROM cursor_run_trace_artifacts a WHERE a.request_id = t.request_id
                   )
                   AND t.request_id NOT IN (
                       SELECT request_id FROM cursor_run_traces ORDER BY received_at_ms DESC LIMIT ?
                   )
                   AND t.request_id NOT IN (
                       SELECT run_id FROM llm_calls ORDER BY created_at_ms DESC LIMIT ?
                   )
                 ORDER BY t.received_at_ms ASC
                 LIMIT ?
             )",
        )
        .bind(cutoff)
        .bind(RETAINED_TRACE_ROWS)
        .bind(RETAINED_TRACE_ROWS)
        .bind(MAX_PRUNED_TRACES_PER_PASS)
        .execute(&mut **tx)
        .await?
        .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::BlobEdge;

    async fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            dir.path().join("test.db").display()
        ))
        .await
        .unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn cleanup_limits_each_write_transaction_and_can_resume() {
        let (_dir, store) = fixture().await;
        let mut tx = store.pool().begin().await.unwrap();
        for n in 0..600_u32 {
            let id = n.to_le_bytes().repeat(8);
            sqlx::query("INSERT INTO blobs(blob_id, data, created_at_ms, last_used_at_ms) VALUES (?, X'01', 1, 1)")
                .bind(&id).execute(&mut *tx).await.unwrap();
            if n > 0 {
                sqlx::query("INSERT INTO blob_edges(parent_blob_id, child_blob_id, field_name) VALUES (?, ?, 'next')")
                    .bind((n - 1).to_le_bytes().repeat(8)).bind(&id).execute(&mut *tx).await.unwrap();
            }
        }
        tx.commit().await.unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            remaining, 88,
            "only 512 expired blobs may be removed per pass"
        );
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .is_empty());
        store.set_proxy_port(9061).await.unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(remaining, 0);
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn blob_deletion_does_not_scan_every_trace_reference() {
        let (_dir, store) = fixture().await;
        let index: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'cursor_run_trace_artifacts_blob'",
        )
        .fetch_optional(store.pool())
        .await
        .unwrap();
        assert_eq!(
            index.as_deref(),
            Some("cursor_run_trace_artifacts_blob"),
            "trace artifact foreign key must be backed by an index"
        );
    }

    #[tokio::test]
    async fn statistics_survive_while_old_request_details_expire() {
        let (_dir, store) = fixture().await;
        for (id, time) in [("old", 1), ("recent", crate::store::now_ms())] {
            sqlx::query("INSERT INTO llm_calls(call_id,run_id,conversation_id,provider_call_index,provider_type,provider_url,request_type,request_url,model_id,display_name,status,created_at_ms,message_count,tool_count,detailed,input_tokens,output_tokens,cache_read_tokens) VALUES (?, 'run', 'conv', 0, 'openai', 'url', 'chat', 'url', 'model', 'name', 'completed', ?, 1, 0, 1, 123, 45, 100)")
                .bind(id).bind(time).execute(store.pool()).await.unwrap();
            sqlx::query("INSERT INTO llm_call_requests VALUES (?, '{}', '{}', 2)")
                .bind(id)
                .execute(store.pool())
                .await
                .unwrap();
            sqlx::query("INSERT INTO llm_call_response_chunks VALUES (?, 0, 0, X'01', 1)")
                .bind(id)
                .execute(store.pool())
                .await
                .unwrap();
        }
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        let stats: (i64,i64,i64,i64) = sqlx::query_as("SELECT COUNT(*),SUM(input_tokens),SUM(output_tokens),SUM(cache_read_tokens) FROM llm_calls").fetch_one(store.pool()).await.unwrap();
        assert_eq!(stats, (2, 246, 90, 200));
        for table in ["llm_call_requests", "llm_call_response_chunks"] {
            let ids: Vec<String> = sqlx::query_scalar(&format!("SELECT call_id FROM {table}"))
                .fetch_all(store.pool())
                .await
                .unwrap();
            assert_eq!(ids, vec!["recent"]);
        }
    }

    /// 一次 checkpoint 会给当前 turn 落一份新版本，并把上一版变成无人引用的中间态。
    /// 这类版本只活在窗口内；窗口过后连同它的边一起回收，留下的引用数只与当前
    /// 会话状态的大小有关，不再随会话长度平方增长。
    #[tokio::test]
    async fn superseded_checkpoint_versions_are_collected_after_the_unrooted_window() {
        let (_dir, store) = fixture().await;
        let conv = crate::model::ConversationId("superseded".into());
        store.ensure_conversation(&conv).await.unwrap();
        let step = store.put_blob(b"step", &[]).await.unwrap();
        let edge = BlobEdge {
            child: step.clone(),
            field_name: "agent_conversation_turn.steps[0]".into(),
        };
        let previous = store
            .put_blob(b"turn version 1", std::slice::from_ref(&edge))
            .await
            .unwrap();
        store
            .retain_conversation_blobs("superseded", std::slice::from_ref(&previous))
            .await
            .unwrap();
        let current = store
            .put_blob(b"turn version 2", std::slice::from_ref(&edge))
            .await
            .unwrap();
        store
            .retain_conversation_blobs("superseded", std::slice::from_ref(&current))
            .await
            .unwrap();

        // 两个版本都刚写过：窗口内都留着，客户端还没吃下最后一次 checkpoint 时能兜住。
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        assert!(store.get_blob(&previous).await.unwrap().is_some());

        // 窗口过后，被取代的版本离开，当前版本与它引用的 step 都还在。
        sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE blobs SET last_used_at_ms = ? WHERE blob_id = ?")
            .bind(crate::store::now_ms())
            .bind(current.as_bytes().as_slice())
            .execute(store.pool())
            .await
            .unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        assert!(store.get_blob(&previous).await.unwrap().is_none());
        assert!(store.get_blob(&current).await.unwrap().is_some());
        assert!(store.get_blob(&step).await.unwrap().is_some());
        let references: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blob_edges")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(references, 1);
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert!(violations.is_empty());
    }

    /// 到期候选的判据：过了无根窗口、既不是会话根、也没有被追踪工件引用的内容块。
    /// 它决定这一趟要不要重建整张 blob 闭包——为假时那段固定成本整段跳过。
    #[tokio::test]
    async fn only_stale_unpinned_blobs_are_expiry_candidates() {
        let (_dir, store) = fixture().await;
        let now = crate::store::now_ms();
        let cutoff = now - UNROOTED_BLOB_MS;
        let unpinned = store.put_blob(b"unpinned", &[]).await.unwrap();
        let pinned = store.put_blob(b"pinned", &[]).await.unwrap();
        store
            .retain_conversation_blobs("candidate-conversation", std::slice::from_ref(&pinned))
            .await
            .unwrap();

        let mut tx = store.pool().begin().await.unwrap();
        let fresh = Store::has_expired_blob_candidates(&mut tx, cutoff)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert!(!fresh, "窗口内的内容块不构成候选");

        sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        let mut tx = store.pool().begin().await.unwrap();
        let stale = Store::has_expired_blob_candidates(&mut tx, cutoff)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert!(stale, "没被任何东西钉住的过期内容块是候选");

        // 被追踪工件引用同样不算候选（外键要求先有追踪摘要行）。
        sqlx::query(
            "INSERT INTO cursor_run_traces(request_id, route, status, received_at_ms)
             VALUES ('candidate-trace', 'local_byok', 'completed', 1)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cursor_run_trace_artifacts(
                request_id, seq, artifact_type, source, blob_id, metadata_json, created_at_ms
             ) VALUES ('candidate-trace', 0, 'run_sse_chunk', 'cursor_client', ?, '{}', 1)",
        )
        .bind(unpinned.as_bytes().as_slice())
        .execute(store.pool())
        .await
        .unwrap();
        let mut tx = store.pool().begin().await.unwrap();
        let pinned_by_trace = Store::has_expired_blob_candidates(&mut tx, cutoff)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert!(!pinned_by_trace, "被追踪工件引用的过期内容块不构成候选");
    }

    /// 工具图片只被消息的 payload 引用：没有边，也不是会话根。写消息时登记的那条引用
    /// 必须真的挡住回收趟，否则图片在无根窗口一过就被当成垃圾删掉，而消息还指着它
    /// （`run::engine::hydrate_tool_images` 会以 "Read image Blob is missing" 失败）。
    /// 会话自己过期被回收时，登记连同消息一起走：留下失效的登记会让内容块再也收不走。
    #[tokio::test]
    async fn an_image_referenced_by_a_message_survives_until_its_conversation_is_recycled() {
        let (_dir, store) = fixture().await;
        let conversation = crate::model::ConversationId("image-conversation".into());
        store.ensure_conversation(&conversation).await.unwrap();
        let image = store.put_blob(b"tool screenshot", &[]).await.unwrap();
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
        store
            .import_checkpoint(&conversation, std::slice::from_ref(&message))
            .await
            .unwrap();
        let marked: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM message_image_blobs WHERE blob_id = ?")
                .bind(image.to_base64())
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(marked, 1, "写消息时必须登记图片引用");

        // 图片内容块过了无根窗口，会话本身还在窗口内（不整会话回收）。
        sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();

        // 存在性检查都走 SQL：`get_blob` 会顺手续期，把内容块拉回无根窗口里，
        // 那样下面的"没人引用就会被收走"就证明不了了。
        let survivors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE blob_id = ?")
            .bind(image.as_bytes().as_slice())
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(survivors, 1, "被消息引用的图片内容块不能被回收");

        // 会话自己过期之后，登记随会话一起删，内容块也就能收了。
        sqlx::query("UPDATE conversations SET updated_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        let marks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_image_blobs")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(marks, 0, "会话回收要带走它的图片引用登记");
        let blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE blob_id = ?")
            .bind(image.as_bytes().as_slice())
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(blobs, 0, "会话走了之后图片内容块不再被引用");
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .is_empty());
    }

    /// 图片引用只在写消息时登记，回收趟不再读 messages 的 payload。
    ///
    /// 这条消息绕开 `put_message_tx` 直接写库（只有测试会这么干），payload 里的图片引用
    /// 故意不是合法 base64：回收趟要是还去 `json_extract` + base64 解码，就会以解码错误
    /// 整趟失败（过去的行为）；现在它只看登记表，过期内容块照常收走。
    #[tokio::test]
    async fn the_pass_never_parses_message_payloads_for_image_pins() {
        let (_dir, store) = fixture().await;
        sqlx::query("INSERT INTO conversations(conversation_id, updated_at_ms) VALUES ('raw-conversation', ?)")
            .bind(crate::store::now_ms())
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms)
             VALUES ('raw-conversation', 'raw-message', 'tool', 'tool',
                     '{\"content\":{\"kind\":\"tool_result\",\"image\":{\"blob_id\":\"not-base64!!\"}}}', 1)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        let stale = store.put_blob(b"garbage candidate", &[]).await.unwrap();
        sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();

        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();

        assert!(
            store.get_blob(&stale).await.unwrap().is_none(),
            "过期且无人引用的内容块照常回收"
        );
    }

    #[tokio::test]
    async fn retained_conversation_pins_old_content_until_conversation_expires() {
        let (_dir, store) = fixture().await;
        let conv = crate::model::ConversationId("retained".into());
        store.ensure_conversation(&conv).await.unwrap();
        let id = store.put_blob(b"pinned", &[]).await.unwrap();
        store
            .retain_conversation_blobs("retained", std::slice::from_ref(&id))
            .await
            .unwrap();
        sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(count, 1);
        sqlx::query("UPDATE conversations SET updated_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        assert!(store.get_blob(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn recent_running_run_defers_cleanup_but_crashed_expired_run_does_not() {
        let (_dir, store) = fixture().await;
        let conv = crate::model::ConversationId("running".into());
        let checkpoint = store.ensure_conversation(&conv).await.unwrap();
        sqlx::query("INSERT INTO runs(run_id,conversation_id,base_checkpoint_id,head_checkpoint_id,run_kind,status,created_at_ms,updated_at_ms) VALUES ('run','running',?,?,'root','running',1,?)")
            .bind(checkpoint.0).bind(checkpoint.0).bind(crate::store::now_ms()).execute(store.pool()).await.unwrap();
        let id = store.put_blob(b"orphan", &[]).await.unwrap();
        sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(count, 1);
        sqlx::query("UPDATE runs SET updated_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE conversations SET updated_at_ms = 1, active_run_id = 'run'")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE conversations SET active_run_id = NULL")
            .execute(store.pool())
            .await
            .unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        assert!(store.get_blob(&id).await.unwrap().is_none());
        assert!(store.conversation(&conv).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn collection_reuses_free_pages_without_rewriting_the_database() {
        let (_dir, store) = fixture().await;
        sqlx::query("INSERT INTO blobs(blob_id,data,created_at_ms,last_used_at_ms) VALUES (randomblob(32),zeroblob(70000000),1,1)").execute(store.pool()).await.unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        let free: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(free * 4096 > 64 * 1024 * 1024);
        let pages_before: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO blobs(blob_id,data,created_at_ms,last_used_at_ms) VALUES (randomblob(32),zeroblob(70000000),1,1)").execute(store.pool()).await.unwrap();
        let pages_after: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(
            pages_after <= pages_before + 2,
            "subsequent writes must reuse freed pages"
        );
    }

    #[tokio::test]
    async fn retention_preserves_recent_graph_and_expires_idle_conversations() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let old = store.put_blob(b"old orphan", &[]).await.unwrap();
        let child = store.put_blob(b"old referenced", &[]).await.unwrap();
        sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        let parent = store
            .put_blob(
                b"new parent",
                &[BlobEdge {
                    child: child.clone(),
                    field_name: "child".into(),
                }],
            )
            .await
            .unwrap();
        let old_conversation = crate::model::ConversationId("expired".into());
        store.ensure_conversation(&old_conversation).await.unwrap();
        sqlx::query("UPDATE conversations SET updated_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        assert!(store.get_blob(&old).await.unwrap().is_none());
        assert!(store.get_blob(&parent).await.unwrap().is_some());
        assert!(store.get_blob(&child).await.unwrap().is_some());
        assert!(store
            .conversation(&old_conversation)
            .await
            .unwrap()
            .is_none());
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert!(violations.is_empty());
        // Collection is repeatable; temporary tables cannot leak between runs.
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reading_old_blob_renews_its_retention() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let id = store.put_blob(b"reused", &[]).await.unwrap();
        sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(store.get_blob(&id).await.unwrap().is_some());
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        assert!(store.get_blob(&id).await.unwrap().is_some());
    }

    /// 终态、明细已被回收的轨迹摘要行只增不减，长期使用后这张表没有上限。
    /// 每轮按最老优先限量回收，并始终保留最近 `RETAINED_TRACE_ROWS` 条。
    #[tokio::test]
    async fn expired_trace_summaries_are_pruned_in_bounded_batches_keeping_the_newest() {
        let (_dir, store) = fixture().await;
        let seeded = MAX_PRUNED_TRACES_PER_PASS + RETAINED_TRACE_ROWS + 108;
        seed_terminal_traces(&store, seeded).await;

        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();

        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cursor_run_traces")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            remaining,
            seeded - MAX_PRUNED_TRACES_PER_PASS,
            "one pass may only remove its batch, not the whole table"
        );
        let oldest: i64 = sqlx::query_scalar("SELECT MIN(received_at_ms) FROM cursor_run_traces")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            oldest,
            TRACE_SEED_BASE + MAX_PRUNED_TRACES_PER_PASS,
            "the oldest summaries go first"
        );

        // The next pass finishes what is left outside the retained window, and from
        // then on the table stays at the window size.
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        let after_second_pass: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cursor_run_traces")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(after_second_pass, RETAINED_TRACE_ROWS);
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        let after_third_pass: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cursor_run_traces")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(after_third_pass, RETAINED_TRACE_ROWS);
    }

    /// 删除只覆盖"任何列表都够不着"的行：在跑的轨迹、以及最近调用按 `run_id`
    /// 指向的轨迹都要留下。还有明细的轨迹由同事务里更早的那几条语句先回收明细，
    /// 之后才轮到摘要——那条保护单独测。
    #[tokio::test]
    async fn traces_still_in_flight_or_still_referenced_survive_expiry() {
        let (_dir, store) = fixture().await;
        let seeded = MAX_PRUNED_TRACES_PER_PASS + RETAINED_TRACE_ROWS + 108;
        seed_terminal_traces(&store, seeded).await;

        for (request_id, status, received_at_ms) in [
            ("running-trace", "running", 1),
            ("referenced-trace", "completed", 3),
        ] {
            sqlx::query(
                "INSERT INTO cursor_run_traces(request_id, route, status, received_at_ms)
                 VALUES (?, 'local_byok', ?, ?)",
            )
            .bind(request_id)
            .bind(status)
            .bind(received_at_ms)
            .execute(store.pool())
            .await
            .unwrap();
        }
        // 最近一次调用按 `run_id` 指向这条轨迹：调用详情要能打开它。
        sqlx::query(
            "INSERT INTO llm_calls(call_id, run_id, conversation_id, provider_call_index, provider_type,
                provider_url, request_type, request_url, model_id, display_name, status, created_at_ms,
                message_count, tool_count, detailed, client_source)
             VALUES ('linked-call', 'referenced-trace', 'conv', 0, 'openai', 'url', 'chat', 'url',
                     'model', 'Model', 'completed', ?, 1, 0, 0, 'cursor')",
        )
        .bind(crate::store::now_ms())
        .execute(store.pool())
        .await
        .unwrap();

        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();

        let survivors: Vec<String> = sqlx::query_scalar(
            "SELECT request_id FROM cursor_run_traces WHERE request_id IN
                ('running-trace', 'referenced-trace') ORDER BY request_id",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(
            survivors,
            vec!["referenced-trace".to_string(), "running-trace".to_string()],
            "in-flight and recently referenced traces must survive"
        );
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cursor_run_traces")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            remaining,
            seeded + 2 - MAX_PRUNED_TRACES_PER_PASS,
            "only summaries outside the retained window may go"
        );
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .is_empty());
    }

    /// 摘要与明细在同一张表的同一轮回收里删除，所以正常路径下"终态且早于窗口"就
    /// 等价于"没有明细"；这一句守着例外：只删摘要、不带走载荷。
    #[tokio::test]
    async fn a_summary_with_artifacts_is_never_removed() {
        let (_dir, store) = fixture().await;
        let seeded = MAX_PRUNED_TRACES_PER_PASS + RETAINED_TRACE_ROWS + 108;
        seed_terminal_traces(&store, seeded).await;
        let blob = store.put_blob(b"kept detail", &[]).await.unwrap();
        sqlx::query(
            "INSERT INTO cursor_run_traces(request_id, route, status, received_at_ms)
             VALUES ('kept-trace', 'local_byok', 'completed', 1)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cursor_run_trace_artifacts(
                request_id, seq, artifact_type, source, blob_id, metadata_json, created_at_ms
             ) VALUES ('kept-trace', 0, 'run_sse_chunk', 'cursor_client', ?, '{}', 1)",
        )
        .bind(blob.as_bytes().as_slice())
        .execute(store.pool())
        .await
        .unwrap();

        // 只跑这一条语句，不跑同事务里回收明细的那几条，才能构造出"旧摘要 + 还在的
        // 明细"这个组合。
        let mut tx = store.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
        let pruned = Store::prune_expired_trace_summaries(&mut tx, crate::store::now_ms())
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(pruned, MAX_PRUNED_TRACES_PER_PASS as u64);
        assert!(store.cursor_trace_exists("kept-trace").await.unwrap());
        assert!(!store
            .cursor_trace_artifacts("kept-trace")
            .await
            .unwrap()
            .is_empty());
    }

    /// 压缩记录只对"会话还在"有意义：会话回收之后调用详情也打不开它，归档文件
    /// 同样失去了引用。孤儿行（会话从未落库）也一并收走，留下来的行不带任何会话
    /// 之外的悬空引用。
    #[tokio::test]
    async fn compaction_events_follow_their_conversation() {
        use crate::store::NewCompactionEvent;
        let (_dir, store) = fixture().await;
        let now = crate::store::now_ms();
        seed_conversation(&store, "live", now, false).await;
        seed_conversation(&store, "idle", now - CONVERSATION_RETENTION_MS - 1, false).await;
        for (conversation, run, archive) in [
            ("live-conversation", "live-run", None),
            (
                "idle-conversation",
                "idle-run",
                Some("/tmp/compaction-history/idle.jsonl"),
            ),
            (
                "never-created",
                "orphan-run",
                Some("/tmp/compaction-history/orphan.jsonl"),
            ),
        ] {
            store
                .record_compaction_event(&NewCompactionEvent {
                    conversation_id: conversation,
                    run_id: run,
                    checkpoint_id: 1,
                    trigger: "preflight",
                    retention: "window",
                    estimated_tokens: Some(100),
                    calibration: None,
                    compactable_messages: 10,
                    replacement_messages: 3,
                    summary_chars: 40,
                    archive_path: archive,
                })
                .await
                .unwrap();
        }

        store.prune_inactive_storage(now).await.unwrap();

        let remaining: Vec<String> = sqlx::query_scalar(
            "SELECT conversation_id FROM compaction_events ORDER BY conversation_id",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(
            remaining,
            vec!["live-conversation".to_string()],
            "会话回收要带走它的压缩记录，孤儿行也不能留"
        );
    }

    const TRACE_SEED_BASE: i64 = 1_000;

    /// 回收本身在 30 万行规模上也要便宜：每 60 秒跑一次，卡住写锁的代价由所有写入方
    /// 分摊。默认不跑，要看数字时：`cargo test -p cursor-server --lib
    /// store::retention::tests::measure_trace_summary_pruning_at_300k_rows -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "measurement harness: seeds 300k rows; run explicitly with --ignored --nocapture"]
    async fn measure_trace_summary_pruning_at_300k_rows() {
        let (_dir, store) = fixture().await;
        // 空库上的同一趟回收：把固定开销（临时表、提交、fync）与按数据量增长的部分分开。
        let started = std::time::Instant::now();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        println!("empty pass: {:?}", started.elapsed());
        let started = std::time::Instant::now();
        sqlx::query(
            "WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < 300000)
             INSERT INTO cursor_run_traces(request_id, route, status, received_at_ms,
                request_bytes, response_bytes, response_event_count)
             SELECT 'trace-' || n, 'local_byok', 'completed', 1700000000000 + n, 1000, 2000, 8 FROM seq",
        )
        .execute(store.pool())
        .await
        .unwrap();
        println!("seed: 300000 traces in {:?}", started.elapsed());

        for pass in 1..=3 {
            let started = std::time::Instant::now();
            store
                .prune_inactive_storage(crate::store::now_ms())
                .await
                .unwrap();
            let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cursor_run_traces")
                .fetch_one(store.pool())
                .await
                .unwrap();
            println!(
                "pass {pass}: {:?}, remaining={remaining}",
                started.elapsed()
            );
        }
        let trace_bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(pgsize),0) FROM dbstat WHERE name = 'cursor_run_traces'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        println!("trace_tree_bytes={trace_bytes}");

        let started = std::time::Instant::now();
        let mut tx = store.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
        let pruned =
            Store::prune_expired_trace_summaries(&mut tx, crate::store::now_ms() - RETENTION_MS)
                .await
                .unwrap();
        tx.rollback().await.unwrap();
        println!(
            "trace delete alone: {:?}, pruned={pruned}",
            started.elapsed()
        );
    }

    /// Seeds `count` terminal, detail-free summaries long past the retention window.
    /// One statement keeps the 1000+ rows these tests need out of the suite's cost.
    async fn seed_terminal_traces(store: &Store, count: i64) {
        sqlx::query(
            "WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?)
             INSERT INTO cursor_run_traces(request_id, route, status, received_at_ms)
             SELECT 'seed-' || n, 'local_byok', 'completed', ? + n FROM seq",
        )
        .bind(count)
        .bind(TRACE_SEED_BASE)
        .execute(store.pool())
        .await
        .unwrap();
    }

    /// 一个"完整"的会话：消息 + checkpoint + 运行 + 工具轮 + 工具调用 + blob 根 + blob
    /// 图，正是整会话回收要一起带走的那些行。`updated_at_ms` 决定它是否过期。
    async fn seed_conversation(store: &Store, name: &str, updated_at_ms: i64, active_run: bool) {
        let conversation = crate::model::ConversationId(format!("{name}-conversation"));
        let root = store.ensure_conversation(&conversation).await.unwrap();
        sqlx::query(
            "UPDATE conversations SET updated_at_ms = ?, active_run_id = ?
             WHERE conversation_id = ?",
        )
        .bind(updated_at_ms)
        .bind(active_run.then(|| format!("{name}-run")))
        .bind(conversation.as_str())
        .execute(store.pool())
        .await
        .unwrap();
        let message = crate::model::CanonicalMessage::text(
            format!("{name}-message"),
            crate::model::Role::User,
            crate::model::Origin::User,
            "hello",
        );
        store
            .import_checkpoint(&conversation, std::slice::from_ref(&message))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO runs(run_id, conversation_id, base_checkpoint_id, head_checkpoint_id,
                run_kind, status, created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, 'root', 'completed', ?, ?)",
        )
        .bind(format!("{name}-run"))
        .bind(conversation.as_str())
        .bind(root.0)
        .bind(root.0)
        .bind(updated_at_ms)
        .bind(updated_at_ms)
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tool_rounds(round_id, run_id, base_checkpoint_id, assistant_json, status,
                created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, '{}', 'settled', ?, ?)",
        )
        .bind(format!("{name}-round"))
        .bind(format!("{name}-run"))
        .bind(root.0)
        .bind(updated_at_ms)
        .bind(updated_at_ms)
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tool_round_calls(round_id, call_index, call_id, model_call_id, name,
                arguments_json, status)
             VALUES (?, 0, ?, ?, 'Read', '{}', 'completed')",
        )
        .bind(format!("{name}-round"))
        .bind(format!("{name}-call"))
        .bind(format!("{name}-call"))
        .execute(store.pool())
        .await
        .unwrap();
        let blob = store.put_blob(name.as_bytes(), &[]).await.unwrap();
        store
            .retain_conversation_blobs(conversation.as_str(), std::slice::from_ref(&blob))
            .await
            .unwrap();
    }

    /// 会话数据的每个子表都随会话一起走；漏掉任何一张都会留下悬空引用
    /// （`PRAGMA foreign_key_check` 之后仍然为空）。返回值按固定顺序：
    /// 会话、消息、checkpoint、checkpoint_messages、blob 根、运行、工具轮。
    async fn conversation_rows(store: &Store, name: &str) -> Vec<i64> {
        let conversation = format!("{name}-conversation");
        let mut counts = Vec::new();
        for sql in [
            "SELECT COUNT(*) FROM conversations WHERE conversation_id = ?",
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?",
            "SELECT COUNT(*) FROM conversation_checkpoints WHERE conversation_id = ?",
            "SELECT COUNT(*) FROM checkpoint_messages WHERE conversation_id = ?",
            "SELECT COUNT(*) FROM conversation_blob_roots WHERE conversation_id = ?",
            "SELECT COUNT(*) FROM runs WHERE conversation_id = ?",
            "SELECT COUNT(*) FROM tool_rounds WHERE run_id IN (SELECT run_id FROM runs WHERE conversation_id = ?)",
        ] {
            counts.push(
                sqlx::query_scalar(sql)
                    .bind(&conversation)
                    .fetch_one(store.pool())
                    .await
                    .unwrap(),
            );
        }
        counts
    }

    /// 窗口内的会话一个字都不动：这是"最近用过的对话不受回收影响"的那条线。
    #[tokio::test]
    async fn conversation_data_inside_the_recycle_window_survives_a_pass() {
        let (_dir, store) = fixture().await;
        let now = crate::store::now_ms();
        seed_conversation(&store, "fresh", now - 60 * 60 * 1000, false).await;

        store.prune_inactive_storage(now).await.unwrap();

        assert_eq!(
            conversation_rows(&store, "fresh").await,
            vec![1, 1, 2, 1, 1, 1, 1],
            "everything of an in-window conversation stays"
        );
    }

    /// 窗口外的会话整会话回收：会话、消息、checkpoint、运行、工具轮、blob 根与 blob
    /// 一起走；调用汇总（`llm_calls`）与配置不在这条线上。
    #[tokio::test]
    async fn idle_conversation_data_is_recycled_after_the_window() {
        let (_dir, store) = fixture().await;
        let now = crate::store::now_ms();
        seed_conversation(&store, "idle", now - CONVERSATION_RETENTION_MS - 1, false).await;
        sqlx::query(
            "INSERT INTO llm_calls(call_id, run_id, conversation_id, provider_call_index,
                provider_type, provider_url, request_type, request_url, model_id, display_name,
                status, created_at_ms, message_count, tool_count, detailed)
             VALUES ('idle-call', 'idle-run', 'idle-conversation', 0, 'openai', 'url', 'chat',
                'url', 'model', 'Model', 'completed', 1, 1, 0, 0)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();
        let blobs_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(blobs_before, 1);

        store.prune_inactive_storage(now).await.unwrap();

        assert_eq!(
            conversation_rows(&store, "idle").await,
            vec![0, 0, 0, 0, 0, 0, 0],
            "an idle conversation is recycled whole"
        );
        let blobs_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            blobs_after, 0,
            "the conversation's blob graph goes with its roots"
        );
        let calls: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM llm_calls")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            calls, 1,
            "usage summaries survive the conversation they described"
        );
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .is_empty());
    }

    /// 有一轮还在跑的会话一律不动，哪怕它已经很旧（`active_run_id` 是那条硬线）；
    /// 上层还有"没有活跃传输、没有正在跑的运行"的准入，这条守着数据库这一侧。
    #[tokio::test]
    async fn a_conversation_with_a_live_run_is_never_recycled() {
        let (_dir, store) = fixture().await;
        let now = crate::store::now_ms();
        let old = now - CONVERSATION_RETENTION_MS - 1;
        seed_conversation(&store, "live", old, true).await;
        sqlx::query(
            "UPDATE runs SET status = 'running', updated_at_ms = ? WHERE run_id = 'live-run'",
        )
        .bind(now)
        .execute(store.pool())
        .await
        .unwrap();

        store.prune_inactive_storage(now).await.unwrap();

        assert_eq!(
            conversation_rows(&store, "live").await,
            vec![1, 1, 2, 1, 1, 1, 1],
            "a run in flight pins its conversation"
        );

        // 运行结束（或崩溃后被清算）之后，同一个会话回到可回收状态。
        sqlx::query("UPDATE runs SET status = 'completed' WHERE run_id = 'live-run'")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE conversations SET active_run_id = NULL WHERE conversation_id = 'live-conversation'")
            .execute(store.pool())
            .await
            .unwrap();
        store.prune_inactive_storage(now).await.unwrap();
        assert_eq!(
            conversation_rows(&store, "live").await,
            vec![0, 0, 0, 0, 0, 0, 0],
            "once the run is over the conversation is recyclable again"
        );
    }

    /// 积压（几个月没打开应用）时每轮只收一批，最老的先走：一次性删掉全部会话会长时间
    /// 占住写锁，让模型调用的落库排队。
    #[tokio::test]
    async fn a_pass_recycles_a_bounded_batch_oldest_first() {
        let (_dir, store) = fixture().await;
        let now = crate::store::now_ms();
        let base = now - CONVERSATION_RETENTION_MS - 1_000_000;
        for index in 0..(MAX_PRUNED_CONVERSATIONS_PER_PASS + 4) {
            seed_conversation(&store, &format!("batch-{index:02}"), base + index, false).await;
        }

        store.prune_inactive_storage(now).await.unwrap();

        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            remaining, 4,
            "one pass may only recycle its batch of {MAX_PRUNED_CONVERSATIONS_PER_PASS}"
        );
        let oldest_kept: Option<String> = sqlx::query_scalar(
            "SELECT conversation_id FROM conversations ORDER BY updated_at_ms LIMIT 1",
        )
        .fetch_optional(store.pool())
        .await
        .unwrap();
        assert_eq!(
            oldest_kept.as_deref(),
            Some(format!("batch-{MAX_PRUNED_CONVERSATIONS_PER_PASS:02}-conversation").as_str()),
            "the oldest conversations go first"
        );
    }

    /// 30 万条消息规模上，会话回收本身要便宜：每 60 秒跑一次，卡住写锁的代价由所有
    /// 写入方分摊。默认不跑，要看数字时：`cargo test -p cursor-server --lib
    /// store::retention::tests::measure_conversation_recycle_at_scale -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "measurement harness: seeds hundreds of conversations; run explicitly with --ignored --nocapture"]
    async fn measure_conversation_recycle_at_scale() {
        let (_dir, store) = fixture().await;
        let now = crate::store::now_ms();
        let started = std::time::Instant::now();
        store.prune_inactive_storage(now).await.unwrap();
        println!("empty pass: {:?}", started.elapsed());

        // 每个会话 600 条消息 + 40 次运行 + 40 个工具轮：接近真实库里"一天一个会话"的形状。
        let started = std::time::Instant::now();
        sqlx::query(
            "WITH RECURSIVE convs(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM convs WHERE n + 1 < 500)
             INSERT INTO conversations(conversation_id, updated_at_ms)
             SELECT 'bulk-' || n, 1 FROM convs",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversation_checkpoints(conversation_id, parent_checkpoint_id, state_digest, created_at_ms)
             SELECT conversation_id, NULL, zeroblob(32), 1 FROM conversations",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "WITH RECURSIVE msgs(c, m) AS (
                 SELECT 0, 0 UNION ALL
                 SELECT CASE WHEN m + 1 < 600 THEN c ELSE c + 1 END,
                        CASE WHEN m + 1 < 600 THEN m + 1 ELSE 0 END
                 FROM msgs WHERE c < 499 OR m < 599
             )
             INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms)
             SELECT 'bulk-' || c, 'bulk-' || c || '-' || m, 'user', 'user',
                    '{\"text\":\"' || replace(hex(zeroblob(1500)), '0', 'a') || '\"}', 1 FROM msgs",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "WITH RECURSIVE runs(c, r) AS (
                 SELECT 0, 0 UNION ALL
                 SELECT CASE WHEN r + 1 < 40 THEN c ELSE c + 1 END,
                        CASE WHEN r + 1 < 40 THEN r + 1 ELSE 0 END
                 FROM runs WHERE c < 499 OR r < 39
             )
             INSERT INTO runs(run_id, conversation_id, base_checkpoint_id, head_checkpoint_id,
                 run_kind, status, created_at_ms, updated_at_ms)
             SELECT 'bulk-' || c || '-' || r, 'bulk-' || c,
                    (SELECT MIN(checkpoint_id) FROM conversation_checkpoints WHERE conversation_id = 'bulk-' || c),
                    (SELECT MIN(checkpoint_id) FROM conversation_checkpoints WHERE conversation_id = 'bulk-' || c),
                    'root', 'completed', 1, 1
             FROM runs",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tool_rounds(round_id, run_id, base_checkpoint_id, assistant_json, status, created_at_ms, updated_at_ms)
             SELECT 'round-' || run_id, run_id, base_checkpoint_id, '{}', 'settled', 1, 1 FROM runs",
        )
        .execute(store.pool())
        .await
        .unwrap();
        let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(store.pool())
            .await
            .unwrap();
        let rounds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_rounds")
            .fetch_one(store.pool())
            .await
            .unwrap();
        println!(
            "seed: {messages} messages, {rounds} rounds in {:?}",
            started.elapsed()
        );

        for pass in 1..=4 {
            let started = std::time::Instant::now();
            store.prune_inactive_storage(now).await.unwrap();
            let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
                .fetch_one(store.pool())
                .await
                .unwrap();
            println!(
                "pass {pass}: {:?}, remaining={remaining}",
                started.elapsed()
            );
        }
    }
}
