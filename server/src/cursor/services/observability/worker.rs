use std::{
    collections::{BTreeMap, HashMap},
    sync::atomic::Ordering,
    time::Duration,
};

use tokio::sync::mpsc;

use crate::store::{BufferedCursorTraceChunk, Store};

use super::event::{TraceEvent, TRACE_ACTIVE, TRACE_DISABLED};

const MAX_BUFFERED_CHUNKS: usize = 32;
const MAX_BUFFERED_BYTES: usize = 256 * 1024;
/// flush 失败重排时的硬上限。软阈值只在压入新分片之后检查，写库一直失败时缓冲会随每条
/// 分片无限增长；越过这些上限就丢掉最旧的分片（记录少一段，但不能把进程内存吃掉）。
const MAX_RETAINED_CHUNKS: usize = 512;
const MAX_RETAINED_BYTES: usize = 2 * 1024 * 1024;
/// 连续失败到这么多次就放弃这个 request_id 的响应分片：一直失败时不再每 50ms 重试一遍。
const MAX_FLUSH_FAILURES: u32 = 10;
/// 等待重排的请求上限。seqno 缺一个时后面的请求全压在 `pending` 里，没有上限的话
/// 上游漏发一个 seqno 就会把整条 trace 的请求正文都攒在内存里。
const MAX_PENDING_REQUESTS: usize = 100;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy)]
enum TraceState {
    Active,
    Disabled,
}

#[derive(Default)]
struct ResponseBuffer {
    chunks: Vec<BufferedCursorTraceChunk>,
    bytes: usize,
    /// 连续 flush 失败次数。成功一次缓冲就被移出 map，所以不需要归零。
    failures: u32,
}

impl ResponseBuffer {
    /// 压入一条分片，返回为守住硬上限而丢掉的最旧分片数。
    fn push(&mut self, source: &str, data: &bytes::Bytes) -> usize {
        self.bytes += data.len();
        self.chunks
            .push(BufferedCursorTraceChunk::new(source, data));
        let mut dropped = 0;
        // 至少留最新这一条：单条分片自己就超过字节上限时也还要留下它。
        while self.chunks.len() > MAX_RETAINED_CHUNKS
            || (self.bytes > MAX_RETAINED_BYTES && self.chunks.len() > 1)
        {
            let oldest = self.chunks.remove(0);
            self.bytes = self.bytes.saturating_sub(oldest.data.len());
            dropped += 1;
        }
        dropped
    }

    /// 是否到了该落库的软阈值。
    fn is_full(&self) -> bool {
        self.chunks.len() >= MAX_BUFFERED_CHUNKS || self.bytes >= MAX_BUFFERED_BYTES
    }

    /// 一次 flush 失败：返回 false 表示连续失败太多，放弃这份缓冲（不再重排、不再重试）。
    fn retry_or_give_up(&mut self) -> bool {
        self.failures += 1;
        self.failures < MAX_FLUSH_FAILURES
    }
}

struct BufferedRequest {
    artifact_type: String,
    data: bytes::Bytes,
    metadata: serde_json::Value,
}

#[derive(Default)]
struct RequestOrder {
    next: i64,
    pending: BTreeMap<i64, Vec<BufferedRequest>>,
    pending_bytes: usize,
}

impl RequestOrder {
    /// 记入一条等待重排的请求。
    fn hold(&mut self, seqno: i64, request: BufferedRequest) {
        self.pending_bytes += request.data.len();
        self.pending.entry(seqno).or_default().push(request);
    }

    /// 缺口是否已经大到不该再等：再攒下去就是整条 trace 的请求正文都在内存里。
    fn overflowed(&self) -> bool {
        self.pending.len() > MAX_PENDING_REQUESTS || self.pending_bytes > MAX_PENDING_BYTES
    }

    /// 取出 seqno 从 `next` 起连续、可以按序落库的请求。
    fn drain_ready(&mut self) -> Vec<BufferedRequest> {
        let mut ready = Vec::new();
        while let Some(requests) = self.pending.remove(&self.next) {
            self.pending_bytes = self.pending_bytes.saturating_sub(
                requests
                    .iter()
                    .map(|request| request.data.len())
                    .sum::<usize>(),
            );
            ready.extend(requests);
            self.next = self.next.saturating_add(1);
        }
        ready
    }

    /// 放弃重排：按 seqno 升序取出全部待落库请求，并把 `next` 推到最大 seqno 之后，
    /// 这样迟到的旧 seqno 会走"直接落库"分支，不再把顺序搅回去。
    fn take_all(&mut self) -> Vec<BufferedRequest> {
        self.pending_bytes = 0;
        let mut highest = self.next.saturating_sub(1);
        let mut ready = Vec::new();
        for (seqno, requests) in std::mem::take(&mut self.pending) {
            highest = highest.max(seqno);
            ready.extend(requests);
        }
        self.next = highest.saturating_add(1);
        ready
    }
}

pub(super) async fn run(store: Store, mut receiver: mpsc::Receiver<TraceEvent>) {
    let mut states = HashMap::<String, TraceState>::new();
    let mut buffers = HashMap::<String, ResponseBuffer>::new();
    let mut request_orders = HashMap::<String, RequestOrder>::new();
    let mut interval = tokio::time::interval(FLUSH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = receiver.recv() => {
                let Some(event) = event else {
                    // 关闭路径：响应缓冲与"还在等 seqno 缺口"的请求工件都要落库。以前只 flush
                    // 缓冲，重排中（缺口一直没补上）的请求正文随 worker 一起消失——那条 trace
                    // 有响应分片却没有对应的请求。
                    flush_all(&store, &mut buffers).await;
                    flush_all_orders(&store, &mut request_orders).await;
                    return;
                };
                process(&store, &mut states, &mut buffers, &mut request_orders, event).await;
            }
            _ = interval.tick() => flush_all(&store, &mut buffers).await,
        }
    }
}

async fn process(
    store: &Store,
    states: &mut HashMap<String, TraceState>,
    buffers: &mut HashMap<String, ResponseBuffer>,
    request_orders: &mut HashMap<String, RequestOrder>,
    event: TraceEvent,
) {
    let request_id = event.request_id().to_owned();
    let finishes_trace = matches!(&event, TraceEvent::Finish { .. });
    match event {
        TraceEvent::Begin {
            request_id,
            activation,
            conversation_id,
            route,
            model_id,
        } => {
            let state = match store
                .start_cursor_trace_if_detailed(
                    &request_id,
                    conversation_id.as_deref(),
                    &route,
                    model_id.as_deref(),
                )
                .await
            {
                Ok(true) => TraceState::Active,
                Ok(false) => TraceState::Disabled,
                Err(error) => {
                    tracing::warn!(%request_id, %error, "failed to start Cursor trace");
                    TraceState::Disabled
                }
            };
            activation.store(
                match state {
                    TraceState::Active => TRACE_ACTIVE,
                    TraceState::Disabled => TRACE_DISABLED,
                },
                Ordering::Release,
            );
            states.insert(request_id, state);
            return;
        }
        TraceEvent::Resume {
            request_id,
            activation,
        } => {
            let state = ensure_state(store, states, &request_id).await;
            activation.store(
                match state {
                    TraceState::Active => TRACE_ACTIVE,
                    TraceState::Disabled => TRACE_DISABLED,
                },
                Ordering::Release,
            );
            return;
        }
        _ => {}
    }

    if !matches!(
        ensure_state(store, states, &request_id).await,
        TraceState::Active
    ) {
        if finishes_trace {
            states.remove(&request_id);
            buffers.remove(&request_id);
            request_orders.remove(&request_id);
        }
        return;
    }

    let result = match event {
        TraceEvent::Request {
            artifact_type,
            data,
            metadata,
            ..
        } => {
            append_request(
                store,
                request_orders,
                &request_id,
                artifact_type,
                data,
                metadata,
            )
            .await
        }
        TraceEvent::Artifact {
            artifact_type,
            source,
            data,
            metadata,
            ..
        } => {
            store
                .append_cursor_trace_artifact(
                    &request_id,
                    &artifact_type,
                    &source,
                    &data,
                    &metadata,
                )
                .await
        }
        TraceEvent::LinkedBlob {
            artifact_type,
            source,
            blob_id,
            metadata,
            ..
        } => {
            store
                .link_cursor_trace_artifact(
                    &request_id,
                    &artifact_type,
                    &source,
                    &blob_id,
                    &metadata,
                )
                .await
        }
        TraceEvent::ResponseStarted { status, .. } => {
            store.start_cursor_trace_response(&request_id, status).await
        }
        TraceEvent::ResponseChunk { source, data, .. } => {
            let buffer = buffers.entry(request_id.clone()).or_default();
            let dropped = buffer.push(&source, &data);
            if dropped > 0 {
                tracing::warn!(
                    %request_id,
                    dropped,
                    retained = buffer.chunks.len(),
                    "dropping oldest Cursor response chunks to bound the retry buffer"
                );
            }
            if buffer.is_full() {
                flush_one(store, buffers, &request_id).await;
            }
            return;
        }
        TraceEvent::Finish { error, .. } => {
            flush_request_order(store, request_orders, &request_id).await;
            flush_one(store, buffers, &request_id).await;
            store
                .finish_cursor_trace(&request_id, error.as_deref())
                .await
        }
        TraceEvent::Begin { .. } | TraceEvent::Resume { .. } => unreachable!(),
    };
    if let Err(error) = result {
        tracing::warn!(%request_id, %error, "failed to record Cursor trace event");
    }
    if finishes_trace {
        states.remove(&request_id);
        buffers.remove(&request_id);
    }
}

async fn append_request(
    store: &Store,
    request_orders: &mut HashMap<String, RequestOrder>,
    request_id: &str,
    artifact_type: String,
    data: bytes::Bytes,
    metadata: serde_json::Value,
) -> crate::Result<()> {
    let append_seqno = metadata
        .get("append_seqno")
        .and_then(serde_json::Value::as_i64);
    let ordered = artifact_type == "bidi_request"
        && metadata
            .get("accepted")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        && metadata
            .get("route_outcome")
            .and_then(serde_json::Value::as_str)
            == Some("local");
    let Some(append_seqno) = append_seqno.filter(|_| ordered) else {
        return store
            .append_cursor_trace_request(
                request_id,
                &artifact_type,
                "cursor_client",
                &data,
                &metadata,
            )
            .await;
    };

    let request = BufferedRequest {
        artifact_type,
        data,
        metadata,
    };
    let order = request_orders.entry(request_id.to_owned()).or_default();
    if append_seqno < order.next {
        return append_one(store, request_id, &request).await;
    }
    order.hold(append_seqno, request);
    if order.overflowed() {
        // 缺口一直没补上。再等下去整条 trace 的请求都压在这里，所以按现有 seqno 顺序落库；
        // 顺序仍是升序的，只是不再为那个永远不来的 seqno 让路。
        tracing::warn!(
            %request_id,
            pending = order.pending.len(),
            pending_bytes = order.pending_bytes,
            "Cursor request seqno gap is not filling; flushing pending requests in seqno order"
        );
        for request in order.take_all() {
            append_one(store, request_id, &request).await?;
        }
        return Ok(());
    }
    for request in order.drain_ready() {
        append_one(store, request_id, &request).await?;
    }
    Ok(())
}

async fn append_one(
    store: &Store,
    request_id: &str,
    request: &BufferedRequest,
) -> crate::Result<()> {
    store
        .append_cursor_trace_request(
            request_id,
            &request.artifact_type,
            "cursor_client",
            &request.data,
            &request.metadata,
        )
        .await
}

async fn flush_request_order(
    store: &Store,
    request_orders: &mut HashMap<String, RequestOrder>,
    request_id: &str,
) {
    let Some(mut order) = request_orders.remove(request_id) else {
        return;
    };
    for request in order.take_all() {
        if let Err(error) = append_one(store, request_id, &request).await {
            tracing::warn!(%request_id, %error, "failed to flush ordered Cursor request trace");
        }
    }
}

async fn ensure_state(
    store: &Store,
    states: &mut HashMap<String, TraceState>,
    request_id: &str,
) -> TraceState {
    if let Some(state) = states.get(request_id).copied() {
        return state;
    }
    let state = match store.cursor_trace_exists(request_id).await {
        Ok(true) => TraceState::Active,
        Ok(false) => TraceState::Disabled,
        Err(error) => {
            tracing::warn!(%request_id, %error, "failed to resume Cursor trace");
            TraceState::Disabled
        }
    };
    states.insert(request_id.to_owned(), state);
    state
}

async fn flush_one(store: &Store, buffers: &mut HashMap<String, ResponseBuffer>, request_id: &str) {
    let Some(mut buffer) = buffers.remove(request_id) else {
        return;
    };
    if let Err(error) = store
        .add_cursor_trace_response_chunks(request_id, &buffer.chunks)
        .await
    {
        tracing::warn!(%request_id, %error, "failed to flush Cursor response chunks");
        if buffer.retry_or_give_up() {
            buffers.insert(request_id.to_owned(), buffer);
        } else {
            let dropped_bytes: usize = buffer.chunks.iter().map(|chunk| chunk.data.len()).sum();
            tracing::warn!(
                %request_id,
                dropped_chunks = buffer.chunks.len(),
                dropped_bytes,
                "giving up on Cursor response chunks after repeated flush failures"
            );
        }
    }
}

async fn flush_all(store: &Store, buffers: &mut HashMap<String, ResponseBuffer>) {
    let request_ids = buffers.keys().cloned().collect::<Vec<_>>();
    for request_id in request_ids {
        flush_one(store, buffers, &request_id).await;
    }
}

/// 关闭时把还压在各请求重排队列里的工件落库：`flush_request_order` 只在 Finish 与溢出时
/// 走，通道关掉时这些请求没有任何人再来取它们。
async fn flush_all_orders(store: &Store, request_orders: &mut HashMap<String, RequestOrder>) {
    let request_ids = request_orders.keys().cloned().collect::<Vec<_>>();
    for request_id in request_ids {
        flush_request_order(store, request_orders, &request_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffered_request(label: &str) -> BufferedRequest {
        BufferedRequest {
            artifact_type: label.into(),
            data: bytes::Bytes::copy_from_slice(label.as_bytes()),
            metadata: serde_json::json!({}),
        }
    }

    fn ordered_metadata(seqno: i64) -> serde_json::Value {
        serde_json::json!({
            "append_seqno": seqno,
            "accepted": true,
            "route_outcome": "local",
        })
    }

    async fn trace_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        store.set_detailed_logging(true).await.unwrap();
        (directory, store)
    }

    /// seqno 缺一个时后面的请求要等；缺口补上后按序落库。
    #[test]
    fn a_seqno_gap_holds_later_requests_until_the_gap_is_filled() {
        let mut order = RequestOrder::default();
        order.hold(1, buffered_request("one"));
        order.hold(3, buffered_request("three"));

        assert!(
            order.drain_ready().is_empty(),
            "seqno 0 还没到，后面的都得等"
        );
        assert_eq!(order.next, 0);

        order.hold(0, buffered_request("zero"));
        let ready = order
            .drain_ready()
            .into_iter()
            .map(|request| request.artifact_type)
            .collect::<Vec<_>>();
        assert_eq!(ready, ["zero", "one"]);
        assert_eq!(order.next, 2);

        order.hold(2, buffered_request("two"));
        let ready = order
            .drain_ready()
            .into_iter()
            .map(|request| request.artifact_type)
            .collect::<Vec<_>>();
        assert_eq!(ready, ["two", "three"]);
        assert_eq!(order.next, 4);
        assert_eq!(order.pending_bytes, 0, "落库之后不能再记着这些字节");
    }

    /// 缺口一直不补上时不能无限积压：超过上限就放弃重排，把现有的按序吐出来。
    #[test]
    fn an_unfilled_seqno_gap_cannot_pile_up_without_bound() {
        let mut order = RequestOrder::default();
        for seqno in 1..=(MAX_PENDING_REQUESTS as i64 + 1) {
            order.hold(seqno, buffered_request("request"));
        }

        assert!(order.overflowed());
        let flushed = order.take_all();
        assert_eq!(flushed.len(), MAX_PENDING_REQUESTS + 1);
        assert!(!order.overflowed(), "吐空之后不该还判成溢出");
        assert_eq!(
            order.next,
            MAX_PENDING_REQUESTS as i64 + 2,
            "next 要跳过已落库的最大 seqno，迟到的旧 seqno 直接落库"
        );
    }

    /// 真落库那条路径：seqno 0 永远不来，攒过上限后剩下的请求必须已经写进库。
    #[tokio::test]
    async fn a_stuck_seqno_gap_flushes_what_it_has_instead_of_hoarding_the_trace() {
        let (_directory, store) = trace_store().await;
        store
            .start_cursor_trace_if_detailed("request", None, "local_byok", None)
            .await
            .unwrap();
        let mut orders = HashMap::new();

        for seqno in 1..=(MAX_PENDING_REQUESTS as i64 + 1) {
            append_request(
                &store,
                &mut orders,
                "request",
                "bidi_request".into(),
                bytes::Bytes::copy_from_slice(b"body"),
                ordered_metadata(seqno),
            )
            .await
            .unwrap();
        }

        let order = &orders["request"];
        assert!(
            order.pending.is_empty(),
            "超过上限必须立刻按现有顺序落库，不能再压着"
        );
        assert_eq!(order.next, MAX_PENDING_REQUESTS as i64 + 2);
        let written: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cursor_run_trace_artifacts WHERE request_id = ?",
        )
        .bind("request")
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(written, MAX_PENDING_REQUESTS as i64 + 1);
    }

    /// 关闭时还压着重排队列的请求必须落库。以前只 flush 响应缓冲，缺口没补上的请求正文
    /// 随 worker 一起消失（trace 里有响应分片，却没有对应的请求）。
    #[tokio::test]
    async fn closing_the_worker_flushes_requests_still_waiting_for_a_seqno_gap() {
        let (_directory, store) = trace_store().await;
        store
            .start_cursor_trace_if_detailed("request", None, "local_byok", None)
            .await
            .unwrap();
        let mut orders = HashMap::new();

        // seqno 0 一直没来，这两条只能压在队列里。
        for seqno in [1_i64, 2] {
            append_request(
                &store,
                &mut orders,
                "request",
                "bidi_request".into(),
                bytes::Bytes::copy_from_slice(b"body"),
                ordered_metadata(seqno),
            )
            .await
            .unwrap();
        }
        assert_eq!(orders["request"].pending.len(), 2, "缺口没补上就该等着");

        flush_all_orders(&store, &mut orders).await;

        let written: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cursor_run_trace_artifacts WHERE request_id = ?",
        )
        .bind("request")
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(written, 2, "关闭时压着的请求必须落库");
        assert!(orders.is_empty(), "落库之后队列不能还留着");
    }

    /// 写库一直失败时缓冲不能随着分片无限增长：越过硬上限就丢最旧的分片。
    #[test]
    fn the_retry_buffer_drops_the_oldest_chunks_at_its_hard_cap() {
        let mut buffer = ResponseBuffer::default();
        for index in 0..MAX_RETAINED_CHUNKS + 5 {
            let data = bytes::Bytes::from(index.to_string());
            buffer.push("sse", &data);
        }

        assert_eq!(buffer.chunks.len(), MAX_RETAINED_CHUNKS);
        assert_eq!(
            buffer.chunks.first().unwrap().data,
            b"5",
            "丢的是最旧的 5 条"
        );
        assert_eq!(
            buffer.bytes,
            buffer.chunks.iter().map(|c| c.data.len()).sum::<usize>()
        );
    }

    /// 写库一直失败时不再无限重排：连续失败到上限就放弃这个 request_id。
    #[tokio::test]
    async fn repeated_flush_failures_give_up_then_stop_retrying() {
        let (_directory, store) = trace_store().await;
        // 故意不建 cursor_run_traces 行：外键约束会让每次写分片都失败。
        let mut buffers = HashMap::new();
        for _ in 0..MAX_FLUSH_FAILURES - 1 {
            buffers
                .entry("request".to_owned())
                .or_insert_with(ResponseBuffer::default)
                .push("sse", &bytes::Bytes::from_static(b"chunk"));
            flush_one(&store, &mut buffers, "request").await;
            assert!(
                buffers.contains_key("request"),
                "还没到失败上限时要保留重排"
            );
        }

        flush_one(&store, &mut buffers, "request").await;

        assert!(
            !buffers.contains_key("request"),
            "连续失败到上限要放弃，不能每条分片再试一次写库"
        );
    }
}
