//! Persists Run ownership, status, and provider call progress.
use sqlx::Row;

use crate::{
    model::{CheckpointId, ConversationId, PreparedRun, RunId, RunKind, Usage},
    Error, Result,
};

use super::{now_ms, Store};

/// 上一次进程留下的中间态：崩溃与强杀都来不及收尾，清算时写上这句说明原因。
pub(crate) const INTERRUPTED_BY_PROCESS_EXIT: &str = "interrupted by process exit";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedRun {
    pub run_id: RunId,
    pub conversation_id: ConversationId,
    pub head_checkpoint_id: CheckpointId,
    pub replaced_run_id: Option<RunId>,
}

impl Store {
    pub async fn claim_run(&self, prepared: &PreparedRun) -> Result<ClaimedRun> {
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = now_ms();
        Self::ensure_conversation_tx(&mut tx, &prepared.conversation_id).await?;
        let belongs: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM conversation_checkpoints
                WHERE checkpoint_id = ? AND conversation_id = ?
             )",
        )
        .bind(prepared.base_checkpoint_id.0)
        .bind(prepared.conversation_id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        if !belongs {
            return Err(Error::Store(format!(
                "base checkpoint {} does not belong to conversation {}",
                prepared.base_checkpoint_id, prepared.conversation_id
            )));
        }

        let replaced: Option<String> =
            sqlx::query_scalar("SELECT active_run_id FROM conversations WHERE conversation_id = ?")
                .bind(prepared.conversation_id.as_str())
                .fetch_one(&mut *tx)
                .await?;
        if let Some(replaced) = replaced.as_deref() {
            if replaced != prepared.run_id.as_str() {
                sqlx::query(
                    "UPDATE runs SET status = 'cancelled', updated_at_ms = ?
                     WHERE run_id = ? AND status = 'running'",
                )
                .bind(now)
                .bind(replaced)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE llm_calls SET status = 'cancelled', finished_at_ms = ?,
                     duration_ms = MAX(0, ? - created_at_ms)
                     WHERE run_id = ? AND status = 'running'",
                )
                .bind(now)
                .bind(now)
                .bind(replaced)
                .execute(&mut *tx)
                .await?;
            }
        }

        let (parent_run_id, parent_tool_call_id, run_kind, subagent_kind) =
            run_kind_columns(&prepared.kind);
        sqlx::query(
            "INSERT INTO runs
             (run_id, cursor_request_id, conversation_id, base_checkpoint_id, head_checkpoint_id,
              parent_run_id, parent_tool_call_id, run_kind, subagent_kind,
              status, created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'running', ?, ?)",
        )
        .bind(prepared.run_id.as_str())
        .bind(prepared.cursor_request_id.as_deref())
        .bind(prepared.conversation_id.as_str())
        .bind(prepared.base_checkpoint_id.0)
        .bind(prepared.base_checkpoint_id.0)
        .bind(parent_run_id)
        .bind(parent_tool_call_id)
        .bind(run_kind)
        .bind(subagent_kind)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE conversations
             SET current_checkpoint_id = ?, active_run_id = ?, updated_at_ms = ?
             WHERE conversation_id = ?",
        )
        .bind(prepared.base_checkpoint_id.0)
        .bind(prepared.run_id.as_str())
        .bind(now)
        .bind(prepared.conversation_id.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ClaimedRun {
            run_id: prepared.run_id.clone(),
            conversation_id: prepared.conversation_id.clone(),
            head_checkpoint_id: prepared.base_checkpoint_id,
            replaced_run_id: replaced
                .filter(|run| run != prepared.run_id.as_str())
                .map(RunId),
        })
    }

    pub async fn active_run_for_cursor_request(
        &self,
        cursor_request_id: &str,
    ) -> Result<Option<RunId>> {
        let run_id: Option<String> = sqlx::query_scalar(
            "SELECT run_id FROM runs
             WHERE cursor_request_id = ? AND status = 'running'
             ORDER BY created_at_ms DESC
             LIMIT 1",
        )
        .bind(cursor_request_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(run_id.map(RunId))
    }

    pub async fn begin_provider_call(&self, run_id: &RunId) -> Result<u64> {
        let _write = self.writes.lock().await;
        let index: Option<i64> = sqlx::query_scalar(
            "UPDATE runs SET provider_call_index = provider_call_index + 1, updated_at_ms = ?
             WHERE run_id = ? AND status = 'running'
             RETURNING provider_call_index",
        )
        .bind(now_ms())
        .bind(run_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        index
            .map(|index| index as u64)
            .ok_or_else(|| Error::Store(format!("run is not active: {run_id}")))
    }

    pub async fn finish_run(
        &self,
        run_id: &RunId,
        status: RunStatus,
        usage: Option<Usage>,
        failure: Option<(&str, &str)>,
    ) -> Result<bool> {
        let usage_json = serde_json::to_string(&usage)?;
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            "SELECT conversation_id, status, failure_category, failure_summary
             FROM runs WHERE run_id = ?",
        )
        .bind(run_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(Error::RunNotFound(run_id.to_string()));
        };
        let conversation_id: String = row.get("conversation_id");
        let current_status: String = row.get("status");
        let (requested_category, requested_summary) = failure.unzip();
        let terminal_status = if current_status == "running" {
            status.as_str()
        } else {
            current_status.as_str()
        };
        let stored_category: Option<String> = row.get("failure_category");
        let stored_summary: Option<String> = row.get("failure_summary");
        let (category, summary) = if current_status == "running" {
            (requested_category, requested_summary)
        } else {
            (stored_category.as_deref(), stored_summary.as_deref())
        };
        let now = now_ms();
        sqlx::query(
            "UPDATE runs SET status = ?, turn_usage_json = ?, failure_category = ?,
             failure_summary = ?, updated_at_ms = ?
             WHERE run_id = ? AND status = 'running'",
        )
        .bind(status.as_str())
        .bind(usage_json)
        .bind(category)
        .bind(summary)
        .bind(now)
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?;
        let (call_status, call_error_kind, call_error_message) = match terminal_status {
            "cancelled" => ("cancelled", None, None),
            "failed" => ("error", category, summary),
            "completed" => (
                "error",
                Some("internal"),
                Some("Run completed before LLM call reached a terminal state"),
            ),
            value => {
                return Err(Error::Store(format!(
                    "cannot finish LLM calls for non-terminal Run status: {value}"
                )))
            }
        };
        sqlx::query(
            "UPDATE llm_calls SET status = ?, finished_at_ms = ?,
             duration_ms = MAX(0, ? - created_at_ms), error_kind = ?, error_message = ?
             WHERE run_id = ? AND status = 'running'",
        )
        .bind(call_status)
        .bind(now)
        .bind(now)
        .bind(call_error_kind)
        .bind(call_error_message)
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?;
        let released = sqlx::query(
            "UPDATE conversations SET active_run_id = NULL, updated_at_ms = ?
             WHERE conversation_id = ? AND active_run_id = ?",
        )
        .bind(now)
        .bind(conversation_id)
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(released)
    }

    /// 清算上一个进程留下的 `running` 行。
    ///
    /// 崩溃、`taskkill /F`、断电都没人来得及收尾，而这些行会让存储回收停摆
    /// （`prune_inactive_storage` 见到 3 天内的 running 就直接放弃）、让调用列表永远显示
    /// "进行中"。实例锁保证同一份数据目录同时只有一个进程在写，所以只处理**严格早于**
    /// `process_started_at_ms`（本进程取得实例锁的时刻）的行；本进程启动后创建的 running
    /// 行——包括正在跑的长会话——永远不会被误伤。返回被清算的行数。
    pub(crate) async fn reconcile_interrupted_runs(
        &self,
        process_started_at_ms: i64,
    ) -> Result<u64> {
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = now_ms();
        // 指向被清算运行的 `active_run_id` 必须先解开，否则该会话永远进不了过期集合。
        // 这里不刷新 `updated_at_ms`：崩溃不是新的活动，回收窗口按真实的最后活动计算。
        sqlx::query(
            "UPDATE conversations SET active_run_id = NULL
             WHERE active_run_id IN (
                 SELECT run_id FROM runs WHERE status = 'running' AND created_at_ms < ?
             )",
        )
        .bind(process_started_at_ms)
        .execute(&mut *tx)
        .await?;
        let runs = sqlx::query(
            "UPDATE runs SET status = 'failed', failure_category = 'interrupted',
             failure_summary = ?, updated_at_ms = ?
             WHERE status = 'running' AND created_at_ms < ?",
        )
        .bind(INTERRUPTED_BY_PROCESS_EXIT)
        .bind(now)
        .bind(process_started_at_ms)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let calls = sqlx::query(
            "UPDATE llm_calls SET status = 'error', finished_at_ms = ?,
             duration_ms = MAX(0, ? - created_at_ms), error_kind = 'interrupted',
             error_message = ?
             WHERE status = 'running' AND created_at_ms < ?",
        )
        .bind(now)
        .bind(now)
        .bind(INTERRUPTED_BY_PROCESS_EXIT)
        .bind(process_started_at_ms)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let traces = sqlx::query(
            "UPDATE cursor_run_traces SET status = 'error', finished_at_ms = ?,
             error_message = ?
             WHERE status = 'running' AND received_at_ms < ?",
        )
        .bind(now)
        .bind(INTERRUPTED_BY_PROCESS_EXIT)
        .bind(process_started_at_ms)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        let interrupted = runs + calls + traces;
        if interrupted > 0 {
            tracing::info!(
                runs,
                llm_calls = calls,
                cursor_run_traces = traces,
                "cleared running rows left by a previous process"
            );
        }
        Ok(interrupted)
    }
}

fn run_kind_columns(kind: &RunKind) -> (Option<&str>, Option<&str>, &'static str, Option<String>) {
    match kind {
        RunKind::Root => (None, None, "root", None),
        RunKind::Subagent {
            parent_run_id,
            parent_tool_call_id,
            kind,
            ..
        } => (
            Some(parent_run_id.as_str()),
            Some(parent_tool_call_id.as_str()),
            "subagent",
            Some(match kind {
                crate::model::SubagentKind::GeneralPurpose => "generalPurpose".into(),
                crate::model::SubagentKind::Named(name) => name.clone(),
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        (directory, store)
    }

    /// 造一个"上一个进程留下的"运行：会话、run、provider 调用三者都停在 running。
    async fn crashed_run(store: &Store, name: &str, created_at_ms: i64) {
        let conversation = crate::model::ConversationId(format!("{name}-conversation"));
        let checkpoint = store.ensure_conversation(&conversation).await.unwrap();
        sqlx::query(
            "INSERT INTO runs(run_id,conversation_id,base_checkpoint_id,head_checkpoint_id,run_kind,status,created_at_ms,updated_at_ms)
             VALUES (?,?,?,?,'root','running',?,?)",
        )
        .bind(format!("{name}-run"))
        .bind(conversation.as_str())
        .bind(checkpoint.0)
        .bind(checkpoint.0)
        .bind(created_at_ms)
        .bind(created_at_ms)
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE conversations SET active_run_id = ? WHERE conversation_id = ?")
            .bind(format!("{name}-run"))
            .bind(conversation.as_str())
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO llm_calls(call_id,run_id,conversation_id,provider_call_index,provider_type,provider_url,request_type,request_url,model_id,display_name,status,created_at_ms,message_count,tool_count,detailed)
             VALUES (?,?,?,0,'openai-chat','https://example.com','openai-chat','https://example.com','model','Model','running',?,1,0,0)",
        )
        .bind(format!("{name}-call"))
        .bind(format!("{name}-run"))
        .bind(conversation.as_str())
        .bind(created_at_ms)
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cursor_run_traces(request_id,route,status,received_at_ms)
             VALUES (?, 'local_byok', 'running', ?)",
        )
        .bind(format!("{name}-trace"))
        .bind(created_at_ms)
        .execute(store.pool())
        .await
        .unwrap();
    }

    /// 上一次崩溃留下的 running 行必须被清算：回收不再停摆、界面不再永远显示"进行中"。
    #[tokio::test]
    async fn clears_running_rows_left_by_the_previous_process() {
        let (_dir, store) = fixture().await;
        crashed_run(&store, "crashed", 1).await;
        let started_at_ms = crate::store::now_ms();

        let interrupted = store
            .reconcile_interrupted_runs(started_at_ms)
            .await
            .unwrap();

        assert_eq!(interrupted, 3);
        let run: (String, Option<String>, Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT status, failure_category, failure_summary, updated_at_ms
             FROM runs WHERE run_id = 'crashed-run'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(run.0, "failed");
        assert_eq!(run.1.as_deref(), Some("interrupted"));
        assert_eq!(run.2.as_deref(), Some(INTERRUPTED_BY_PROCESS_EXIT));
        assert!(run.3.is_some_and(|updated| updated > 1));
        let call: (
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT status, error_kind, error_message, finished_at_ms, duration_ms
             FROM llm_calls WHERE call_id = 'crashed-call'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(call.0, "error");
        assert_eq!(call.1.as_deref(), Some("interrupted"));
        assert_eq!(call.2.as_deref(), Some(INTERRUPTED_BY_PROCESS_EXIT));
        assert!(call.3.is_some());
        assert!(call.4.is_some_and(|duration| duration >= 0));
        let trace: (String, Option<String>) =
            sqlx::query_as("SELECT status, error_message FROM cursor_run_traces WHERE request_id = 'crashed-trace'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(trace.0, "error");
        assert_eq!(trace.1.as_deref(), Some(INTERRUPTED_BY_PROCESS_EXIT));
        // 会话必须重新可回收：`active_run_id` 解开后它才进得了过期集合。
        let active: Option<String> = sqlx::query_scalar(
            "SELECT active_run_id FROM conversations WHERE conversation_id = 'crashed-conversation'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(active, None);
        let deferring: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM runs WHERE status = 'running' AND updated_at_ms >= ?",
        )
        .bind(crate::store::now_ms() - crate::store::retention::RETENTION_MS)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(deferring, 0, "清算后回收不再被遗留的 running 挡住");
    }

    /// 本进程启动后创建的 running 行（含正在跑的长会话）不是遗留，一行都不能碰。
    #[tokio::test]
    async fn leaves_running_rows_created_after_the_process_started() {
        let (_dir, store) = fixture().await;
        let started_at_ms = crate::store::now_ms();
        crashed_run(&store, "live", started_at_ms).await;

        let interrupted = store
            .reconcile_interrupted_runs(started_at_ms)
            .await
            .unwrap();

        assert_eq!(interrupted, 0);
        let run: (String, Option<i64>) =
            sqlx::query_as("SELECT status, updated_at_ms FROM runs WHERE run_id = 'live-run'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(run.0, "running");
        assert_eq!(run.1, Some(started_at_ms));
        let call: (String, Option<i64>) = sqlx::query_as(
            "SELECT status, finished_at_ms FROM llm_calls WHERE call_id = 'live-call'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(call, (String::from("running"), None));
        let active: Option<String> = sqlx::query_scalar(
            "SELECT active_run_id FROM conversations WHERE conversation_id = 'live-conversation'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(active.as_deref(), Some("live-run"));
    }

    /// 没有遗留时什么都不改：终态行、别人的 `active_run_id` 都保持原样。
    #[tokio::test]
    async fn does_nothing_when_there_is_nothing_to_reconcile() {
        let (_dir, store) = fixture().await;
        let conversation = crate::model::ConversationId("settled-conversation".into());
        let checkpoint = store.ensure_conversation(&conversation).await.unwrap();
        sqlx::query(
            "INSERT INTO runs(run_id,conversation_id,base_checkpoint_id,head_checkpoint_id,run_kind,status,created_at_ms,updated_at_ms)
             VALUES ('settled-run','settled-conversation',?,?,'root','completed',1,1)",
        )
        .bind(checkpoint.0)
        .bind(checkpoint.0)
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "UPDATE conversations SET active_run_id = 'settled-run', updated_at_ms = 1
             WHERE conversation_id = 'settled-conversation'",
        )
        .execute(store.pool())
        .await
        .unwrap();

        let interrupted = store
            .reconcile_interrupted_runs(crate::store::now_ms())
            .await
            .unwrap();

        assert_eq!(interrupted, 0);
        let conversation_state: (Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT active_run_id, updated_at_ms FROM conversations WHERE conversation_id = 'settled-conversation'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(
            conversation_state,
            (Some(String::from("settled-run")), Some(1))
        );
        let run: (String, Option<i64>) =
            sqlx::query_as("SELECT status, updated_at_ms FROM runs WHERE run_id = 'settled-run'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(run, (String::from("completed"), Some(1)));
    }
}
