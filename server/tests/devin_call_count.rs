//! 状态接口按 `client_source` 统计调用数：Devin 页与 Codex 页每次刷新都会调它，
//! 界面上还会反复轮询。没有索引时这是一次 `llm_calls` 全表扫描（30 万行、debug 构建
//! 实测中位数约 97 ms/次，数字见 `client_source_count_measurement.rs`）；迁移 0014
//! 建了覆盖索引，计数只读 `client_source`，不需要回表。
//!
//! 下面两条 SQL 是调用点原样的语句，改动要连着改：
//! - `Store::devin_call_count`（server/src/store/llm_calls.rs）
//! - Codex 状态接口（server/src/codex/gateway.rs）

#[path = "support/fixtures.rs"]
mod fixtures;

use cursor_server::store::Store;

const DEVIN_COUNT_SQL: &str = "SELECT COUNT(*) FROM llm_calls WHERE client_source = 'devin'";
const CODEX_COUNT_SQL: &str = "SELECT COUNT(*) FROM llm_calls WHERE client_source = 'codex'";

async fn insert_call(store: &Store, call_id: &str, client_source: &str) {
    sqlx::query(
        "INSERT INTO llm_calls(call_id, run_id, conversation_id, provider_call_index, provider_type,
            provider_url, request_type, request_url, model_id, display_name, status, created_at_ms,
            message_count, tool_count, detailed, client_source)
         VALUES (?, ?, 'conv', 0, 'openai', 'url', 'chat', 'url', 'model', 'Model', 'completed', 1,
                 1, 0, 0, ?)",
    )
    .bind(call_id)
    .bind(format!("run-{call_id}"))
    .bind(client_source)
    .execute(store.pool())
    .await
    .unwrap();
}

/// `EXPLAIN QUERY PLAN` 的 detail 列，一行一步。
async fn query_plan(store: &Store, sql: &str) -> Vec<String> {
    sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .fetch_all(store.pool())
        .await
        .unwrap()
        .into_iter()
        .map(|row| sqlx::Row::try_get::<String, _>(&row, "detail").unwrap())
        .collect()
}

#[tokio::test]
async fn client_source_counts_read_the_index_and_count_only_their_own_client() {
    let (_directory, store) = fixtures::temp_store().await;
    for (call_id, source) in [
        ("devin-1", "devin"),
        ("devin-2", "devin"),
        ("codex-1", "codex"),
        ("cursor-1", "cursor"),
        ("internal-1", "internal"),
    ] {
        insert_call(&store, call_id, source).await;
    }

    assert_eq!(store.devin_call_count().await.unwrap(), 2);
    let codex: i64 = sqlx::query_scalar(CODEX_COUNT_SQL)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(codex, 1);

    for sql in [DEVIN_COUNT_SQL, CODEX_COUNT_SQL] {
        let plan = query_plan(&store, sql).await;
        assert!(
            plan.iter()
                .any(|step| step.contains("llm_calls_client_source")),
            "the count must read the client_source index instead of scanning the table: {plan:?}"
        );
    }
}
