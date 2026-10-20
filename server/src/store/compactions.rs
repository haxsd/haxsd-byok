//! 上下文压缩的提交记录：每次成功压缩一行，界面按 run 展示、按会话倒序排查。

use serde::Serialize;
use sqlx::sqlite::SqliteRow;
use sqlx::Row;

use super::{now_ms, Store};
use crate::Result;

/// 一次已提交的压缩。
#[derive(Clone, Debug, Serialize)]
pub struct CompactionEvent {
    pub conversation_id: String,
    pub run_id: String,
    pub checkpoint_id: i64,
    /// `preflight`（主动压力检查）、`context-overflow`（上游拒绝后补救）或 `explicit`。
    pub trigger: String,
    /// `window`（保留最近几轮）或 `none`（只留摘要）。
    pub retention: String,
    pub estimated_tokens: Option<i64>,
    pub calibration: Option<f64>,
    pub compactable_messages: i64,
    pub replacement_messages: i64,
    pub summary_chars: i64,
    /// 被摘要掉那段历史的可检索归档；写失败或超限时为空。
    pub archive_path: Option<String>,
    pub created_at_ms: i64,
}

/// 新增一条压缩记录；字段与 `CompactionEvent` 一一对应（id 由库里生成）。
pub struct NewCompactionEvent<'a> {
    pub conversation_id: &'a str,
    pub run_id: &'a str,
    pub checkpoint_id: i64,
    pub trigger: &'a str,
    pub retention: &'a str,
    pub estimated_tokens: Option<i64>,
    pub calibration: Option<f64>,
    pub compactable_messages: i64,
    pub replacement_messages: i64,
    pub summary_chars: i64,
    pub archive_path: Option<&'a str>,
}

impl Store {
    /// 记录一次已提交的压缩。
    ///
    /// 观测写入是旁路：失败只留一条 warn，不影响这次压缩本身（历史已经提交）。
    pub async fn record_compaction_event(&self, event: &NewCompactionEvent<'_>) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO compaction_events(
                conversation_id, run_id, checkpoint_id, trigger, retention,
                estimated_tokens, calibration, compactable_messages,
                replacement_messages, summary_chars, archive_path, created_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(event.conversation_id)
        .bind(event.run_id)
        .bind(event.checkpoint_id)
        .bind(event.trigger)
        .bind(event.retention)
        .bind(event.estimated_tokens)
        .bind(event.calibration)
        .bind(event.compactable_messages)
        .bind(event.replacement_messages)
        .bind(event.summary_chars)
        .bind(event.archive_path)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 某个 run 最近一次压缩；调用详情据此展示"这段历史被压缩过"。
    pub async fn compaction_event_for_run(&self, run_id: &str) -> Result<Option<CompactionEvent>> {
        let row = sqlx::query(
            "SELECT conversation_id, run_id, checkpoint_id, trigger, retention,
                    estimated_tokens, calibration, compactable_messages,
                    replacement_messages, summary_chars, archive_path, created_at_ms
             FROM compaction_events
             WHERE run_id = ?
             ORDER BY created_at_ms DESC, id DESC
             LIMIT 1",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(from_row))
    }
}

fn from_row(row: SqliteRow) -> CompactionEvent {
    // 这些列在表里都是 NOT NULL，读失败说明库被外部改坏了；用默认值兜住，
    // 不让一条观测记录把调用详情整页打空。
    let text = |name: &str| row.try_get::<String, _>(name).unwrap_or_default();
    CompactionEvent {
        conversation_id: text("conversation_id"),
        run_id: text("run_id"),
        checkpoint_id: row.try_get("checkpoint_id").unwrap_or_default(),
        trigger: text("trigger"),
        retention: text("retention"),
        estimated_tokens: row.try_get("estimated_tokens").ok().flatten(),
        calibration: row.try_get("calibration").ok().flatten(),
        compactable_messages: row.try_get("compactable_messages").unwrap_or_default(),
        replacement_messages: row.try_get("replacement_messages").unwrap_or_default(),
        summary_chars: row.try_get("summary_chars").unwrap_or_default(),
        archive_path: row.try_get("archive_path").ok().flatten(),
        created_at_ms: row.try_get("created_at_ms").unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}",
            directory.path().join("compaction.db").display()
        );
        (directory, Store::connect(&url).await.unwrap())
    }

    #[tokio::test]
    async fn a_compaction_event_round_trips_by_run() {
        let (_directory, store) = store().await;
        store
            .record_compaction_event(&NewCompactionEvent {
                conversation_id: "conversation",
                run_id: "run-1",
                checkpoint_id: 42,
                trigger: "preflight",
                retention: "window",
                estimated_tokens: Some(490_502),
                calibration: Some(1.0959),
                compactable_messages: 757,
                replacement_messages: 288,
                summary_chars: 5_095,
                archive_path: Some("/tmp/archive.jsonl"),
            })
            .await
            .unwrap();

        let event = store
            .compaction_event_for_run("run-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.checkpoint_id, 42);
        assert_eq!(event.trigger, "preflight");
        assert_eq!(event.retention, "window");
        assert_eq!(event.compactable_messages, 757);
        assert_eq!(event.replacement_messages, 288);
        assert_eq!(event.archive_path.as_deref(), Some("/tmp/archive.jsonl"));
        assert!(store
            .compaction_event_for_run("run-2")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn the_latest_event_wins_for_a_run_that_compacted_twice() {
        let (_directory, store) = store().await;
        for (checkpoint_id, compactable, replacement) in [(1_i64, 10_i64, 5_i64), (2, 20, 6)] {
            store
                .record_compaction_event(&NewCompactionEvent {
                    conversation_id: "conversation",
                    run_id: "run-1",
                    checkpoint_id,
                    trigger: "context-overflow",
                    retention: "none",
                    estimated_tokens: None,
                    calibration: None,
                    compactable_messages: compactable,
                    replacement_messages: replacement,
                    summary_chars: 1,
                    archive_path: None,
                })
                .await
                .unwrap();
        }
        let event = store
            .compaction_event_for_run("run-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.checkpoint_id, 2);
        assert_eq!(event.compactable_messages, 20);
    }
}
