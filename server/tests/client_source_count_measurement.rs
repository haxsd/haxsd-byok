//! 30 万行 `llm_calls` 上 `client_source` 计数的查询计划与耗时测量。
//!
//! 默认不跑（种 30 万行要几秒），要看数字时显式运行：
//! `cargo test -p cursor-server --test client_source_count_measurement -- --ignored --nocapture`
//!
//! 结论（debug 构建、SQLite 3.46.0、本机）：
//! - 有 `llm_calls_client_source`：`SEARCH llm_calls USING COVERING INDEX ... (client_source=?)`，
//!   中位数 0.2 ms 左右；
//! - 去掉索引：`SCAN llm_calls`，中位数约 97 ms —— 与审计里"30 万行约 110 ms/次"相符。
//! - 索引本身 4.3 MB（30 万行，约 15 B/行）。

#[path = "support/fixtures.rs"]
mod fixtures;

use std::time::Instant;

use cursor_server::store::Store;

const DEVIN_COUNT_SQL: &str = "SELECT COUNT(*) FROM llm_calls WHERE client_source = 'devin'";
const CODEX_COUNT_SQL: &str = "SELECT COUNT(*) FROM llm_calls WHERE client_source = 'codex'";

async fn seed(store: &Store, rows: i64) {
    let started = Instant::now();
    sqlx::query(
        "WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?)
         INSERT INTO llm_calls(call_id, run_id, conversation_id, provider_call_index, model_hash,
            provider_type, provider_url, request_type, request_url, model_id, display_name,
            reasoning_effort, fast, status, finish_reason, created_at_ms, request_started_at_ms,
            finished_at_ms, queue_ms, duration_ms, input_tokens, output_tokens, total_tokens,
            message_count, tool_count, request_bytes, response_bytes, stream_event_count,
            detailed, client_source)
         SELECT 'call-' || n, 'run-' || (n / 10), 'conv-' || (n / 200), n % 8, NULL,
            'openai-chat', 'https://api.example.com', 'openai-chat',
            'https://api.example.com/v1/chat/completions', 'gpt-5-codex', 'GPT-5 Codex',
            'medium', 0, 'completed', 'stop', 1790000000000 + n, 1790000000000 + n,
            1790000000000 + n + 500, 12, 500, 4000 + (n % 500), 300 + (n % 200),
            4300 + (n % 700), 24 + (n % 40), 3, 180000, 24000, 480, 0,
            CASE WHEN n % 100 = 0 THEN 'devin'
                 WHEN n % 100 = 1 THEN 'codex'
                 WHEN n % 100 = 2 THEN 'internal'
                 ELSE 'cursor' END
         FROM seq",
    )
    .bind(rows)
    .execute(store.pool())
    .await
    .unwrap();
    println!("seed: {rows} rows in {:?}", started.elapsed());
}

async fn plan(store: &Store, sql: &str) -> Vec<String> {
    sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .fetch_all(store.pool())
        .await
        .unwrap()
        .into_iter()
        .map(|row| sqlx::Row::try_get::<String, _>(&row, "detail").unwrap())
        .collect()
}

async fn time(store: &Store, sql: &str, runs: usize) -> Vec<f64> {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let _: i64 = sqlx::query_scalar(sql)
            .fetch_one(store.pool())
            .await
            .unwrap();
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples
}

fn stats(samples: &[f64]) -> String {
    format!(
        "min {:.2} ms, median {:.2} ms, p90 {:.2} ms, max {:.2} ms",
        samples[0],
        samples[samples.len() / 2],
        samples[(samples.len() * 9) / 10],
        samples[samples.len() - 1]
    )
}

#[tokio::test]
#[ignore = "measurement harness: seeds 300k rows; run explicitly with --ignored --nocapture"]
async fn measure_client_source_counts_at_300k_rows() {
    let (_directory, store) = fixtures::temp_store().await;
    seed(&store, 300_000).await;

    let (rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM llm_calls")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let table_pages: i64 = sqlx::query_scalar(
        "SELECT COUNT(*), COALESCE(SUM(pgsize),0) FROM dbstat WHERE name = 'llm_calls'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    let tree: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(pgsize),0) FROM dbstat WHERE name IN ('llm_calls', 'llm_calls_client_source')")
        .fetch_one(store.pool())
        .await
        .unwrap();
    println!("rows={rows} page_size={page_size} llm_calls_pages={table_pages} tree_bytes={tree}");
    let sqlite_version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(store.pool())
        .await
        .unwrap();
    println!("sqlite_version={sqlite_version}");

    for sql in [DEVIN_COUNT_SQL, CODEX_COUNT_SQL] {
        println!("--- with migration 0014 index: {sql}");
        println!("plan: {:?}", plan(&store, sql).await);
        println!("time: {}", stats(&time(&store, sql, 25).await));
    }

    sqlx::query("DROP INDEX llm_calls_client_source")
        .execute(store.pool())
        .await
        .unwrap();
    let index_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'llm_calls_client_source'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    println!("index_exists_after_drop={index_exists}");
    for sql in [DEVIN_COUNT_SQL, CODEX_COUNT_SQL] {
        println!("--- without index (before 0014): {sql}");
        // 先各跑一次把连接上的旧编译计划挤掉，再打印真正生效的计划。
        println!("plan: {:?}", plan(&store, sql).await);
        println!("plan(again): {:?}", plan(&store, sql).await);
        println!("time: {}", stats(&time(&store, sql, 25).await));
    }

    // 索引本身的大小：建回来再看一次页数。
    sqlx::query("CREATE INDEX llm_calls_client_source ON llm_calls(client_source)")
        .execute(store.pool())
        .await
        .unwrap();
    let index_pages: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(pgsize),0) FROM dbstat WHERE name = 'llm_calls_client_source'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    println!("index_bytes={index_pages}");
}
