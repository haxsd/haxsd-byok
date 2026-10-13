//! Provides deterministic provider streams for integration tests.
#![allow(dead_code)]

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use cursor_server::{
    model::{ModelInvocation, ModelRequest},
    provider::{ModelEvent, Provider, ProviderStream},
    Error,
};
use futures_util::{stream, StreamExt};
use tokio_util::sync::CancellationToken;

enum FakeResponse {
    Events(Vec<Result<ModelEvent, Error>>),
    Gated {
        ready: Arc<tokio::sync::Notify>,
        events: Vec<Result<ModelEvent, Error>>,
    },
    Pending,
}

#[derive(Clone, Default)]
pub struct FakeProvider {
    responses: Arc<Mutex<VecDeque<FakeResponse>>>,
    by_conversation: Arc<Mutex<std::collections::HashMap<String, VecDeque<FakeResponse>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl FakeProvider {
    pub fn push(&self, events: Vec<ModelEvent>) {
        self.push_results(events.into_iter().map(Ok).collect());
    }
    /// 按会话排队的响应：多会话测试里两个会话的调用顺序不确定，按会话分发才稳。
    pub fn push_for(&self, conversation_id: &str, events: Vec<ModelEvent>) {
        self.by_conversation
            .lock()
            .unwrap()
            .entry(conversation_id.to_owned())
            .or_default()
            .push_back(FakeResponse::Events(events.into_iter().map(Ok).collect()));
    }
    /// 同 [`push_for`]，但这一轮要等到 `Notify` 被唤醒才产出事件。
    pub fn push_gated_for(
        &self,
        conversation_id: &str,
        events: Vec<ModelEvent>,
    ) -> Arc<tokio::sync::Notify> {
        let ready = Arc::new(tokio::sync::Notify::new());
        self.by_conversation
            .lock()
            .unwrap()
            .entry(conversation_id.to_owned())
            .or_default()
            .push_back(FakeResponse::Gated {
                ready: ready.clone(),
                events: events.into_iter().map(Ok).collect(),
            });
        ready
    }
    pub fn push_results(&self, events: Vec<Result<ModelEvent, Error>>) {
        self.responses
            .lock()
            .unwrap()
            .push_back(FakeResponse::Events(events));
    }
    pub fn push_error(&self, error: Error) {
        self.responses
            .lock()
            .unwrap()
            .push_back(FakeResponse::Events(vec![Err(error)]));
    }
    pub fn push_pending(&self) {
        self.responses
            .lock()
            .unwrap()
            .push_back(FakeResponse::Pending);
    }
    pub fn push_gated(&self, events: Vec<ModelEvent>) -> Arc<tokio::sync::Notify> {
        let ready = Arc::new(tokio::sync::Notify::new());
        self.responses
            .lock()
            .unwrap()
            .push_back(FakeResponse::Gated {
                ready: ready.clone(),
                events: events.into_iter().map(Ok).collect(),
            });
        ready
    }
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for FakeProvider {
    fn stream(
        &self,
        invocation: ModelInvocation,
        _cancellation: CancellationToken,
    ) -> ProviderStream {
        self.requests
            .lock()
            .unwrap()
            .push(invocation.request.clone());
        let per_conversation = self
            .by_conversation
            .lock()
            .unwrap()
            .get_mut(&invocation.conversation_id)
            .and_then(|queue| queue.pop_front());
        let events = per_conversation
            .or_else(|| self.responses.lock().unwrap().pop_front())
            .expect("fake response configured");
        match events {
            FakeResponse::Events(events) => Box::pin(stream::iter(events)),
            FakeResponse::Gated { ready, events } => Box::pin(
                stream::once(async move {
                    ready.notified().await;
                    events
                })
                .flat_map(stream::iter),
            ),
            FakeResponse::Pending => Box::pin(stream::pending()),
        }
    }
}
