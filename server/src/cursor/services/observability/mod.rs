//! Records Cursor request traces without blocking request or runtime paths.

mod event;
mod worker;

use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc,
};

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::store::{BlobId, Store};

use event::{TraceEvent, TRACE_DISABLED, TRACE_UNKNOWN};

const TRACE_QUEUE_CAPACITY: usize = 512;

#[derive(Clone)]
pub struct CursorTraceService {
    sender: mpsc::Sender<TraceEvent>,
}

impl CursorTraceService {
    pub fn new(store: Store) -> Self {
        let (sender, receiver) = mpsc::channel(TRACE_QUEUE_CAPACITY);
        tokio::spawn(worker::run(store, receiver));
        Self { sender }
    }

    pub fn recorder(&self, request_id: &str) -> CursorTraceRecorder {
        CursorTraceRecorder {
            request_id: Arc::from(request_id),
            sender: self.sender.clone(),
            finished: Arc::new(AtomicBool::new(false)),
            activation: Arc::new(AtomicU8::new(TRACE_UNKNOWN)),
        }
    }
}

#[derive(Clone)]
pub struct CursorTraceRecorder {
    request_id: Arc<str>,
    sender: mpsc::Sender<TraceEvent>,
    finished: Arc<AtomicBool>,
    activation: Arc<AtomicU8>,
}

impl CursorTraceRecorder {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn begin(&self, conversation_id: Option<&str>, route: &str, model_id: Option<&str>) {
        self.send_control(TraceEvent::Begin {
            request_id: self.request_id.to_string(),
            activation: self.activation.clone(),
            conversation_id: conversation_id.map(str::to_owned),
            route: route.to_owned(),
            model_id: model_id.map(str::to_owned),
        });
    }

    pub fn resume(&self) {
        self.send_control(TraceEvent::Resume {
            request_id: self.request_id.to_string(),
            activation: self.activation.clone(),
        });
    }

    pub fn request(&self, artifact_type: &str, data: Bytes, metadata: serde_json::Value) {
        self.send(TraceEvent::Request {
            request_id: self.request_id.to_string(),
            artifact_type: artifact_type.to_owned(),
            data,
            metadata,
        });
    }

    pub fn artifact(
        &self,
        artifact_type: &str,
        source: &str,
        data: &[u8],
        metadata: serde_json::Value,
    ) {
        self.send(TraceEvent::Artifact {
            request_id: self.request_id.to_string(),
            artifact_type: artifact_type.to_owned(),
            source: source.to_owned(),
            data: Bytes::copy_from_slice(data),
            metadata,
        });
    }

    pub fn linked_blob(
        &self,
        artifact_type: &str,
        source: &str,
        blob_id: &BlobId,
        metadata: serde_json::Value,
    ) {
        self.send(TraceEvent::LinkedBlob {
            request_id: self.request_id.to_string(),
            artifact_type: artifact_type.to_owned(),
            source: source.to_owned(),
            blob_id: blob_id.clone(),
            metadata,
        });
    }

    pub fn response_started(&self, status: u16) {
        self.send(TraceEvent::ResponseStarted {
            request_id: self.request_id.to_string(),
            status,
        });
    }

    pub fn response_chunk(&self, source: &str, data: Bytes) {
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        self.send(TraceEvent::ResponseChunk {
            request_id: self.request_id.to_string(),
            source: source.to_owned(),
            data,
        });
    }

    pub fn finish(&self, error: Option<&str>) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        self.send_control(TraceEvent::Finish {
            request_id: self.request_id.to_string(),
            error: error.map(str::to_owned),
        });
    }

    /// 遥测事件（请求正文、响应片段、blob 链接）：队列满时丢弃并记一条警告。
    ///
    /// 这些事件量大，丢几条只说明"这次调用的记录少了一段"；把它们排进队列等空位
    /// 反而会把生产者拖住。
    fn send(&self, event: TraceEvent) {
        if self.activation.load(Ordering::Acquire) == TRACE_DISABLED {
            return;
        }
        if let Err(error) = self.sender.try_send(event) {
            tracing::warn!(
                request_id = %self.request_id,
                %error,
                "dropping Cursor trace event"
            );
        }
    }

    /// 生命周期事件（开始、恢复、结束）：队列满时排队等空位，绝不丢弃。
    ///
    /// 丢掉 `Begin` 会让这条调用在调用列表里根本不存在；丢掉 `Finish` 会让它永远停在
    /// "进行中"，并连带把它引用的响应 blob 钉在库里不回收。两者都不是"记录少了点"，
    /// 而是错的状态。
    fn send_control(&self, event: TraceEvent) {
        match self.sender.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(event)) => {
                // 这里不能 `.await`：`finish` 会在非 async 的收尾路径上被调用
                // （见 `CursorTraceRecorder::finish`），所以把"等到空位再发"交给一个
                // 短命任务。队列容量固定，这类事件每个请求最多三条，不会堆积。
                let sender = self.sender.clone();
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            let _ = sender.send(event).await;
                        });
                    }
                    Err(_) => tracing::warn!(
                        request_id = %self.request_id,
                        "no runtime to queue a Cursor trace lifecycle event"
                    ),
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::event::TraceEvent;
    use super::*;

    /// 队列满时遥测片段可以丢，`Begin`/`Finish` 这类生命周期事件不能丢掉：丢了这条
    /// 调用会永远停在"进行中"（或干脆不出现在调用列表里）。容量 1 的队列复现
    /// "生产者跑在消费者前面"那一刻。
    #[tokio::test]
    async fn lifecycle_events_survive_a_full_trace_queue() {
        let (sender, mut receiver) = mpsc::channel(1);
        let service = CursorTraceService { sender };
        let recorder = service.recorder("request");

        recorder.artifact("test", "test", b"payload", serde_json::json!({}));
        // 队列此刻是满的：这条 Finish 必须排队等空位，而不是被丢掉。
        recorder.finish(None);

        let first = receiver.recv().await.unwrap();
        assert!(matches!(first, TraceEvent::Artifact { .. }));
        let finish = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("the finish event must reach the queue")
            .unwrap();
        assert!(matches!(finish, TraceEvent::Finish { .. }));
    }
}
