//! 搜索模块测试用的本地 HTTP 桩(只在测试构建里存在)。
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use axum::{body::Body, http::StatusCode, response::Response};

/// 一个只回固定响应的本地服务,并记录被请求的次数 —— 用来观察"有没有真的再出网"。
pub(super) struct StubServer {
    address: SocketAddr,
    requests: Arc<AtomicUsize>,
}

impl StubServer {
    pub(super) async fn fixed(
        status: StatusCode,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let app = axum::Router::new().fallback(move || {
            let counted = counted.clone();
            let body = body.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Response::builder()
                    .status(status)
                    .header(axum::http::header::CONTENT_TYPE, content_type)
                    .body(Body::from(body))
                    .unwrap()
            }
        });
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { address, requests }
    }

    pub(super) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub(super) fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}
