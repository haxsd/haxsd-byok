//! Expires inactive runtime state while preserving lifetime usage statistics.
use super::{BlobId, Store};
use crate::Result;

pub(crate) const RETENTION_MS: i64 = 3 * 24 * 60 * 60 * 1000;

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

    /// Caller must exclude live transports, including requests still being compiled.
    pub(crate) async fn prune_inactive_storage(&self, now: i64) -> Result<()> {
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let cutoff = now - RETENTION_MS;
        // Ancient runs left after a crash must not permanently prevent collection.
        let running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM runs WHERE status = 'running' AND updated_at_ms >= ?",
        )
        .bind(cutoff)
        .fetch_one(&mut *tx)
        .await?;
        if running > 0 {
            return Ok(());
        }
        sqlx::query("CREATE TEMP TABLE expired_conversations AS SELECT conversation_id FROM conversations WHERE updated_at_ms < ? AND active_run_id IS NULL")
            .bind(cutoff).execute(&mut *tx).await?;
        // Child records must go first; checkpoints refer to their parents.
        for statement in [
            "DELETE FROM conversation_blob_roots WHERE conversation_id IN (SELECT conversation_id FROM expired_conversations)",
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
        for statement in [
            "DELETE FROM llm_call_requests WHERE call_id IN (SELECT call_id FROM llm_calls WHERE created_at_ms < ? AND status != 'running')",
            "DELETE FROM llm_call_response_chunks WHERE call_id IN (SELECT call_id FROM llm_calls WHERE created_at_ms < ? AND status != 'running')",
            "DELETE FROM cursor_run_trace_artifacts WHERE request_id IN (SELECT request_id FROM cursor_run_traces WHERE received_at_ms < ? AND status != 'running')",
        ] {
            sqlx::query(statement).bind(cutoff).execute(&mut *tx).await?;
        }
        sqlx::query("CREATE TEMP TABLE retained_blobs(blob_id BLOB PRIMARY KEY)")
            .execute(&mut *tx)
            .await?;
        // 只有会话根与追踪能长期钉住 blob；其余按"最近写过或读过"存活，见
        // `UNROOTED_BLOB_MS`。
        sqlx::query("INSERT OR IGNORE INTO retained_blobs SELECT blob_id FROM blobs WHERE last_used_at_ms >= ? UNION SELECT blob_id FROM cursor_run_trace_artifacts UNION SELECT blob_id FROM conversation_blob_roots")
            .bind(now - UNROOTED_BLOB_MS).execute(&mut *tx).await?;
        // Tool images in retained conversations may predate the retention window.
        let images: Vec<String> = sqlx::query_scalar("SELECT DISTINCT json_extract(payload_json, '$.content.image.blob_id') FROM messages WHERE json_extract(payload_json, '$.content.image.blob_id') IS NOT NULL")
            .fetch_all(&mut *tx).await?;
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
        let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retained_blobs")
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
        let references = sqlx::query("DELETE FROM blob_edges WHERE parent_blob_id IN (SELECT blob_id FROM expired_blobs) OR child_blob_id IN (SELECT blob_id FROM expired_blobs)").execute(&mut *tx).await?.rows_affected();
        let orphans =
            sqlx::query("DELETE FROM blobs WHERE blob_id IN (SELECT blob_id FROM expired_blobs)")
                .execute(&mut *tx)
                .await?
                .rows_affected();
        sqlx::query("DROP TABLE expired_blobs")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE retained_blobs")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        // Freed pages are reused by later writes. Automatic VACUUM would rewrite
        // the whole database while holding the same lock that blocked startup.
        tracing::info!(
            retained_blobs = retained,
            deleted_blobs = orphans,
            deleted_references = references,
            "storage retention pass completed"
        );
        Ok(())
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
}
