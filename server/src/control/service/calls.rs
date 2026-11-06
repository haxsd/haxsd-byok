//! 控制面的调用域：概览、调用列表/详情与观测设置。

use base64::{engine::general_purpose::STANDARD, Engine};

use crate::{
    model::{CursorRunTraceArtifact, CursorRunTraceSummary, LlmCallSummary, Overview},
    Error, Result,
};

use super::{
    CallDetail, CallSummary, ControlService, CursorTraceArtifactDetail, CursorTraceDetail,
    ObservabilitySettings,
};

impl ControlService {
    pub async fn overview(
        &self,
        start_ms: Option<i64>,
        end_ms: Option<i64>,
        model_hashes: Option<&str>,
        bucket_ms: Option<i64>,
    ) -> Result<Overview> {
        self.store
            .overview(start_ms, end_ms, model_hashes, bucket_ms)
            .await
    }

    pub async fn calls(&self, limit: i64) -> Result<Vec<CallSummary>> {
        let mut calls = self
            .store
            .llm_calls(limit)
            .await?
            .into_iter()
            .map(|call| CallSummary {
                call,
                call_kind: "provider_llm",
                route: "local_byok",
            })
            .collect::<Vec<_>>();
        calls.extend(
            self.store
                .official_cursor_traces(limit)
                .await?
                .into_iter()
                .map(official_call),
        );
        calls.sort_by_key(|call| std::cmp::Reverse(call.call.created_at_ms));
        calls.truncate(limit.clamp(1, 500) as usize);
        Ok(calls)
    }

    pub async fn call(&self, call_id: &str) -> Result<CallDetail> {
        if let Some(call) = self.store.llm_call(call_id).await? {
            let cursor_trace = self.cursor_trace_detail(&call.run_id).await?;
            let trace_truncated = cursor_trace.as_ref().is_some_and(|trace| trace.truncated);
            let (response_chunks, chunks_truncated) = clip_to_budget(
                self.store.llm_call_chunks(call_id).await?,
                CALL_DETAIL_BUDGET,
                |chunk| chunk.data.len(),
            );
            // 这个 run 触发过压缩：详情里要说清楚，否则用户只会觉得"模型忘了前面的事"。
            let compaction = self
                .store
                .compaction_event_for_run(&call.run_id)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "compaction event lookup failed");
                    None
                });
            return Ok(CallDetail {
                request: self.store.llm_call_request(call_id).await?,
                response_chunks,
                truncated: chunks_truncated || trace_truncated,
                call: CallSummary {
                    call,
                    call_kind: "provider_llm",
                    route: "local_byok",
                },
                cursor_trace,
                compaction,
            });
        }
        let request_id = call_id.strip_prefix("cursor:").unwrap_or(call_id);
        let trace = self
            .store
            .cursor_trace(request_id)
            .await?
            .filter(|trace| trace.route == "cursor_official")
            .ok_or_else(|| Error::RunNotFound(format!("call {call_id}")))?;
        let cursor_trace = self.cursor_trace_detail_from(trace.clone()).await?;
        Ok(CallDetail {
            call: official_call(trace),
            request: None,
            response_chunks: Vec::new(),
            truncated: cursor_trace.truncated,
            cursor_trace: Some(cursor_trace),
            compaction: None,
        })
    }

    async fn cursor_trace_detail(&self, request_id: &str) -> Result<Option<CursorTraceDetail>> {
        let Some(trace) = self.store.cursor_trace(request_id).await? else {
            return Ok(None);
        };
        Ok(Some(self.cursor_trace_detail_from(trace).await?))
    }

    async fn cursor_trace_detail_from(
        &self,
        trace: CursorRunTraceSummary,
    ) -> Result<CursorTraceDetail> {
        let (artifacts, truncated) = clip_to_budget(
            self.store.cursor_trace_artifacts(&trace.request_id).await?,
            CALL_DETAIL_BUDGET,
            |artifact| artifact.data.len(),
        );
        Ok(CursorTraceDetail {
            trace,
            artifacts: artifacts.into_iter().map(cursor_artifact).collect(),
            truncated,
        })
    }

    pub async fn observability(&self) -> Result<ObservabilitySettings> {
        Ok(ObservabilitySettings {
            detailed: self.store.detailed_logging().await?,
        })
    }

    pub async fn set_observability(
        &self,
        settings: ObservabilitySettings,
    ) -> Result<ObservabilitySettings> {
        self.store.set_detailed_logging(settings.detailed).await?;
        Ok(settings)
    }
}

fn official_call(trace: CursorRunTraceSummary) -> CallSummary {
    let model_id = trace.model_id.clone().unwrap_or_else(|| "Cursor".into());
    let ttfb = trace
        .first_response_at_ms
        .map(|value| (value - trace.received_at_ms).max(0));
    let duration = trace
        .finished_at_ms
        .map(|value| (value - trace.received_at_ms).max(0));
    let error = trace.error_message.clone();
    CallSummary {
        call: LlmCallSummary {
            client_source: "cursor".into(),
            call_id: format!("cursor:{}", trace.request_id),
            run_id: trace.request_id.clone(),
            conversation_id: trace
                .conversation_id
                .clone()
                .unwrap_or_else(|| trace.request_id.clone()),
            provider_call_index: 0,
            model_hash: None,
            provider_type: "cursor-official".into(),
            provider_url: "https://api2.cursor.sh".into(),
            request_type: "cursor-run-sse".into(),
            request_url: "https://api2.cursor.sh/agent.v1.AgentService/RunSSE".into(),
            model_id: model_id.clone(),
            display_name: model_id,
            reasoning_effort: None,
            fast: None,
            status: trace.status.clone(),
            finish_reason: None,
            created_at_ms: trace.received_at_ms,
            request_started_at_ms: Some(trace.received_at_ms),
            response_headers_at_ms: trace.first_response_at_ms,
            first_event_at_ms: trace.first_response_at_ms,
            first_text_at_ms: None,
            first_valid_response_at_ms: None,
            finished_at_ms: trace.finished_at_ms,
            queue_ms: None,
            ttfb_ms: ttfb,
            ttft_ms: None,
            ttfr_ms: None,
            duration_ms: duration,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            usage: None,
            message_count: 0,
            tool_count: 0,
            request_bytes: Some(trace.request_bytes),
            response_bytes: trace.response_bytes,
            stream_event_count: trace.response_event_count,
            http_status: trace.http_status,
            error_kind: error.as_ref().map(|_| "cursor_official".into()),
            error_message: error,
            detailed: true,
            purpose: "chat".into(),
        },
        call_kind: "cursor_official",
        route: "cursor_official",
    }
}

/// 一次调用详情响应的正文预算。
///
/// 详情页把响应原样读进内存：前端还要 `JSON.parse` + `JSON.stringify(…, null, 2)` 再交给
/// Monaco 建全文模型，服务端 serialize 一次也是同一量级。以前这里没有任何上限——单次请求
/// 的轨迹工件与响应分片全量取回，3 天保留窗口内的一次大请求就够让详情窗口在主线程上长
/// 时间无响应。闸门按"条数 + 总字节"两道，截断时在响应里标 `truncated`：用户至少知道看到
/// 的不是全部，而不是以为详情就这么点。
#[derive(Clone, Copy)]
struct DetailBudget {
    items: usize,
    bytes: usize,
}

/// 500 条、4 MiB 原始正文。base64 会放大 4/3，所以真实响应体在这个数的 1.34 倍以内；
/// 正文远超这个量级的调用，详情页本来也读不完。轨迹工件与响应分片各用一份预算，整个详情
/// 响应因此被限制在 8 MiB 正文（加一份请求体）以内。
const CALL_DETAIL_BUDGET: DetailBudget = DetailBudget {
    items: 500,
    bytes: 4 * 1024 * 1024,
};

/// 按预算截断正文列表，返回保留下来的部分与"是否截断过"。
///
/// 用原始字节而不是序列化后的长度：`data.len()` 已经在手里，而且放大倍数固定，不必为了
/// 量一遍再序列化一次。
fn clip_to_budget<T>(
    items: Vec<T>,
    budget: DetailBudget,
    length_of: impl Fn(&T) -> usize,
) -> (Vec<T>, bool) {
    let mut kept = Vec::new();
    let mut bytes = 0usize;
    for item in items {
        if kept.len() >= budget.items || bytes.saturating_add(length_of(&item)) > budget.bytes {
            return (kept, true);
        }
        bytes += length_of(&item);
        kept.push(item);
    }
    (kept, false)
}

fn cursor_artifact(artifact: CursorRunTraceArtifact) -> CursorTraceArtifactDetail {
    let byte_count = artifact.data.len();
    let (encoding, data) = match readable_utf8(&artifact.data) {
        Some(value) => ("utf8", value.into()),
        None => ("base64", STANDARD.encode(&artifact.data)),
    };
    CursorTraceArtifactDetail {
        seq: artifact.seq,
        artifact_type: artifact.artifact_type,
        source: artifact.source,
        metadata: artifact.metadata,
        created_at_ms: artifact.created_at_ms,
        byte_count,
        encoding,
        data,
    }
}

fn readable_utf8(data: &[u8]) -> Option<&str> {
    let value = std::str::from_utf8(data).ok()?;
    value
        .chars()
        .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_call_detail_budget_clips_the_body_and_reports_it() {
        let budget = DetailBudget {
            items: 3,
            bytes: 10,
        };

        // 没超预算：原样返回，不能谎报截断。
        let (kept, truncated) = clip_to_budget(vec![2, 3], budget, |size| *size as usize);
        assert_eq!(kept, vec![2, 3]);
        assert!(!truncated);

        // 条目闸门。
        let (kept, truncated) = clip_to_budget(vec![1, 1, 1, 1], budget, |size| *size as usize);
        assert_eq!(kept, vec![1, 1, 1]);
        assert!(truncated);

        // 字节闸门：一条超大的正文不能因为"排在第一"就整个塞进去。
        let (kept, truncated) = clip_to_budget(vec![1, 1, 9, 1], budget, |size| *size as usize);
        assert_eq!(kept, vec![1, 1]);
        assert!(truncated);

        // 第一条就超预算：回空列表也要标截断，页面才知道"不是没有正文"。
        let (kept, truncated) = clip_to_budget(vec![11], budget, |size| *size as usize);
        assert!(kept.is_empty());
        assert!(truncated);
    }

    /// 工件编码：可读 UTF-8 走 utf8，其余走 base64（审计 11/P3-7）。
    #[test]
    fn an_artifact_uses_utf8_only_when_it_is_readable_text() {
        let artifact = |data: Vec<u8>| CursorRunTraceArtifact {
            seq: 1,
            artifact_type: "response".into(),
            source: "upstream".into(),
            metadata: serde_json::json!({}),
            data,
            created_at_ms: 0,
        };

        let text = cursor_artifact(artifact(b"line one\nline two\ttabbed\r\n".to_vec()));
        assert_eq!(text.encoding, "utf8");
        assert!(text.data.starts_with("line one"));
        assert_eq!(text.byte_count, 26);

        // 二进制不是合法 UTF-8。
        let binary = cursor_artifact(artifact(vec![0xff, 0xfe, 0x00, 0x01]));
        assert_eq!(binary.encoding, "base64");
        assert_eq!(binary.data, STANDARD.encode([0xff, 0xfe, 0x00, 0x01]));
        assert_eq!(binary.byte_count, 4);

        // 合法 UTF-8 但含控制字符（终端转义等）：进 base64，免得前端把它当文本渲染。
        let control = cursor_artifact(artifact(b"\x1b[31mred\x1b[0m".to_vec()));
        assert_eq!(control.encoding, "base64");
    }
}
