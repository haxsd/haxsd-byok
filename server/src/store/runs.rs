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

    /// 这次运行还有"后台子代理"在跑吗——父轮靠它决定"先别收尾，等它们回来"。
    ///
    /// 条件要求两件事同时成立：子运行在跑，**且**父会话里那次 Task 调用已经有结果。
    /// 阻塞式 Task 期间父轮拿不到工具结果、根本走不到收尾点，所以这个条件实际只在
    /// 后台子代理上成立；之所以 join 一次而不是只看子运行，是为了万一留下孤立的
    /// running 子行时，父轮不会被白等一场。
    pub async fn has_running_background_subagents(&self, run_id: &RunId) -> Result<bool> {
        let running: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM runs child
                JOIN tool_round_calls call ON call.call_id = child.parent_tool_call_id
                WHERE child.parent_run_id = ?
                  AND child.status = 'running'
                  AND call.status = 'completed'
             )",
        )
        .bind(run_id.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(running)
    }

    /// 父会话里那次 Task 调用：只有"结果已经提交"的才是后台子代理。
    ///
    /// 阻塞式 Task 的工具结果要等子代理跑完才提交，而后台子代理的结果在它开跑后立刻
    /// 就回来了——所以这条查询既是"该不该由服务端唤醒"的判据，也顺带给出标题和父会话。
    /// 只按 `call_id` 匹配（它本身唯一），不依赖工具名，免得客户端换个名字就把唤醒断掉。
    pub async fn background_task(&self, call_id: &str) -> Result<Option<(ConversationId, String)>> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT runs.conversation_id, tool_round_calls.arguments_json
             FROM tool_round_calls
             JOIN tool_rounds ON tool_rounds.round_id = tool_round_calls.round_id
             JOIN runs ON runs.run_id = tool_rounds.run_id
             WHERE tool_round_calls.call_id = ?
               AND tool_round_calls.status = 'completed'
             ORDER BY tool_round_calls.completed_at_ms DESC
             LIMIT 1",
        )
        .bind(call_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((conversation_id, arguments)) = row else {
            return Ok(None);
        };
        let Some(title) = serde_json::from_str::<serde_json::Value>(&arguments)
            .ok()
            .and_then(|arguments| {
                arguments
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .filter(|description| !description.is_empty())
                    .map(str::to_owned)
            })
        else {
            return Ok(None);
        };
        Ok(Some((ConversationId::new(conversation_id), title)))
    }

    /// 这些父工具调用对应的子代理运行已经结束了没：结束了就带着终态返回。
    ///
    /// 用于把 `SubagentRunState` 从派发时的 `Backgrounded` 纠正成终态——客户端
    /// 靠这个状态把任务气泡标成完成。
    ///
    /// 一次查完：checkpoint 构建会带着整批待决的父工具调用进来，逐个 id 发一条 SELECT
    /// 就是 N+1（每轮 checkpoint 一次），窗口函数按父调用分组取最新一条，语义与逐条
    /// `ORDER BY updated_at_ms DESC LIMIT 1` 相同。
    pub async fn finished_child_runs(
        &self,
        parent_tool_call_ids: &[String],
    ) -> Result<Vec<(String, RunStatus, u64)>> {
        if parent_tool_call_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", parent_tool_call_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT parent_tool_call_id, status, updated_at_ms FROM (
                 SELECT parent_tool_call_id, status, updated_at_ms,
                        ROW_NUMBER() OVER (
                            PARTITION BY parent_tool_call_id ORDER BY updated_at_ms DESC
                        ) AS rank
                   FROM runs
                  WHERE parent_tool_call_id IN ({placeholders})
                    AND run_kind = 'subagent'
                    AND status IN ('completed', 'failed', 'cancelled')
             ) WHERE rank = 1"
        );
        let mut query = sqlx::query_as::<_, (String, String, i64)>(&sql);
        for tool_call_id in parent_tool_call_ids {
            query = query.bind(tool_call_id);
        }
        let rows = query.fetch_all(&self.pool).await?;
        let mut finished = Vec::with_capacity(rows.len());
        for (tool_call_id, status, updated_at_ms) in rows {
            let status = match status.as_str() {
                "completed" => RunStatus::Completed,
                "failed" => RunStatus::Failed,
                _ => RunStatus::Cancelled,
            };
            finished.push((tool_call_id, status, updated_at_ms.max(0) as u64));
        }
        Ok(finished)
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
    /// （`prune_inactive_storage` 见到 `CONVERSATION_RETENTION_MS`（当前 24 小时）内还有
    /// 更新的 running 行就整趟放弃）、让调用列表永远显示"进行中"。实例锁保证同一份数据目录
    /// 同时只有一个进程在写，所以只处理**严格早于**
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
        } => (
            parent_run_id.as_ref().map(RunId::as_str),
            parent_tool_call_id.as_deref(),
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

    /// 父轮靠它决定"先别收尾"：只有"子运行在跑 + 那次 Task 调用已经有结果"才算后台子代理，
    /// 孤立的 running 子行不会把父轮挂住。
    #[tokio::test]
    async fn detects_running_background_subagents() {
        let (_dir, store) = fixture().await;
        let conversation = crate::model::ConversationId("parent-conversation".into());
        let checkpoint = store.ensure_conversation(&conversation).await.unwrap();
        sqlx::query(
            "INSERT INTO runs(run_id,conversation_id,base_checkpoint_id,head_checkpoint_id,run_kind,status,created_at_ms,updated_at_ms)
             VALUES ('parent-run',?,?,?,'root','running',1,1)",
        )
        .bind(conversation.as_str())
        .bind(checkpoint.0)
        .bind(checkpoint.0)
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tool_rounds(round_id,run_id,base_checkpoint_id,assistant_json,status,created_at_ms,updated_at_ms)
             VALUES ('parent-round','parent-run',?,'{}','completed',1,1)",
        )
        .bind(checkpoint.0)
        .execute(store.pool())
        .await
        .unwrap();
        let parent = RunId::from("parent-run");
        assert!(!store
            .has_running_background_subagents(&parent)
            .await
            .unwrap());

        // 子运行起来了，但父会话那次 Task 调用还没有结果（阻塞式 Task 的形状）：不等待。
        sqlx::query(
            "INSERT INTO runs(run_id,conversation_id,base_checkpoint_id,head_checkpoint_id,parent_run_id,parent_tool_call_id,run_kind,status,created_at_ms,updated_at_ms)
             VALUES ('child-run',?,?,?,'parent-run','task-call','subagent','running',2,2)",
        )
        .bind(conversation.as_str())
        .bind(checkpoint.0)
        .bind(checkpoint.0)
        .execute(store.pool())
        .await
        .unwrap();
        assert!(!store
            .has_running_background_subagents(&parent)
            .await
            .unwrap());

        // 结果提交了 = 后台子代理：父轮等它。
        sqlx::query(
            "INSERT INTO tool_round_calls(round_id,call_index,call_id,model_call_id,name,arguments_json,status,completed_at_ms)
             VALUES ('parent-round',0,'task-call','model-call','Task','{}','completed',2)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        assert!(store
            .has_running_background_subagents(&parent)
            .await
            .unwrap());
        assert!(store
            .finished_child_runs(&["task-call".to_owned()])
            .await
            .unwrap()
            .is_empty());

        sqlx::query(
            "UPDATE runs SET status = 'completed', updated_at_ms = 3 WHERE run_id = 'child-run'",
        )
        .execute(store.pool())
        .await
        .unwrap();
        assert!(!store
            .has_running_background_subagents(&parent)
            .await
            .unwrap());
        assert_eq!(
            store
                .finished_child_runs(&["task-call".to_owned()])
                .await
                .unwrap(),
            vec![("task-call".to_owned(), RunStatus::Completed, 3)]
        );
    }

    /// 唤醒的判据：父会话里那次 Task 调用的结果已经提交才是后台子代理。
    #[tokio::test]
    async fn background_task_requires_a_committed_task_result() {
        let (_dir, store) = fixture().await;
        let conversation = crate::model::ConversationId("task-conversation".into());
        let checkpoint = store.ensure_conversation(&conversation).await.unwrap();
        sqlx::query(
            "INSERT INTO runs(run_id,conversation_id,base_checkpoint_id,head_checkpoint_id,run_kind,status,created_at_ms,updated_at_ms)
             VALUES ('task-run',?,?,?,'root','running',1,1)",
        )
        .bind(conversation.as_str())
        .bind(checkpoint.0)
        .bind(checkpoint.0)
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tool_rounds(round_id,run_id,base_checkpoint_id,assistant_json,status,created_at_ms,updated_at_ms)
             VALUES ('task-round','task-run',?,'{}','completed',1,1)",
        )
        .bind(checkpoint.0)
        .execute(store.pool())
        .await
        .unwrap();
        let insert_call = |call_index: i64, call_id: &'static str, status: &'static str| {
            let store = store.clone();
            async move {
                sqlx::query(
                    "INSERT INTO tool_round_calls(round_id,call_index,call_id,model_call_id,name,arguments_json,status,completed_at_ms)
                     VALUES ('task-round',?,?,'model-call','Task','{\"description\":\"研究多智能体模式\"}',?,2)",
                )
                .bind(call_index)
                .bind(call_id)
                .bind(status)
                .execute(store.pool())
                .await
                .unwrap();
            }
        };
        // 还没结果 = 阻塞式 Task：此刻不该由服务端唤醒。
        insert_call(0, "foreground-call", "running").await;
        assert!(store
            .background_task("foreground-call")
            .await
            .unwrap()
            .is_none());
        // 结果已经提交 = 后台子代理：给出父会话与标题。
        insert_call(1, "background-call", "completed").await;
        let (parent, title) = store
            .background_task("background-call")
            .await
            .unwrap()
            .expect("background task");
        assert_eq!(parent.as_str(), "task-conversation");
        assert_eq!(title, "研究多智能体模式");
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

    /// 一次查完整批的语义与逐条查一致：每个父工具调用取最新的一条**终态**子 run。
    ///
    /// checkpoint 构建带着整批待决的父工具调用进来，逐个 id 发一条 SELECT 就是 N+1；
    /// 这条用例同时钉住"取最新"（同一个 id 有多条终态）与"只认终态"（还有 running 时不算
    /// 结束），再加一条反例：run_kind 不是 subagent 的同名行不许被算进来。
    #[tokio::test]
    async fn finished_child_runs_picks_the_latest_terminal_per_parent_call() {
        let (_directory, store) = fixture().await;
        let conversation = crate::model::ConversationId("conv".into());
        let checkpoint = store.ensure_conversation(&conversation).await.unwrap();
        for (run_id, parent, kind, status, updated_at_ms) in [
            ("a-old", "call-a", "subagent", "failed", 10_i64),
            ("a-new", "call-a", "subagent", "completed", 20),
            ("b-running", "call-b", "subagent", "running", 30),
            ("b-done", "call-b", "subagent", "cancelled", 5),
            ("root-a", "call-a", "root", "completed", 999),
        ] {
            sqlx::query(
                "INSERT INTO runs(run_id,conversation_id,base_checkpoint_id,head_checkpoint_id,
                     parent_tool_call_id,run_kind,status,created_at_ms,updated_at_ms)
                 VALUES (?,?,?,?,?,?,?,1,?)",
            )
            .bind(run_id)
            .bind(conversation.as_str())
            .bind(checkpoint.0)
            .bind(checkpoint.0)
            .bind(parent)
            .bind(kind)
            .bind(status)
            .bind(updated_at_ms)
            .execute(store.pool())
            .await
            .unwrap();
        }

        let mut finished = store
            .finished_child_runs(&["call-a".into(), "call-b".into(), "call-missing".into()])
            .await
            .unwrap();
        finished.sort_by(|left, right| left.0.cmp(&right.0));

        assert_eq!(
            finished,
            vec![
                ("call-a".to_string(), RunStatus::Completed, 20_u64),
                ("call-b".to_string(), RunStatus::Cancelled, 5_u64),
            ]
        );
        assert!(
            store.finished_child_runs(&[]).await.unwrap().is_empty(),
            "空批次不该发查询"
        );
    }

    async fn plan_of(store: &Store, sql: &str, binds: &[&str]) -> String {
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut query = sqlx::query(&explain);
        for value in binds {
            query = query.bind(*value);
        }
        query
            .fetch_all(store.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String, _>("detail").unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 按父反查必须走索引；不按父反查时不该用这两个索引。
    ///
    /// 两条都是热路径：父轮收尾判定子代理在跑时每 5 秒问一次（`parent_run_id`），
    /// checkpoint 构建每轮问一次整批子代理终态（`parent_tool_call_id`）。它们以前都是
    /// 全表扫——`runs` 只随会话回收连带删除，扫描量随对话历史单调增长。
    #[tokio::test]
    async fn parent_lookups_search_their_indexes() {
        let (_directory, store) = fixture().await;
        // 造点数据再 ANALYZE：空表上规划器没有统计，计划会随启发式摇摆。
        let conversation = crate::model::ConversationId("conv".into());
        let checkpoint = store.ensure_conversation(&conversation).await.unwrap();
        for index in 0..200_i64 {
            sqlx::query(
                "INSERT INTO runs(run_id,conversation_id,base_checkpoint_id,head_checkpoint_id,
                     parent_run_id,parent_tool_call_id,run_kind,status,created_at_ms,updated_at_ms)
                 VALUES (?,?,?,?,?,?,'subagent','running',1,1)",
            )
            .bind(format!("run-{index}"))
            .bind(conversation.as_str())
            .bind(checkpoint.0)
            .bind(checkpoint.0)
            .bind(format!("parent-{}", index % 7))
            .bind(format!("call-{}", index % 11))
            .execute(store.pool())
            .await
            .unwrap();
        }
        sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();

        let by_parent_run = plan_of(
            &store,
            "SELECT EXISTS(
                 SELECT 1 FROM runs child
                 JOIN tool_round_calls call ON call.call_id = child.parent_tool_call_id
                 WHERE child.parent_run_id = ? AND child.status = 'running'
                   AND call.status = 'completed'
             )",
            &["parent-1"],
        )
        .await;
        assert!(by_parent_run.contains("runs_parent_run"), "{by_parent_run}");
        assert!(!by_parent_run.contains("SCAN runs"), "{by_parent_run}");

        let by_parent_call = plan_of(
            &store,
            "SELECT parent_tool_call_id, status, updated_at_ms FROM runs
              WHERE parent_tool_call_id = ? AND run_kind = 'subagent'
                AND status IN ('completed', 'failed', 'cancelled')",
            &["call-1"],
        )
        .await;
        assert!(
            by_parent_call.contains("runs_parent_tool_call"),
            "{by_parent_call}"
        );
        assert!(!by_parent_call.contains("SCAN runs"), "{by_parent_call}");

        let without_parent = plan_of(
            &store,
            "SELECT run_id FROM runs WHERE status = 'running'",
            &[],
        )
        .await;
        assert!(
            !without_parent.contains("runs_parent_"),
            "不按父反查时不该用父索引：{without_parent}"
        );
    }
}
