//! Provides aggregate data for the control API.
//! Efficient database aggregates for the desktop overview.

use std::collections::BTreeMap;

use chrono::Utc;
use sqlx::Row;

use crate::{
    model::{ModelTokenUsage, Overview, OverviewMetrics, TokenUsageBucket, TokenUsageGranularity},
    Result,
};

use super::Store;

const OVERVIEW_DAYS: u64 = 365;
const MAX_RANGE_BUCKETS: i64 = 60;
const MAX_EXPLICIT_BUCKETS: i64 = 1440;
const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;

impl Store {
    pub async fn overview(
        &self,
        start_ms: Option<i64>,
        end_ms: Option<i64>,
        model_hashes: Option<&str>,
        bucket_ms: Option<i64>,
    ) -> Result<Overview> {
        let (token_usage_granularity, bucket_ms, series_start_ms, bucket_count) =
            token_usage_buckets(start_ms, end_ms, bucket_ms);
        // 合计的时间条件只在真有边界时出现。
        //
        // 为什么不留 `? IS NULL OR created_at_ms >= ?`：`? IS NULL` 不引用任何列，OR 子句
        // 拆不成索引约束，`llm_calls_created` 用不上。
        // 为什么两端都没给时**也不**绑一个"覆盖全表"的宽区间：合计要读整行，实测 20 万行
        // 下宽区间会把计划从顺序扫推成"索引 + 回表"（13.2ms vs 8.2ms）。无区间 = 全部历史
        // （调用列表那句"共 N 条"就按它算），这个语义下顺序扫就是最快的形状。
        // 只给了上界时才问一次最早的行（`MIN(created_at_ms)` 走 `llm_calls_created`，是一次
        // 索引定位），把下界补成具体值——这样谓词才可 sarg，显式区间的调用方真的走上索引。
        let range = match (start_ms, end_ms) {
            (Some(start_ms), Some(end_ms)) => Some((start_ms, end_ms)),
            (Some(start_ms), None) => Some((start_ms, i64::MAX)),
            (None, Some(end_ms)) => Some((earliest_call_ms(&self.pool).await?, end_ms)),
            (None, None) => None,
        };
        let aggregate = aggregate_predicates(range.is_some(), model_hashes.is_some());

        let call_sql = format!(
            "SELECT
                COUNT(*) AS llm_calls,
                COALESCE(SUM(status = 'completed'), 0) AS successful_calls,
                COALESCE(SUM(status != 'completed'), 0) AS failed_calls
             FROM llm_calls{}",
            where_clause(&call_predicates(range.is_some(), model_hashes.is_some()))
        );
        let mut call_query = sqlx::query(&call_sql);
        if let Some((start_ms, end_ms)) = range {
            call_query = call_query.bind(start_ms).bind(end_ms);
        }
        if let Some(model_hashes) = model_hashes {
            call_query = call_query.bind(model_hashes);
        }
        let call_row = call_query.fetch_one(&self.pool).await?;

        let token_sql = format!(
            "SELECT
                COALESCE(SUM({fresh_input}), 0) AS input_tokens,
                COALESCE(SUM(COALESCE(cache_read_tokens, 0)), 0) AS cache_read_tokens,
                COALESCE(SUM(COALESCE(cache_write_tokens, 0)), 0) AS cache_write_tokens,
                COALESCE(SUM(COALESCE(output_tokens, 0)), 0) AS output_tokens
             FROM llm_calls{}",
            where_clause(&aggregate),
            fresh_input = fresh_input_sql(),
        );
        let mut token_query = sqlx::query(&token_sql);
        if let Some((start_ms, end_ms)) = range {
            token_query = token_query.bind(start_ms).bind(end_ms);
        }
        if let Some(model_hashes) = model_hashes {
            token_query = token_query.bind(model_hashes);
        }
        let token_row = token_query.fetch_one(&self.pool).await?;

        let input_tokens = non_negative(token_row.try_get("input_tokens")?);
        let cache_read_tokens = non_negative(token_row.try_get("cache_read_tokens")?);
        let cache_write_tokens = non_negative(token_row.try_get("cache_write_tokens")?);
        let output_tokens = non_negative(token_row.try_get("output_tokens")?);
        let prompt_tokens = saturating_sum(&[input_tokens, cache_read_tokens, cache_write_tokens]);
        let metrics = OverviewMetrics {
            llm_calls: call_row.try_get("llm_calls")?,
            successful_calls: call_row.try_get("successful_calls")?,
            failed_calls: call_row.try_get("failed_calls")?,
            token_usage: prompt_tokens.saturating_add(output_tokens),
            prompt_tokens,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
        };

        // 一行 = 时间桶 × 模型身份（`model_hash`，`model_id`）。父桶的合计字段由这些
        // 分组求和而来，所以「合计 = 各模型之和」是构造出来的恒等式。
        let series_sql = format!(
            "SELECT
                (created_at_ms / {bucket_ms}) * {bucket_ms} AS bucket_start_ms,
                model_hash,
                model_id,
                MAX(display_name) AS display_name,
                COALESCE(SUM(status != 'running'), 0) AS llm_calls,
                COALESCE(SUM({fresh_input}), 0) AS input_tokens,
                COALESCE(SUM(COALESCE(cache_read_tokens, 0)), 0) AS cache_read_tokens,
                COALESCE(SUM(COALESCE(cache_write_tokens, 0)), 0) AS cache_write_tokens,
                COALESCE(SUM(COALESCE(output_tokens, 0)), 0) AS output_tokens
             FROM llm_calls{}
             GROUP BY bucket_start_ms, model_hash, model_id
             ORDER BY bucket_start_ms, model_hash, model_id",
            where_clause(&aggregate_predicates(true, model_hashes.is_some())),
            fresh_input = fresh_input_sql(),
        );
        let mut series_query = sqlx::query(&series_sql)
            .bind(start_ms.unwrap_or(series_start_ms).max(series_start_ms))
            .bind(end_ms.unwrap_or(i64::MAX));
        if let Some(model_hashes) = model_hashes {
            series_query = series_query.bind(model_hashes);
        }
        let rows = series_query.fetch_all(&self.pool).await?;
        let mut recorded = BTreeMap::<i64, TokenUsageBucket>::new();
        for row in rows {
            let bucket_start_ms: i64 = row.try_get("bucket_start_ms")?;
            let model = ModelTokenUsage {
                model_hash: row.try_get("model_hash")?,
                model_id: row.try_get("model_id")?,
                display_name: row.try_get("display_name")?,
                llm_calls: non_negative(row.try_get("llm_calls")?),
                input_tokens: non_negative(row.try_get("input_tokens")?),
                cache_read_tokens: non_negative(row.try_get("cache_read_tokens")?),
                cache_write_tokens: non_negative(row.try_get("cache_write_tokens")?),
                output_tokens: non_negative(row.try_get("output_tokens")?),
            };
            let bucket = recorded
                .entry(bucket_start_ms)
                .or_insert_with(|| TokenUsageBucket {
                    bucket_start_ms,
                    ..TokenUsageBucket::default()
                });
            bucket.input_tokens = bucket.input_tokens.saturating_add(model.input_tokens);
            bucket.cache_read_tokens = bucket
                .cache_read_tokens
                .saturating_add(model.cache_read_tokens);
            bucket.cache_write_tokens = bucket
                .cache_write_tokens
                .saturating_add(model.cache_write_tokens);
            bucket.output_tokens = bucket.output_tokens.saturating_add(model.output_tokens);
            bucket.models.push(model);
        }
        let token_usage_series = (0..bucket_count)
            .map(|offset| series_start_ms.saturating_add(offset.saturating_mul(bucket_ms)))
            .map(|bucket_start_ms| {
                recorded
                    .remove(&bucket_start_ms)
                    .unwrap_or(TokenUsageBucket {
                        bucket_start_ms,
                        ..TokenUsageBucket::default()
                    })
            })
            .collect();

        Ok(Overview {
            metrics,
            token_usage_granularity,
            token_usage_series,
        })
    }
}

/// 表里最早一条调用的时间；空表时 0（等价于"不设下界"）。
///
/// 只有"只给了上界"这一种请求需要它：`SELECT MIN(created_at_ms)` 走 `llm_calls_created`，
/// 是一次索引定位而不是全表扫。
async fn earliest_call_ms(pool: &sqlx::SqlitePool) -> Result<i64> {
    Ok(
        sqlx::query_scalar::<_, Option<i64>>("SELECT MIN(created_at_ms) FROM llm_calls")
            .fetch_one(pool)
            .await?
            .unwrap_or(0),
    )
}

/// 合计的时间/模型谓词：只在真有边界时才出现。
///
/// `created_at_ms >= ? AND created_at_ms < ?` 对 `llm_calls_created` 是可 sarg 的范围条件；
/// 模型筛选只在请求真的带了筛选时才拼。两者都缺时谓词表为空，SQL 里连 `WHERE` 都不出现
/// ——合计要读整行，绑一个覆盖全表的宽区间反而会把计划推成"索引 + 回表"。
fn aggregate_predicates(has_range: bool, has_model_filter: bool) -> Vec<&'static str> {
    let mut predicates: Vec<&'static str> = Vec::new();
    if has_range {
        predicates.push("created_at_ms >= ? AND created_at_ms < ?");
    }
    if has_model_filter {
        predicates.push("model_hash IN (SELECT value FROM json_each(?))");
    }
    predicates
}

/// 调用计数的谓词：在合计谓词前再钉上"跑完的调用才算数"。
fn call_predicates(has_range: bool, has_model_filter: bool) -> Vec<&'static str> {
    let mut predicates = vec!["status != 'running'"];
    predicates.extend(aggregate_predicates(has_range, has_model_filter));
    predicates
}

/// 把谓词拼成 ` WHERE a AND b`；没有谓词时返回空串。
fn where_clause(predicates: &[&str]) -> String {
    if predicates.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", predicates.join(" AND "))
    }
}

fn token_usage_buckets(
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    requested_bucket_ms: Option<i64>,
) -> (TokenUsageGranularity, i64, i64, i64) {
    if let (Some(start_ms), Some(end_ms)) = (start_ms, end_ms) {
        let duration_ms = end_ms.saturating_sub(start_ms).max(1);
        let explicit_bucket_ms = requested_bucket_ms.filter(|bucket_ms| *bucket_ms >= MINUTE_MS);
        let bucket_ms = explicit_bucket_ms.unwrap_or({
            if duration_ms <= HOUR_MS {
                MINUTE_MS
            } else if duration_ms <= MAX_RANGE_BUCKETS * HOUR_MS {
                HOUR_MS
            } else {
                DAY_MS
            }
        });
        let granularity = if bucket_ms < HOUR_MS {
            TokenUsageGranularity::Minute
        } else if bucket_ms < DAY_MS {
            TokenUsageGranularity::Hour
        } else {
            TokenUsageGranularity::Day
        };
        let max_buckets = if explicit_bucket_ms.is_some() {
            MAX_EXPLICIT_BUCKETS
        } else {
            MAX_RANGE_BUCKETS
        };
        let last_bucket_ms = end_ms.saturating_sub(1).div_euclid(bucket_ms) * bucket_ms;
        let first_bucket_ms = start_ms.div_euclid(bucket_ms) * bucket_ms;
        let bucket_count =
            ((last_bucket_ms - first_bucket_ms).div_euclid(bucket_ms) + 1).clamp(1, max_buckets);
        let series_start_ms =
            last_bucket_ms.saturating_sub((bucket_count - 1).saturating_mul(bucket_ms));
        return (granularity, bucket_ms, series_start_ms, bucket_count);
    }

    let today_start_ms = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|value| value.and_utc().timestamp_millis())
        .unwrap_or(0);
    let series_start_ms = today_start_ms.saturating_sub(
        i64::try_from(OVERVIEW_DAYS - 1)
            .unwrap_or(0)
            .saturating_mul(DAY_MS),
    );
    (
        TokenUsageGranularity::Day,
        DAY_MS,
        series_start_ms,
        i64::try_from(OVERVIEW_DAYS).unwrap_or(0),
    )
}

fn fresh_input_sql() -> &'static str {
    "CASE
        WHEN request_type = 'anthropic' THEN MAX(0, COALESCE(input_tokens, 0))
        ELSE MAX(0, COALESCE(input_tokens, 0)
            - COALESCE(cache_read_tokens, 0)
            - COALESCE(cache_write_tokens, 0))
     END"
}

fn non_negative(value: i64) -> i64 {
    value.max(0)
}

fn saturating_sum(values: &[i64]) -> i64 {
    values
        .iter()
        .fold(0_i64, |total, value| total.saturating_add(*value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelTokenUsage;

    const HOUR_MS: i64 = 60 * 60 * 1_000;
    /// 2026-01-01T00:00:00Z：整点对齐，桶边界可以直接手算。
    const START_MS: i64 = 1_767_225_600_000;

    async fn temp_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        (directory, store)
    }

    /// 直接写一行调用记录：分桶聚合读的是 `llm_calls` 的时间戳、状态、身份与用量，
    /// 测试必须自己决定这些值，而不能用 `start_llm_call` 写入的当前时间。
    /// 非 Anthropic 请求的 `input_tokens` 口径是 `input - cache_read - cache_write`。
    #[allow(clippy::too_many_arguments)]
    async fn insert_call(
        store: &Store,
        call_id: &str,
        model_hash: Option<&str>,
        model_id: &str,
        display_name: &str,
        status: &str,
        created_at_ms: i64,
        tokens: (i64, i64, i64, i64),
    ) {
        let (input_tokens, cache_read_tokens, cache_write_tokens, output_tokens) = tokens;
        sqlx::query(
            "INSERT INTO llm_calls(
                call_id, run_id, conversation_id, provider_call_index, model_hash,
                provider_type, provider_url, request_type, request_url, model_id, display_name,
                status, created_at_ms, message_count, tool_count, detailed,
                input_tokens, cache_read_tokens, cache_write_tokens, output_tokens
            ) VALUES (?, 'run', 'conversation', 0, ?, 'openai-responses', 'https://example.test',
                      'openai-responses', 'https://example.test/v1/responses', ?, ?, ?, ?, 1, 0, 0, ?, ?, ?, ?)",
        )
        .bind(call_id)
        .bind(model_hash)
        .bind(model_id)
        .bind(display_name)
        .bind(status)
        .bind(created_at_ms)
        .bind(input_tokens)
        .bind(cache_read_tokens)
        .bind(cache_write_tokens)
        .bind(output_tokens)
        .execute(store.pool())
        .await
        .unwrap();
    }

    fn model_usage<'a>(
        bucket: &'a TokenUsageBucket,
        model_hash: Option<&str>,
    ) -> &'a ModelTokenUsage {
        bucket
            .models
            .iter()
            .find(|model| model.model_hash.as_deref() == model_hash)
            .expect("bucket must contain the model group")
    }

    /// 一个桶里两个模型各自保留自己的用量，父桶的合计恰好等于这些分组之和。
    #[tokio::test]
    async fn a_bucket_keeps_each_models_tokens_separate_and_sums_them_for_the_total() {
        let (_directory, store) = temp_store().await;
        insert_call(
            &store,
            "call-deepseek",
            Some("aaaa"),
            "deepseek-v4.1-flash",
            "DeepSeek V4.1 Flash",
            "completed",
            START_MS + 60_000,
            (1_000, 800, 100, 50),
        )
        .await;
        insert_call(
            &store,
            "call-gpt",
            Some("bbbb"),
            "gpt-5",
            "GPT-5",
            "completed",
            START_MS + 120_000,
            (2_000, 0, 0, 200),
        )
        .await;

        let overview = store
            .overview(
                Some(START_MS),
                Some(START_MS + 2 * HOUR_MS),
                None,
                Some(HOUR_MS),
            )
            .await
            .unwrap();

        let bucket = &overview.token_usage_series[0];
        assert_eq!(bucket.bucket_start_ms, START_MS);
        assert_eq!(bucket.models.len(), 2);
        assert_eq!(bucket.input_tokens, 100 + 2_000);
        assert_eq!(bucket.cache_read_tokens, 800);
        assert_eq!(bucket.cache_write_tokens, 100);
        assert_eq!(bucket.output_tokens, 250);

        let deepseek = model_usage(bucket, Some("aaaa"));
        assert_eq!(deepseek.model_id, "deepseek-v4.1-flash");
        assert_eq!(deepseek.display_name, "DeepSeek V4.1 Flash");
        assert_eq!(deepseek.llm_calls, 1);
        assert_eq!(deepseek.input_tokens, 100);
        assert_eq!(deepseek.cache_read_tokens, 800);
        assert_eq!(deepseek.cache_write_tokens, 100);
        assert_eq!(deepseek.output_tokens, 50);

        let gpt = model_usage(bucket, Some("bbbb"));
        assert_eq!(gpt.model_id, "gpt-5");
        assert_eq!(gpt.display_name, "GPT-5");
        assert_eq!(gpt.llm_calls, 1);
        assert_eq!(gpt.input_tokens, 2_000);
        assert_eq!(gpt.cache_read_tokens, 0);
        assert_eq!(gpt.output_tokens, 200);

        // 范围里没有数据的桶照旧是零，且不编造任何模型分组。
        let empty = &overview.token_usage_series[1];
        assert_eq!(empty.bucket_start_ms, START_MS + HOUR_MS);
        assert!(empty.models.is_empty());
        assert_eq!(empty.input_tokens, 0);
    }

    /// 删除模型后历史调用的 `model_hash` 会被清成 NULL（见 `delete_model`）。
    /// 这些行自成一组，身份保持未知：绝不按 `model_id` 或名字猜它是哪个模型。
    #[tokio::test]
    async fn calls_without_a_model_hash_stay_an_unpriced_group_of_their_own() {
        let (_directory, store) = temp_store().await;
        insert_call(
            &store,
            "call-named",
            Some("aaaa"),
            "gpt-5",
            "GPT-5",
            "completed",
            START_MS + 60_000,
            (2_000, 0, 0, 200),
        )
        .await;
        insert_call(
            &store,
            "call-unknown",
            None,
            "deleted-model",
            "Deleted Model",
            "completed",
            START_MS + 120_000,
            (1_000, 0, 0, 100),
        )
        .await;

        let overview = store
            .overview(
                Some(START_MS),
                Some(START_MS + HOUR_MS),
                None,
                Some(HOUR_MS),
            )
            .await
            .unwrap();

        let bucket = &overview.token_usage_series[0];
        assert_eq!(bucket.models.len(), 2);
        let unknown = model_usage(bucket, None);
        assert_eq!(unknown.model_id, "deleted-model");
        assert_eq!(unknown.display_name, "Deleted Model");
        assert_eq!(unknown.llm_calls, 1);
        assert_eq!(unknown.input_tokens, 1_000);
        assert_eq!(unknown.output_tokens, 100);
        assert_eq!(bucket.input_tokens, 3_000);
    }

    /// 同一个稳定哈希 + 同一个 `model_id` 在桶里只给一行：改过名字的模型不会因为
    /// 历史名字被拆开，分组标签是数据库里确定的那个名字。
    #[tokio::test]
    async fn one_group_reports_a_single_label_for_a_renamed_model() {
        let (_directory, store) = temp_store().await;
        insert_call(
            &store,
            "call-before-rename",
            Some("aaaa"),
            "gpt-5",
            "Old Name",
            "completed",
            START_MS + 60_000,
            (1_000, 800, 100, 50),
        )
        .await;
        insert_call(
            &store,
            "call-after-rename",
            Some("aaaa"),
            "gpt-5",
            "Zeta Name",
            "completed",
            START_MS + 120_000,
            (2_000, 0, 0, 200),
        )
        .await;

        let overview = store
            .overview(
                Some(START_MS),
                Some(START_MS + HOUR_MS),
                None,
                Some(HOUR_MS),
            )
            .await
            .unwrap();

        let bucket = &overview.token_usage_series[0];
        assert_eq!(bucket.models.len(), 1);
        let group = model_usage(bucket, Some("aaaa"));
        assert_eq!(group.display_name, "Zeta Name");
        assert_eq!(group.llm_calls, 2);
        assert_eq!(group.input_tokens, 2_100);
        assert_eq!(group.output_tokens, 250);
    }

    /// 同一个模型的 `model_id` 跟着厂商改名后分成两组，但两组带着同一个稳定哈希，
    /// 价格按哈希找得到，历史用量不会被改名吞掉。
    #[tokio::test]
    async fn a_changed_model_id_keeps_the_stable_hash_for_pricing() {
        let (_directory, store) = temp_store().await;
        insert_call(
            &store,
            "call-old-id",
            Some("aaaa"),
            "deepseek-v4.1-flash",
            "DeepSeek V4.1 Flash",
            "completed",
            START_MS + 60_000,
            (1_000, 800, 100, 50),
        )
        .await;
        insert_call(
            &store,
            "call-new-id",
            Some("aaaa"),
            "deepseek-v4.1-flash-0324",
            "DeepSeek V4.1 Flash 0324",
            "completed",
            START_MS + 120_000,
            (2_000, 0, 0, 200),
        )
        .await;

        let overview = store
            .overview(
                Some(START_MS),
                Some(START_MS + HOUR_MS),
                None,
                Some(HOUR_MS),
            )
            .await
            .unwrap();

        let bucket = &overview.token_usage_series[0];
        assert_eq!(bucket.models.len(), 2);
        for model in &bucket.models {
            assert_eq!(model.model_hash.as_deref(), Some("aaaa"));
        }
        assert_eq!(
            model_usage(bucket, Some("aaaa")).model_id,
            "deepseek-v4.1-flash"
        );
        assert_eq!(bucket.input_tokens, 2_100);
        assert_eq!(bucket.models[0].input_tokens, 100);
        assert_eq!(bucket.models[1].input_tokens, 2_000);
    }

    /// 时间范围与模型筛选照旧生效：范围外、筛选外、以及没有哈希的行都不进桶。
    #[tokio::test]
    async fn buckets_honor_the_requested_range_and_model_filter() {
        let (_directory, store) = temp_store().await;
        for (call_id, model_hash, created_at_ms) in [
            ("call-before", Some("aaaa"), START_MS - 600_000),
            ("call-inside", Some("aaaa"), START_MS + 600_000),
            ("call-other-model", Some("bbbb"), START_MS + 900_000),
            ("call-unpriced", None, START_MS + 1_200_000),
            ("call-after", Some("aaaa"), START_MS + 2 * HOUR_MS),
        ] {
            insert_call(
                &store,
                call_id,
                model_hash,
                "gpt-5",
                "GPT-5",
                "completed",
                created_at_ms,
                (1_000, 0, 0, 100),
            )
            .await;
        }

        let overview = store
            .overview(
                Some(START_MS),
                Some(START_MS + HOUR_MS),
                Some(r#"["aaaa"]"#),
                Some(HOUR_MS),
            )
            .await
            .unwrap();

        assert_eq!(overview.metrics.llm_calls, 1);
        assert_eq!(overview.metrics.input_tokens, 1_000);
        assert_eq!(overview.token_usage_series.len(), 1);
        let bucket = &overview.token_usage_series[0];
        assert_eq!(bucket.models.len(), 1);
        assert_eq!(bucket.input_tokens, 1_000);
        assert_eq!(model_usage(bucket, Some("aaaa")).llm_calls, 1);
    }

    /// 桶里的调用次数与总体口径一致：还在跑的调用不算数；用量合计不做状态过滤，
    /// 仍然是范围内全部行（包含刚跑了但还没收尾的调用）的和。
    #[tokio::test]
    async fn bucket_call_counts_ignore_still_running_calls_while_tokens_sum_every_row() {
        let (_directory, store) = temp_store().await;
        insert_call(
            &store,
            "call-completed",
            Some("aaaa"),
            "gpt-5",
            "GPT-5",
            "completed",
            START_MS + 60_000,
            (1_000, 800, 100, 50),
        )
        .await;
        insert_call(
            &store,
            "call-running",
            Some("aaaa"),
            "gpt-5",
            "GPT-5",
            "running",
            START_MS + 120_000,
            (300, 100, 0, 5),
        )
        .await;

        let overview = store
            .overview(
                Some(START_MS),
                Some(START_MS + HOUR_MS),
                None,
                Some(HOUR_MS),
            )
            .await
            .unwrap();

        let bucket = &overview.token_usage_series[0];
        let group = model_usage(bucket, Some("aaaa"));
        assert_eq!(group.llm_calls, 1);
        assert_eq!(group.input_tokens, 100 + 200);
        assert_eq!(group.cache_read_tokens, 900);
        assert_eq!(group.output_tokens, 55);
        assert_eq!(overview.metrics.llm_calls, 1);
        assert_eq!(overview.metrics.input_tokens, 300);
    }

    /// 不带区间的概览（桌面端默认的全局刷新）不再全表扫 `llm_calls`，但合计的口径
    /// 保持"全部历史"：调用列表用这个计数显示"共 N 条"，把它缩成序列窗口会让那句
    /// 文案在大库上少算；一年窗口是分桶序列自己的口径。
    #[tokio::test]
    async fn overview_without_a_range_counts_all_history_while_the_series_covers_the_window() {
        let (_directory, store) = temp_store().await;
        let now = crate::store::now_ms();
        insert_call(
            &store,
            "call-in-window",
            Some("aaaa"),
            "gpt-5",
            "GPT-5",
            "completed",
            now - 24 * HOUR_MS,
            (1_000, 0, 0, 100),
        )
        .await;
        insert_call(
            &store,
            "call-before-window",
            Some("aaaa"),
            "gpt-5",
            "GPT-5",
            "completed",
            now - (OVERVIEW_DAYS as i64 + 35) * 24 * HOUR_MS,
            (7_000, 0, 0, 700),
        )
        .await;

        let overview = store.overview(None, None, None, None).await.unwrap();

        assert_eq!(overview.metrics.llm_calls, 2, "无区间的合计仍然是全部历史");
        assert_eq!(overview.metrics.input_tokens, 8_000);
        assert_eq!(overview.metrics.output_tokens, 800);
        assert_eq!(
            overview.token_usage_series.len(),
            OVERVIEW_DAYS as usize,
            "默认序列仍然是最近一年"
        );
        let series_calls: i64 = overview
            .token_usage_series
            .iter()
            .map(|bucket| {
                bucket
                    .models
                    .iter()
                    .map(|model| model.llm_calls)
                    .sum::<i64>()
            })
            .sum();
        assert_eq!(series_calls, 1, "窗口之外的历史调用不进序列");
    }

    /// 谓词只在真有边界/筛选时出现：默认路径（两端都不给、无模型筛选）连 `WHERE` 的时间
    /// 条件都没有。
    ///
    /// 这是刻意选择而不是漏改：合计要读整行，20 万行下绑一个"覆盖全表"的宽区间会把计划
    /// 从顺序扫推成"索引 + 回表"（实测 13.2ms vs 8.2ms）。
    #[test]
    fn aggregate_predicates_appear_only_when_they_can_be_sarged() {
        assert_eq!(aggregate_predicates(false, false), Vec::<&str>::new());
        assert_eq!(call_predicates(false, false), vec!["status != 'running'"]);
        assert_eq!(
            where_clause(&call_predicates(false, false)),
            " WHERE status != 'running'"
        );
        assert_eq!(where_clause(&aggregate_predicates(false, false)), "");

        assert!(aggregate_predicates(true, false)[0].starts_with("created_at_ms >= ?"));
        assert!(aggregate_predicates(true, true)[1].starts_with("model_hash IN"));
        assert_eq!(call_predicates(true, true).len(), 3);
    }

    /// 时间条件必须真的落到 `llm_calls_created` 上（有人把可选区间写回 `? IS NULL OR …`
    /// 时计划会退回全表扫），而默认路径反过来：没有区间就不许出现时间条件。
    #[tokio::test]
    async fn the_aggregate_uses_the_created_at_index_only_when_a_range_is_given() {
        let (_directory, store) = temp_store().await;

        let ranged_sql = format!(
            "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM llm_calls{}",
            where_clause(&call_predicates(true, false))
        );
        let ranged = sqlx::query(&ranged_sql).bind(START_MS).bind(i64::MAX);
        let ranged_plan = ranged
            .fetch_all(store.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String, _>("detail").unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            ranged_plan.contains("llm_calls_created"),
            "给了区间就必须走 created_at_ms 索引：{ranged_plan}"
        );

        let default_sql = format!(
            "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM llm_calls{}",
            where_clause(&call_predicates(false, false))
        );
        let default_plan = sqlx::query(&default_sql)
            .fetch_all(store.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String, _>("detail").unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            default_plan.contains("SCAN llm_calls"),
            "无区间时按顺序扫：{default_plan}"
        );
        assert!(
            !default_plan.contains("llm_calls_created"),
            "无区间时不许绑时间条件（绑了会变成索引 + 回表，实测更慢）：{default_plan}"
        );
    }
}
