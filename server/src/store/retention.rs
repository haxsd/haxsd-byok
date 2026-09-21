//! Expires inactive runtime state while preserving lifetime usage statistics.
use super::{BlobId, Store};
use crate::Result;

pub(crate) const RETENTION_MS: i64 = 3 * 24 * 60 * 60 * 1000;

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
        sqlx::query("INSERT OR IGNORE INTO retained_blobs SELECT blob_id FROM blobs WHERE last_used_at_ms >= ? UNION SELECT blob_id FROM cursor_run_trace_artifacts UNION SELECT blob_id FROM conversation_blob_roots")
            .bind(cutoff).execute(&mut *tx).await?;
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
        sqlx::query("DELETE FROM blob_edges WHERE parent_blob_id NOT IN (SELECT blob_id FROM retained_blobs) OR child_blob_id NOT IN (SELECT blob_id FROM retained_blobs)").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM blobs WHERE blob_id NOT IN (SELECT blob_id FROM retained_blobs)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE retained_blobs")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        // Deletion alone only creates reusable pages. Reclaim substantial free space
        // while the caller still holds transport admission and the write lock.
        let free: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&self.pool)
            .await?;
        let size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await?;
        if free * size >= 64 * 1024 * 1024 {
            sqlx::query("VACUUM").execute(&self.pool).await?;
        }
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
    async fn collection_reclaims_large_free_database_pages() {
        let (_dir, store) = fixture().await;
        sqlx::query("INSERT INTO blobs(blob_id,data,created_at_ms,last_used_at_ms) VALUES (randomblob(32),zeroblob(70000000),1,1)").execute(store.pool()).await.unwrap();
        store
            .prune_inactive_storage(crate::store::now_ms())
            .await
            .unwrap();
        let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(pages * 4096 < 2 * 1024 * 1024);
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
