//! Subscribes Cursor RunSSE clients to replayable Transport output.
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use bytes::Bytes;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::{
    cursor::{
        protocol::connect::{self, END_STREAM_FLAG},
        services::observability::CursorTraceRecorder,
        transport::{TransportHandle, TransportRegistry},
    },
    Error, Result,
};

pub async fn stream(registry: &TransportRegistry, request_id: &str) -> Result<Response<Body>> {
    // 只读附着：RunSSE 不许造 transport。`wait_route` 判定 Local 与这里取句柄之间没有
    // 原子性——回收任务可能刚好把这条（已经收尾的）条目摘掉，此时新建出来的 transport
    // 没有任何 append、也没有人负责收尾：新 actor 自己握着命令发送端，永远不会结束，
    // 客户端既收不到帧也收不到 EOF，这个 request_id 还会一直占着"有本地 transport"。
    //
    // 已经收尾但还没被回收的条目照旧附着：它还能把这一轮的帧重放一次（客户端是在
    // BidiAppend 之后才开 RunSSE 的，一轮快速失败的 run 会在附着之前就收尾），重放完
    // 订阅通道关闭，客户端拿到 EOF。
    let handle = registry
        .local(request_id)
        .await
        .ok_or_else(|| Error::RunNotFound(request_id.to_owned()))?;
    let receiver = handle.subscribe();
    let trace = handle.trace().cloned();
    if let Some(trace) = &trace {
        trace.response_started(StatusCode::OK.as_u16());
    }
    let body_stream = local_body_stream(receiver, handle, trace);
    let mut response = Response::new(Body::from_stream(body_stream));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert("connect-protocol-version", HeaderValue::from_static("1"));
    Ok(response)
}

fn local_body_stream(
    mut receiver: mpsc::Receiver<Bytes>,
    handle: TransportHandle,
    trace: Option<CursorTraceRecorder>,
) -> impl tokio_stream::Stream<Item = std::result::Result<Bytes, Infallible>> {
    async_stream::stream! {
        let mut guard = LocalRunGuard::new(handle);
        let mut trace = TraceStreamSink::new(trace, "byok_server");
        while let Some(chunk) = receiver.recv().await {
            let terminal = is_end_stream_frame(&chunk);
            trace.chunk(&chunk);
            guard.frame_sent();
            if terminal {
                guard.complete();
                trace.finish(end_stream_error(&chunk));
            }
            yield Ok::<Bytes, Infallible>(chunk);
            if terminal {
                return;
            }
        }
        guard.complete();
        trace.finish(None);
    }
}

fn is_end_stream_frame(frame: &Bytes) -> bool {
    frame
        .first()
        .is_some_and(|flags| flags & END_STREAM_FLAG != 0)
}

fn end_stream_error(frame: &Bytes) -> Option<String> {
    connect::decode_frames(frame)
        .ok()?
        .into_iter()
        .find_map(|(flags, payload)| {
            if flags & END_STREAM_FLAG == 0 {
                return None;
            }
            let value = serde_json::from_slice::<serde_json::Value>(&payload).ok()?;
            let error = value.get("error")?;
            let code = error.get("code").and_then(serde_json::Value::as_str);
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .filter(|message| !message.is_empty());
            Some(match (code, message) {
                (Some(code), Some(message)) => format!("{code}: {message}"),
                (Some(code), None) => code.to_string(),
                (None, Some(message)) => message.to_string(),
                (None, None) => error.to_string(),
            })
        })
}

struct LocalRunGuard {
    handle: TransportHandle,
    completed: bool,
    started: std::time::Instant,
    frames: usize,
}

impl LocalRunGuard {
    fn new(handle: TransportHandle) -> Self {
        Self {
            handle,
            completed: false,
            started: std::time::Instant::now(),
            frames: 0,
        }
    }

    fn frame_sent(&mut self) {
        self.frames += 1;
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for LocalRunGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        // 走到这里就是 Cursor 侧的 Run 流没了：客户端断开、写回失败、或进程被结束。
        // 用户看到的"报错"几乎总是这一刻，而这一刻的现场只在这里拿得到，所以既落日志
        // 也抓一份诊断文件（诊断文件里有 Cursor 的代理配置、系统代理与日志尾部）。
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let request_id = self.handle.request_id().to_owned();
        let conversation_id = self.handle.conversation_id().map(str::to_owned);
        let frames = self.frames;
        tracing::warn!(
            %request_id,
            conversation_id = conversation_id.as_deref().unwrap_or("none"),
            elapsed_ms,
            frames,
            "Cursor Run stream dropped before completion; aborting the run"
        );
        // 抓一份诊断文件：里面有 Cursor 的代理配置、系统代理与最近一段应用日志的尾部，
        // 全是同步读盘。这一跳在 async worker 上（而且是个 `Drop`），不能就地做——丢进
        // 阻塞线程池，收尾的取消与连接清理照常往下走。抓取永不 panic、永不上抛。
        let diagnostics = serde_json::json!({
            "request_id": request_id,
            "conversation_id": conversation_id,
            "elapsed_ms": elapsed_ms,
            "frames_sent": frames,
        });
        tokio::task::spawn_blocking(move || {
            crate::diagnostics::capture("run_stream_dropped", diagnostics);
        });
        let handle = self.handle.clone();
        tokio::spawn(async move {
            handle.disconnect().await;
        });
    }
}

pub async fn upstream(
    registry: TransportRegistry,
    request_id: String,
    generation: u64,
    response: Response<Body>,
    trace: Option<CursorTraceRecorder>,
) -> Response<Body> {
    let (parts, body) = response.into_parts();
    if let Some(trace) = &trace {
        trace.response_started(parts.status.as_u16());
    }
    // 这条 guard 必须在响应体之外就位：官方 run 的 upstream 标记曾经只在"响应体被消费
    // 到流结束"时清理，而 guard 建在生成器内部——客户端在首次 poll 之前就断开（或者
    // 干脆不读）时生成器从未启动，guard 从未存在，标记就永远留在 registry 里：同 id 的
    // 后续 RunSSE 被送去官方，`maintain_storage_if_idle` 的门禁也因为 upstream 非空
    // 永久失效。生成器一旦拿到它，丢弃响应体就等于清理。
    let guard = UpstreamRunGuard {
        registry,
        request_id,
        generation,
    };
    let stream = async_stream::stream! {
        // 下划线前缀只是"不读它"：它活到流的最后一刻。
        let _guard = guard;
        let mut trace = TraceStreamSink::new(trace, "cursor_official");
        let mut body = body.into_data_stream();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(chunk) => {
                    trace.chunk(&chunk);
                    yield Ok::<Bytes, axum::Error>(chunk);
                }
                Err(error) => {
                    trace.finish(Some(error.to_string()));
                    yield Err(error);
                    return;
                }
            }
        }
        trace.finish(None);
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

enum TraceStreamEvent {
    Chunk(Bytes),
    Finish(Option<String>),
}

struct TraceStreamSink {
    sender: Option<mpsc::UnboundedSender<TraceStreamEvent>>,
}

impl TraceStreamSink {
    fn new(trace: Option<CursorTraceRecorder>, source: &'static str) -> Self {
        let Some(trace) = trace else {
            return Self { sender: None };
        };
        let (sender, mut receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                match event {
                    TraceStreamEvent::Chunk(chunk) => {
                        trace.response_chunk(source, chunk);
                    }
                    TraceStreamEvent::Finish(error) => {
                        trace.finish(error.as_deref());
                        return;
                    }
                }
            }
            trace.finish(None);
        });
        Self {
            sender: Some(sender),
        }
    }

    fn chunk(&self, chunk: &Bytes) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(TraceStreamEvent::Chunk(chunk.clone()));
        }
    }

    fn finish(&mut self, error: Option<String>) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(TraceStreamEvent::Finish(error));
        }
    }
}

impl Drop for TraceStreamSink {
    fn drop(&mut self) {
        if self.sender.is_some() {
            self.finish(Some(
                "response stream dropped before completion".to_string(),
            ));
        }
    }
}

struct UpstreamRunGuard {
    registry: TransportRegistry,
    request_id: String,
    generation: u64,
}

impl Drop for UpstreamRunGuard {
    fn drop(&mut self) {
        self.registry
            .finish_upstream(self.request_id.clone(), self.generation);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{cursor::conversation::TransportCommand, cursor::transport::test_registry};

    use super::*;

    /// 等回收任务把已经收尾的条目摘掉。
    async fn wait_until_recycled(registry: &TransportRegistry, request_id: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while registry.local(request_id).await.is_some() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("收尾的条目必须被回收");
    }

    /// 一段不会被当成收尾的输出帧：Connect 帧的首字节是 flags，`END_STREAM_FLAG` 之外的
    /// 位没有含义——随便挑的首字节（`b"first"` 的 `f` = 0x66）可能正好带上这一位，于是
    /// 这条流会在第一帧之后被当成正常收尾。
    fn open_frame(text: &str) -> Bytes {
        let mut bytes = vec![0_u8];
        bytes.extend_from_slice(text.as_bytes());
        Bytes::from(bytes)
    }

    /// 一个真正的终止帧：首字节就是 END_STREAM 标志。
    fn end_stream_frame() -> Bytes {
        Bytes::from_static(&[END_STREAM_FLAG])
    }

    /// RunSSE 只附着、不创建。路由判定 Local 与取句柄之间没有原子性：回收任务摘掉条目
    /// 的那一瞬间，以前的 `get_or_create` 会造出一个没人收尾的僵尸 transport（新 actor
    /// 自己握着命令发送端，永远不会结束），客户端收不到帧也收不到 EOF，这个 request_id
    /// 还永久占着"有本地 transport"。
    #[tokio::test]
    async fn run_sse_attaches_to_an_existing_transport_instead_of_creating_one() {
        let (_directory, registry) = test_registry().await;
        let handle = registry.get_or_create("recycled-request").await.unwrap();
        handle.command(TransportCommand::Disconnect).await.unwrap();
        wait_until_recycled(&registry, "recycled-request").await;

        let error = stream(&registry, "recycled-request")
            .await
            .expect_err("条目已经回收：不许再造一个 transport");
        assert!(matches!(error, Error::RunNotFound(_)), "{error:?}");
        assert!(
            registry.local("recycled-request").await.is_none(),
            "RunSSE 不许留下僵尸条目"
        );
    }

    /// 已经收尾的 transport（终止帧发完、条目还没被回收）：附着仍要把这一轮的帧重放
    /// 一次，然后 EOF。客户端是在 BidiAppend 之后才开 RunSSE 的，一轮快速失败的 run
    /// 会在附着之前就收尾——那时 404 会让客户端丢掉唯一的现场。
    #[tokio::test]
    async fn run_sse_replays_a_transport_that_already_finished() {
        let (_directory, registry) = test_registry().await;
        let handle = registry.get_or_create("finished-request").await.unwrap();
        assert!(handle.emit_frame(end_stream_frame()));
        handle.close_output();

        let response = stream(&registry, "finished-request").await.unwrap();
        let mut body = response.into_body().into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap(), end_stream_frame());
        assert!(body.next().await.is_none(), "重放完就是 EOF");
    }

    /// 活着的 transport：附着时先重放已经发出去的帧，再跟上实时帧。
    #[tokio::test]
    async fn run_sse_replays_what_the_run_already_emitted() {
        let (_directory, registry) = test_registry().await;
        let handle = registry.get_or_create("live-request").await.unwrap();
        assert!(handle.emit_frame(open_frame("first")));

        let response = stream(&registry, "live-request").await.unwrap();
        let mut body = response.into_body().into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap(), open_frame("first"));

        assert!(handle.emit_frame(open_frame("second")));
        assert_eq!(body.next().await.unwrap().unwrap(), open_frame("second"));

        // 收尾让订阅通道关闭：流正常结束，客户端拿到 EOF。
        handle.close_output();
        assert!(body.next().await.is_none());
    }

    /// 官方 run 的 upstream 标记必须随响应体一起消失，而不是等"响应体被消费到流结束"。
    /// 客户端在首次 poll 之前就断开（或者干脆不读）时，生成器从未启动；guard 若建在
    /// 生成器内部就从未存在，标记会永远留在 registry 里挡着存储回收。
    #[tokio::test]
    async fn an_upstream_response_that_is_never_read_still_clears_the_route() {
        let (_directory, registry) = test_registry().await;
        assert_eq!(registry.mark_upstream("never-read").await, 1);
        assert!(registry.upstream("never-read").await);

        let response = upstream(
            registry.clone(),
            "never-read".to_owned(),
            1,
            Response::new(Body::from("body")),
            None,
        )
        .await;
        // 客户端断开：整个响应（含从未被 poll 的响应体）在这里被丢掉。
        drop(response);

        tokio::time::timeout(Duration::from_secs(5), async {
            while registry.upstream("never-read").await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("丢弃响应体就必须把路由标记还回去");
    }
}
