//! Persists provider call payloads, timing, and usage.
use sqlx::Row;

use crate::{
    model::{LlmCallRequest, LlmCallResponseChunk, LlmCallSummary, NewLlmCall, Usage},
    Result,
};

use super::{now_ms, Store};

const CALL_COLUMNS: &str = "client_source, call_id, run_id, conversation_id, provider_call_index, model_hash, provider_type, provider_url, request_type, request_url, model_id, display_name, reasoning_effort, fast, status, finish_reason, created_at_ms, request_started_at_ms, response_headers_at_ms, first_event_at_ms, first_text_at_ms, first_valid_response_at_ms, finished_at_ms, queue_ms, ttfb_ms, ttft_ms, ttfr_ms, duration_ms, input_tokens, output_tokens, total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, usage_json, message_count, tool_count, request_bytes, response_bytes, stream_event_count, http_status, error_kind, error_message, detailed, purpose";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContextUsageAnchor {
    pub(crate) context_input_tokens: u64,
    pub(crate) message_count: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct BufferedLlmChunk {
    pub(crate) seq: i64,
    pub(crate) elapsed_ms: i64,
    pub(crate) data: Option<Vec<u8>>,
    pub(crate) byte_count: usize,
}

impl BufferedLlmChunk {
    pub(crate) fn new(seq: i64, elapsed_ms: i64, data: &[u8]) -> Self {
        Self {
            seq,
            elapsed_ms,
            data: Some(data.to_vec()),
            byte_count: data.len(),
        }
    }

    pub(crate) fn metrics(seq: i64, elapsed_ms: i64, byte_count: usize) -> Self {
        Self {
            seq,
            elapsed_ms,
            data: None,
            byte_count,
        }
    }
}

impl Store {
    pub async fn detailed_logging(&self) -> Result<bool> {
        let value: String = sqlx::query_scalar(
            "SELECT value_json FROM service_settings WHERE setting_key = 'llm_detailed_logging'",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(serde_json::from_str(&value)?)
    }

    pub async fn set_detailed_logging(&self, enabled: bool) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('llm_detailed_logging', ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(serde_json::to_string(&enabled)?)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn start_llm_call(&self, call: &NewLlmCall) -> Result<()> {
        let _write = self.writes.lock().await;
        let now = now_ms();
        sqlx::query(
            r#"INSERT INTO llm_calls(
                call_id, run_id, conversation_id, provider_call_index, model_hash,
                provider_type, provider_url, request_type, request_url, model_id, display_name,
                reasoning_effort, fast, status,
                created_at_ms, request_started_at_ms, queue_ms, message_count, tool_count, detailed, client_source,
                purpose
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'running', ?, ?, 0, ?, ?, ?, ?, ?)"#,
        )
        .bind(&call.call_id)
        .bind(&call.run_id)
        .bind(&call.conversation_id)
        .bind(call.provider_call_index)
        .bind(&call.model_hash)
        .bind(call.provider_type.as_str())
        .bind(&call.provider_url)
        .bind(call.request_type.as_str())
        .bind(&call.request_url)
        .bind(&call.model_id)
        .bind(&call.display_name)
        .bind(&call.reasoning_effort)
        .bind(call.fast)
        .bind(now)
        .bind(now)
        .bind(call.message_count as i64)
        .bind(call.tool_count as i64)
        .bind(call.detailed)
        .bind(call.client_source.as_str())
        .bind(call.purpose.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// `serde_json` 序列化出来的字节数：只数不留。
///
/// 非 detailed 的记录只需要 `request_bytes` 这个数字，而 `to_string` 会把整份请求体
/// （含全部历史消息，几百 KB 量级）复制一份出来——只为读一次 `.len()`。
fn json_byte_len(value: &serde_json::Value) -> Result<i64> {
    #[derive(Default)]
    struct ByteCounter(usize);

    impl std::io::Write for ByteCounter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0 += buffer.len();
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0 as i64)
}

impl Store {
    pub async fn record_llm_request(
        &self,
        call_id: &str,
        headers: &serde_json::Value,
        body: &serde_json::Value,
        detailed: bool,
    ) -> Result<()> {
        // 非 detailed 模式只要一个字节数：`to_string` 会把整份请求体（含全部历史消息）
        // 复制一份出来，只为读一次 `.len()`。数一遍写入的字节就够，正文一个字节都不留。
        let (body_json, request_bytes) = if detailed {
            let body_json = serde_json::to_string(body)?;
            let bytes = body_json.len() as i64;
            (Some(body_json), bytes)
        } else {
            (None, json_byte_len(body)?)
        };
        let headers_json = detailed
            .then(|| serde_json::to_string(headers))
            .transpose()?;
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(body_json) = &body_json {
            sqlx::query("INSERT INTO llm_call_requests(call_id, headers_json, body_json, byte_count) SELECT ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM llm_calls WHERE call_id = ?)")
                .bind(call_id)
                .bind(headers_json)
                .bind(body_json)
                .bind(request_bytes)
                .bind(call_id)
                .execute(&mut *transaction)
                .await?;
        }
        sqlx::query("UPDATE llm_calls SET request_bytes = ? WHERE call_id = ?")
            .bind(request_bytes)
            .bind(call_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn record_llm_response_headers(
        &self,
        call_id: &str,
        elapsed_ms: i64,
        http_status: u16,
    ) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query("UPDATE llm_calls SET response_headers_at_ms = ?, ttfb_ms = ?, http_status = ? WHERE call_id = ?")
            .bind(now_ms())
            .bind(elapsed_ms)
            .bind(http_status as i64)
            .bind(call_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn record_llm_chunk(
        &self,
        call_id: &str,
        seq: i64,
        elapsed_ms: i64,
        data: &[u8],
        detailed: bool,
    ) -> Result<()> {
        let chunk = if detailed {
            BufferedLlmChunk::new(seq, elapsed_ms, data)
        } else {
            BufferedLlmChunk::metrics(seq, elapsed_ms, data.len())
        };
        self.record_llm_chunks(call_id, &[chunk], detailed).await
    }

    pub(crate) async fn record_llm_chunks(
        &self,
        call_id: &str,
        chunks: &[BufferedLlmChunk],
        detailed: bool,
    ) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let byte_count = chunks
            .iter()
            .map(|chunk| chunk.byte_count as i64)
            .sum::<i64>();
        let event_count = chunks.len() as i64;
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if detailed {
            for chunk in chunks {
                let data = chunk.data.as_deref().ok_or_else(|| {
                    crate::Error::Store("detailed LLM chunk is missing payload data".into())
                })?;
                sqlx::query("INSERT INTO llm_call_response_chunks(call_id, seq, received_offset_ms, data, byte_count) SELECT ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM llm_calls WHERE call_id = ?)")
                    .bind(call_id)
                    .bind(chunk.seq)
                    .bind(chunk.elapsed_ms)
                    .bind(data)
                    .bind(chunk.byte_count as i64)
                    .bind(call_id)
                    .execute(&mut *transaction)
                    .await?;
            }
        }
        sqlx::query("UPDATE llm_calls SET first_event_at_ms = COALESCE(first_event_at_ms, ?), response_bytes = response_bytes + ?, stream_event_count = stream_event_count + ? WHERE call_id = ?")
            .bind(now_ms())
            .bind(byte_count)
            .bind(event_count)
            .bind(call_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn record_llm_first_valid_response(
        &self,
        call_id: &str,
        elapsed_ms: i64,
    ) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query("UPDATE llm_calls SET first_valid_response_at_ms = COALESCE(first_valid_response_at_ms, ?), ttfr_ms = COALESCE(ttfr_ms, ?) WHERE call_id = ?")
            .bind(now_ms())
            .bind(elapsed_ms)
            .bind(call_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn record_llm_first_text(&self, call_id: &str, elapsed_ms: i64) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query("UPDATE llm_calls SET first_text_at_ms = COALESCE(first_text_at_ms, ?), ttft_ms = COALESCE(ttft_ms, ?) WHERE call_id = ?")
            .bind(now_ms())
            .bind(elapsed_ms)
            .bind(call_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn record_llm_usage(&self, call_id: &str, usage: Usage) -> Result<()> {
        let usage_json = serde_json::to_string(&usage)?;
        let _write = self.writes.lock().await;
        sqlx::query("UPDATE llm_calls SET input_tokens = ?, output_tokens = ?, total_tokens = ?, cache_read_tokens = ?, cache_write_tokens = ?, reasoning_tokens = ?, usage_json = ? WHERE call_id = ?")
            .bind(as_i64(usage.input_tokens))
            .bind(as_i64(usage.output_tokens))
            .bind(as_i64(usage.total_tokens))
            .bind(as_i64(usage.cache_read_tokens))
            .bind(as_i64(usage.cache_write_tokens))
            .bind(as_i64(usage.reasoning_tokens))
            .bind(usage_json)
            .bind(call_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn finish_llm_call(
        &self,
        call_id: &str,
        status: &str,
        finish_reason: Option<&str>,
        elapsed_ms: i64,
        error_kind: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query("UPDATE llm_calls SET status = ?, finish_reason = ?, finished_at_ms = ?, duration_ms = ?, error_kind = ?, error_message = ? WHERE call_id = ? AND status = 'running'")
            .bind(status)
            .bind(finish_reason)
            .bind(now_ms())
            .bind(elapsed_ms)
            .bind(error_kind)
            .bind(error_message)
            .bind(call_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub(crate) async fn latest_context_usage(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ContextUsageAnchor>> {
        let row = sqlx::query(
            "SELECT usage_json, message_count FROM llm_calls
             WHERE conversation_id = ?
               AND purpose = 'chat'
               AND json_extract(usage_json, '$.context_input_tokens') IS NOT NULL
             ORDER BY created_at_ms DESC, rowid DESC
             LIMIT 1",
        )
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let usage: Usage = serde_json::from_str(row.try_get("usage_json")?)?;
        let Some(context_input_tokens) = usage.context_input_tokens else {
            return Ok(None);
        };
        let message_count = row.try_get::<i64, _>("message_count")?;
        let Ok(message_count) = usize::try_from(message_count) else {
            return Ok(None);
        };
        Ok(Some(ContextUsageAnchor {
            context_input_tokens,
            message_count,
        }))
    }

    pub async fn llm_calls(&self, limit: i64) -> Result<Vec<LlmCallSummary>> {
        let rows = sqlx::query(&format!(
            "SELECT {CALL_COLUMNS} FROM llm_calls ORDER BY created_at_ms DESC LIMIT ?"
        ))
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(summary_from_row).collect()
    }

    /// Every call the Devin gateway produced, which is how the Devin page knows
    /// whether a real conversation has travelled through the gateway yet. Counting
    /// the whole table keeps that answer stable once other traffic outgrows the
    /// newest page of calls.
    pub async fn devin_call_count(&self) -> Result<usize> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM llm_calls WHERE client_source = 'devin'")
                .fetch_one(&self.pool)
                .await?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    pub async fn llm_call(&self, call_id: &str) -> Result<Option<LlmCallSummary>> {
        sqlx::query(&format!(
            "SELECT {CALL_COLUMNS} FROM llm_calls WHERE call_id = ?"
        ))
        .bind(call_id)
        .fetch_optional(&self.pool)
        .await?
        .map(summary_from_row)
        .transpose()
    }

    pub async fn llm_call_request(&self, call_id: &str) -> Result<Option<LlmCallRequest>> {
        let row = sqlx::query(
            "SELECT headers_json, body_json, byte_count FROM llm_call_requests WHERE call_id = ?",
        )
        .bind(call_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(LlmCallRequest {
                headers: serde_json::from_str(row.try_get("headers_json")?)?,
                body: serde_json::from_str(row.try_get("body_json")?)?,
                byte_count: row.try_get("byte_count")?,
            })
        })
        .transpose()
    }

    pub async fn llm_call_chunks(&self, call_id: &str) -> Result<Vec<LlmCallResponseChunk>> {
        let rows = sqlx::query("SELECT seq, received_offset_ms, data, byte_count FROM llm_call_response_chunks WHERE call_id = ? ORDER BY seq")
            .bind(call_id)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(LlmCallResponseChunk {
                    seq: row.try_get("seq")?,
                    received_offset_ms: row.try_get("received_offset_ms")?,
                    data: String::from_utf8_lossy(&row.try_get::<Vec<u8>, _>("data")?).into_owned(),
                    byte_count: row.try_get("byte_count")?,
                })
            })
            .collect()
    }
}

fn as_i64(value: Option<u64>) -> Option<i64> {
    value.map(|value| value.min(i64::MAX as u64) as i64)
}

fn summary_from_row(row: sqlx::sqlite::SqliteRow) -> Result<LlmCallSummary> {
    let usage = row.try_get::<Option<String>, _>("usage_json")?;
    Ok(LlmCallSummary {
        client_source: row.try_get("client_source")?,
        call_id: row.try_get("call_id")?,
        run_id: row.try_get("run_id")?,
        conversation_id: row.try_get("conversation_id")?,
        provider_call_index: row.try_get("provider_call_index")?,
        model_hash: row.try_get("model_hash")?,
        provider_type: row.try_get("provider_type")?,
        provider_url: row.try_get("provider_url")?,
        request_type: row.try_get("request_type")?,
        request_url: row.try_get("request_url")?,
        model_id: row.try_get("model_id")?,
        display_name: row.try_get("display_name")?,
        reasoning_effort: row.try_get("reasoning_effort")?,
        fast: Some(row.try_get("fast")?),
        status: row.try_get("status")?,
        finish_reason: row.try_get("finish_reason")?,
        created_at_ms: row.try_get("created_at_ms")?,
        request_started_at_ms: row.try_get("request_started_at_ms")?,
        response_headers_at_ms: row.try_get("response_headers_at_ms")?,
        first_event_at_ms: row.try_get("first_event_at_ms")?,
        first_text_at_ms: row.try_get("first_text_at_ms")?,
        first_valid_response_at_ms: row.try_get("first_valid_response_at_ms")?,
        finished_at_ms: row.try_get("finished_at_ms")?,
        queue_ms: row.try_get("queue_ms")?,
        ttfb_ms: row.try_get("ttfb_ms")?,
        ttft_ms: row.try_get("ttft_ms")?,
        ttfr_ms: row.try_get("ttfr_ms")?,
        duration_ms: row.try_get("duration_ms")?,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        total_tokens: row.try_get("total_tokens")?,
        cache_read_tokens: row.try_get("cache_read_tokens")?,
        cache_write_tokens: row.try_get("cache_write_tokens")?,
        reasoning_tokens: row.try_get("reasoning_tokens")?,
        usage: usage
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        message_count: row.try_get("message_count")?,
        tool_count: row.try_get("tool_count")?,
        request_bytes: row.try_get("request_bytes")?,
        response_bytes: row.try_get("response_bytes")?,
        stream_event_count: row.try_get("stream_event_count")?,
        http_status: row.try_get("http_status")?,
        error_kind: row.try_get("error_kind")?,
        error_message: row.try_get("error_message")?,
        detailed: row.try_get("detailed")?,
        purpose: row.try_get("purpose")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devin::{DEVIN_CALL_ID_PREFIX, DEVIN_EXECUTION_ID_PREFIX};
    use crate::model::ProviderType;

    fn new_call(call_id: &str) -> NewLlmCall {
        NewLlmCall {
            purpose: crate::model::CallPurpose::Chat,
            client_source: crate::model::CallClient::Internal,
            call_id: call_id.into(),
            run_id: format!("run-{call_id}"),
            conversation_id: "conversation".into(),
            provider_call_index: 0,
            model_hash: "requested-model".into(),
            provider_type: ProviderType::Plugin,
            provider_url: "plugin://test".into(),
            request_type: ProviderType::Plugin,
            request_url: "plugin://test".into(),
            model_id: "requested-model".into(),
            display_name: "Requested Model".into(),
            reasoning_effort: None,
            fast: false,
            message_count: 1,
            tool_count: 0,
            detailed: false,
        }
    }

    /// Devin 页靠这个计数判断"是否已经跑过一次对话"，因此它既要认出两种前缀，
    /// 也不能只看最新一页调用——否则其它流量一多，界面会退回"还没有调用记录"。
    #[tokio::test]
    async fn devin_call_count_covers_both_prefixes_beyond_the_newest_page() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();

        for call_id in [
            format!("{DEVIN_CALL_ID_PREFIX}0f8f4d1e"),
            format!("{DEVIN_EXECUTION_ID_PREFIX}exec-1"),
            "cursor-run:1".into(),
        ] {
            let mut call = new_call(&call_id);
            call.client_source = if call_id == "cursor-run:1" {
                crate::model::CallClient::Cursor
            } else {
                crate::model::CallClient::Devin
            };
            store.start_llm_call(&call).await.unwrap();
        }
        for index in 0..250 {
            let call_id = format!("cursor-call:{index}");
            store.start_llm_call(&new_call(&call_id)).await.unwrap();
        }

        assert_eq!(store.devin_call_count().await.unwrap(), 2);
    }

    /// 插件模型不在 model_configs 中,调用记录必须照常落库并可按其稳定 ID 筛选。
    #[tokio::test]
    async fn plugin_calls_record_without_a_model_config_row() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let plugin_model = "plugin:dev.example/codex/gpt-test";
        store
            .start_llm_call(&NewLlmCall {
                purpose: crate::model::CallPurpose::Chat,
                client_source: crate::model::CallClient::Internal,
                call_id: "plugin-call".into(),
                run_id: "run".into(),
                conversation_id: "conversation".into(),
                provider_call_index: 0,
                model_hash: plugin_model.into(),
                provider_type: ProviderType::Plugin,
                provider_url: "plugin://dev.example/codex".into(),
                request_type: ProviderType::Plugin,
                request_url: "plugin://dev.example/codex".into(),
                model_id: "gpt-test".into(),
                display_name: "GPT Test".into(),
                reasoning_effort: None,
                fast: false,
                message_count: 1,
                tool_count: 0,
                detailed: false,
            })
            .await
            .unwrap();
        store
            .finish_llm_call("plugin-call", "completed", None, 10, None, None)
            .await
            .unwrap();
        let overview = store
            .overview(None, None, Some(&format!("[\"{plugin_model}\"]")), None)
            .await
            .unwrap();
        assert_eq!(overview.metrics.llm_calls, 1);
        assert_eq!(overview.metrics.successful_calls, 1);
    }

    #[tokio::test]
    async fn latest_context_usage_follows_conversation_chronology() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();

        for (call_id, model_id, context_input_tokens, message_count) in [
            ("call-a-1", "model-a", 100_u64, 3_usize),
            ("call-b", "model-b", 200_u64, 5_usize),
            ("call-a-2", "model-a", 300_u64, 7_usize),
        ] {
            store
                .start_llm_call(&NewLlmCall {
                    purpose: crate::model::CallPurpose::Chat,
                    client_source: crate::model::CallClient::Internal,
                    call_id: call_id.into(),
                    run_id: format!("run-{call_id}"),
                    conversation_id: "conversation".into(),
                    provider_call_index: 0,
                    model_hash: model_id.into(),
                    provider_type: ProviderType::Plugin,
                    provider_url: "plugin://test".into(),
                    request_type: ProviderType::Plugin,
                    request_url: "plugin://test".into(),
                    model_id: model_id.into(),
                    display_name: model_id.into(),
                    reasoning_effort: None,
                    fast: false,
                    message_count,
                    tool_count: 0,
                    detailed: false,
                })
                .await
                .unwrap();
            store
                .record_llm_usage(
                    call_id,
                    Usage {
                        input_tokens: Some(context_input_tokens),
                        context_input_tokens: Some(context_input_tokens),
                        output_tokens: Some(10),
                        total_tokens: Some(context_input_tokens + 10),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }

        assert_eq!(
            store.latest_context_usage("conversation").await.unwrap(),
            Some(ContextUsageAnchor {
                context_input_tokens: 300,
                message_count: 7,
            })
        );
        assert_eq!(store.latest_context_usage("other").await.unwrap(), None);
    }

    /// 非 detailed 模式不落正文，但 `request_bytes` 必须与序列化长度一致。
    ///
    /// 以前这里为了这一个数字把整份请求体 `to_string` 出来（含全部历史消息）；现在只数
    /// 一遍写入的字节，所以这条用例钉的是"数字没变、正文没留"。
    #[tokio::test]
    async fn a_non_detailed_request_records_the_body_length_without_keeping_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let call = new_call("len-call");
        store.start_llm_call(&call).await.unwrap();
        let body = serde_json::json!({
            "model": "test",
            "messages": [{ "role": "user", "content": "你好，写一段代码" }],
        });

        store
            .record_llm_request("len-call", &serde_json::json!({ "x": "y" }), &body, false)
            .await
            .unwrap();

        let bytes: i64 =
            sqlx::query_scalar("SELECT request_bytes FROM llm_calls WHERE call_id = ?")
                .bind("len-call")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(bytes, serde_json::to_string(&body).unwrap().len() as i64);
        let stored: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM llm_call_requests WHERE call_id = ?")
                .bind("len-call")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(stored, 0, "非 detailed 不落正文");
    }

    async fn plan_of(store: &Store, sql: &str) -> String {
        sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}"))
            .fetch_all(store.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String, _>("detail").unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `latest_context_usage` 要走上会话索引；不带会话约束的查询不该走它。
    ///
    /// 命中：`conversation_id = ?` 加 `ORDER BY created_at_ms DESC LIMIT 1` 由
    /// `llm_calls_conversation_created` 一次定位——索引的第二列与排序同向，所以也不该再
    /// 排临时 B 树。
    /// 不该命中：没有会话号时这个索引的前导列没有约束，它帮不上忙（规划器只能回到表扫或
    /// 用 created_at_ms 的索引）——索引是给"某个会话"用的，不是给全库用的。
    #[tokio::test]
    async fn latest_context_usage_searches_the_conversation_index() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();

        // 先造点数据再 ANALYZE：空表上规划器没有统计，计划会随启发式摇摆（实测同一个查询
        // 一会儿选索引、一会儿选全表扫），那样的断言证明不了任何事。
        for index in 0..200_i64 {
            let mut call = new_call(&format!("seed-{index}"));
            call.conversation_id = if index % 2 == 0 {
                "c1".into()
            } else {
                "other".into()
            };
            store.start_llm_call(&call).await.unwrap();
        }
        sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();

        let hit = plan_of(
            &store,
            "SELECT usage_json, message_count FROM llm_calls
             WHERE conversation_id = 'c1'
               AND json_extract(usage_json, '$.context_input_tokens') IS NOT NULL
             ORDER BY created_at_ms DESC, rowid DESC
             LIMIT 1",
        )
        .await;
        assert!(hit.contains("llm_calls_conversation_created"), "{hit}");
        assert!(!hit.contains("SCAN llm_calls"), "{hit}");
        // 计划里允许出现 `USE TEMP B-TREE FOR LAST TERM OF ORDER BY`：排序里还有 `rowid DESC`
        // 这个 tiebreaker（同一毫秒的两条调用要取确定的那条），索引存的是隐含 rowid 升序，
        // 所以最后一项要排——它只在"同一个 created_at_ms 之内"排，最多一行。不允许的是
        // 为整个结果集排序（那说明索引方向根本没被用上）。
        assert!(
            !hit.contains("USE TEMP B-TREE FOR ORDER BY"),
            "不该为整个排序建临时表：{hit}"
        );

        let miss = plan_of(
            &store,
            "SELECT usage_json, message_count FROM llm_calls
             WHERE json_extract(usage_json, '$.context_input_tokens') IS NOT NULL
             ORDER BY created_at_ms DESC, rowid DESC
             LIMIT 1",
        )
        .await;
        assert!(
            !miss.contains("llm_calls_conversation_created"),
            "没有会话约束时不该用会话索引：{miss}"
        );
    }
    /// 摘要调用是真实调用，但它重放的是裁剪过的历史——输入规模不代表会话上下文。
    /// 拿它当锚点会让下一轮估算按"重放输入"起算（系统性低估，预检不触发）。
    #[tokio::test]
    async fn a_compaction_summary_call_never_becomes_the_context_anchor() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();

        for (call_id, purpose, context_input_tokens, message_count) in [
            (
                "chat-1",
                crate::model::CallPurpose::Chat,
                1_000_u64,
                40_usize,
            ),
            (
                "summary-1",
                crate::model::CallPurpose::CompactionSummary,
                300_u64,
                12_usize,
            ),
        ] {
            store
                .start_llm_call(&NewLlmCall {
                    purpose,
                    client_source: crate::model::CallClient::Internal,
                    call_id: call_id.into(),
                    run_id: format!("run-{call_id}"),
                    conversation_id: "conversation".into(),
                    provider_call_index: 0,
                    model_hash: "model".into(),
                    provider_type: ProviderType::Plugin,
                    provider_url: "plugin://test".into(),
                    request_type: ProviderType::Plugin,
                    request_url: "plugin://test".into(),
                    model_id: "model".into(),
                    display_name: "model".into(),
                    reasoning_effort: None,
                    fast: false,
                    message_count,
                    tool_count: 0,
                    detailed: false,
                })
                .await
                .unwrap();
            store
                .record_llm_usage(
                    call_id,
                    Usage {
                        input_tokens: Some(context_input_tokens),
                        context_input_tokens: Some(context_input_tokens),
                        output_tokens: Some(10),
                        total_tokens: Some(context_input_tokens + 10),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }

        assert_eq!(
            store.latest_context_usage("conversation").await.unwrap(),
            Some(ContextUsageAnchor {
                context_input_tokens: 1_000,
                message_count: 40,
            }),
            "the newest row is a summary call; the anchor must stay on the chat turn"
        );
    }
    /// 迁移 0023 给历史行留的默认值必须成立：不写 purpose 的插入按 "chat" 落库。
    #[tokio::test]
    async fn a_row_inserted_without_a_purpose_defaults_to_chat() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO llm_calls(call_id, run_id, conversation_id, provider_call_index, provider_type, provider_url, request_type, request_url, model_id, display_name, status, created_at_ms, message_count, tool_count, detailed) VALUES ('legacy', 'run', 'conversation', 0, 'openai', 'url', 'chat', 'url', 'model', 'name', 'completed', 1, 1, 0, 0)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        let purpose: String =
            sqlx::query_scalar("SELECT purpose FROM llm_calls WHERE call_id = 'legacy'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(
            purpose, "chat",
            "rows written before 0023 read as chat turns"
        );
    }
}
