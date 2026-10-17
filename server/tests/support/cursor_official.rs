//! Shared helpers for the Cursor official forwarding tests: a local upstream stub that
//! also works as an HTTP forward proxy, plus router/store plumbing.
#![allow(dead_code)]

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use axum::{
    body::Body,
    http::{header, Request},
    Router,
};
use cursor_server::{
    api::cursor,
    cursor::{
        prompting::{PromptAssets, PromptCompiler},
        transport::TransportRegistry,
    },
    local_app::CursorHarness,
    network::NetworkClients,
    store::{ProxyMode, ProxySettingsInput, Store, TabMode, TabSettings},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

/// 官方 Tab 转发路径里的一条路由（`services::tab` 永远转发，不解码正文）。
pub const TAB_PATH: &str = "/aiserver.v1.AiService/StreamCpp";
const STUB_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\ncontent-type: application/proto\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
/// 桩读一条请求的硬上限：对端连上却不发请求头时必须放弃，不能让任务挂着。
const HEAD_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// 与生产侧同量级的请求体上限；桩只是对端，吃光内存的失败会伪装成用例卡死。
const MAX_BUFFERED_BYTES: usize = 8 * 1024 * 1024;
/// 一次路由调用的上限：被测代码卡住时要报"超时"，而不是让整个用例挂到 CI 超时。
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 同时扮演上游与 HTTP 转发代理的本地桩：固定应答，记下每条连接与它的请求行。
pub struct Stub {
    pub origin: String,
    connections: Arc<AtomicUsize>,
    request_heads: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let request_heads = Arc::new(Mutex::new(Vec::new()));
        let counted = connections.clone();
        let recorded = request_heads.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                let head = read_request_head(&mut socket).await;
                recorded.lock().unwrap().push(head);
                let _ = socket.write_all(STUB_RESPONSE).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            }
        });
        Self {
            origin: format!("http://{address}"),
            connections,
            request_heads,
        }
    }

    pub fn attempts(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// 第 `index` 条连接的请求行（`POST <目标> HTTP/1.1`）。经转发代理时目标是绝对
    /// 地址，直连时是原始路径。
    pub fn request_line(&self, index: usize) -> String {
        self.request_heads.lock().unwrap()[index]
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    }
}

/// 读到请求头末尾，再按 `content-length`（或 chunked 的结束块）把正文读干净再应答：
/// 没读完就关连接会让客户端在写正文时报错，那是桩的毛病而不是被测代码的。
///
/// 超时与缓冲上限是必须的：桩是被测代码的对端，一个连上却不发头的客户端会让这个任务
/// 永远挂着，而无上限的缓冲在用例里也能把内存吃光——两种情况看起来都像"用例卡死"。
async fn read_request_head(socket: &mut tokio::net::TcpStream) -> String {
    read_request_head_within(socket, HEAD_READ_TIMEOUT).await
}

async fn read_request_head_within(
    socket: &mut tokio::net::TcpStream,
    limit: std::time::Duration,
) -> String {
    match tokio::time::timeout(limit, read_request(socket)).await {
        Ok(head) => head,
        Err(_) => String::new(),
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = match socket.read(&mut chunk).await {
            Ok(read) => read,
            Err(_) => return String::new(),
        };
        if read == 0 {
            return String::from_utf8_lossy(&buffer).into_owned();
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_BUFFERED_BYTES {
            return String::from_utf8_lossy(&buffer).into_owned();
        }
        let Some(head_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        if body_is_complete(&head, &buffer[head_end + 4..]) {
            return head;
        }
    }
}

/// 正文读完了吗：`transfer-encoding: chunked` 看结束块（不带 trailers），其余按
/// `content-length`（缺省 0）。
fn body_is_complete(head: &str, body: &[u8]) -> bool {
    if head
        .lines()
        .filter_map(|line| header_value(line, "transfer-encoding"))
        .any(|value| value.trim().eq_ignore_ascii_case("chunked"))
    {
        return body.ends_with(b"0\r\n\r\n");
    }
    let declared = head
        .lines()
        .find_map(|line| header_value(line, "content-length"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    body.len() >= declared
}

fn header_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let (field, value) = line.split_once(':')?;
    field.eq_ignore_ascii_case(name).then_some(value)
}

/// 绑定一个端口再放掉：地址确定，但上面肯定没人监听。
pub async fn dead_address() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address.to_string()
}

pub async fn set_proxy(store: &Store, mode: ProxyMode, address: &str) {
    store
        .set_proxy_settings(ProxySettingsInput {
            mode,
            address: address.into(),
            auth_enabled: false,
            username: String::new(),
            password: None,
        })
        .await
        .unwrap();
}

pub async fn set_tab_service(store: &Store, address: &str) {
    store
        .set_tab_settings(TabSettings {
            mode: TabMode::Custom,
            address: address.into(),
        })
        .await
        .unwrap();
}

pub async fn tab_router(store: Store, clients: NetworkClients) -> Router {
    let assets = PromptAssets::embedded().unwrap();
    let harness = CursorHarness::new(store.clone()).unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(crate::fake_provider::FakeProvider::default()),
        PromptCompiler::new(assets),
    );
    cursor::router(registry, clients, harness).unwrap()
}

pub async fn post_tab(router: Router, body: &str) -> axum::response::Response {
    tokio::time::timeout(
        REQUEST_TIMEOUT,
        router.oneshot(
            Request::post(TAB_PATH)
                .header(header::CONTENT_TYPE, "application/proto")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        ),
    )
    .await
    .expect("tab request did not finish within the test timeout")
    .unwrap()
}

/// 收集测试期间的日志：自愈与重试各留一条，不刷屏。
///
/// 只在"这个进程里只有这一个用例"的测试文件里用：`tracing` 的 callsite 兴趣是按
/// 首次求值缓存的，别的用例先在没有订阅者的线程上触发同一行日志，这一行就会被
/// 跳过——那是测试进程的假象，不是被测代码没记。
#[derive(Clone, Default)]
pub struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogs;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// 测试线程的日志落进 `CapturedLogs`；守卫必须活到断言之后。
pub fn capture_logs(logs: &CapturedLogs) -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish(),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn chunked_bodies_are_only_complete_at_the_terminator() {
        let head = "POST /x HTTP/1.1\r\ntransfer-encoding: chunked\r\n";
        assert!(!body_is_complete(head, b"4\r\nabcd\r\n"));
        assert!(body_is_complete(head, b"4\r\nabcd\r\n0\r\n\r\n"));
    }

    #[test]
    fn content_length_bodies_are_complete_once_fully_buffered() {
        let head = "POST /x HTTP/1.1\r\ncontent-length: 4\r\n";
        assert!(!body_is_complete(head, b"ab"));
        assert!(body_is_complete(head, b"abcd"));
        // 没有正文声明（GET）时头读完就算完整。
        assert!(body_is_complete("GET /x HTTP/1.1\r\n", b""));
    }

    /// 桩必须把 chunked 正文读干净再应答：旧实现只认 content-length，头一到就回包并
    /// 关连接，客户端写到后半段正文时才报错——看起来像被测代码坏了。
    #[tokio::test]
    async fn the_stub_waits_for_a_chunked_body_before_responding() {
        let stub = Stub::start().await;
        let address = stub.origin.trim_start_matches("http://").to_owned();
        let mut client = tokio::net::TcpStream::connect(&address).await.unwrap();
        client
            .write_all(
                b"POST /chunked HTTP/1.1\r\nhost: stub\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        client.write_all(b"4\r\nabcd\r\n").await.unwrap();
        // 给桩足够时间在"只读到一半"时就抢先应答（旧实现会在这里回包）。
        tokio::time::sleep(Duration::from_millis(100)).await;
        client.write_all(b"0\r\n\r\n").await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200 OK"),
            "桩的应答：{}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(stub.request_line(0), "POST /chunked HTTP/1.1");
        assert_eq!(stub.attempts(), 1);
    }

    /// 连上却不发请求头的对端：桩必须按超时放弃，而不是把任务挂到用例结束。
    #[tokio::test]
    async fn a_silent_client_times_out_instead_of_hanging_the_stub() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        let head = read_request_head_within(&mut server, Duration::from_millis(100)).await;

        assert!(head.is_empty(), "没等到请求头就该放弃：{head:?}");
        // 对端还活着，桩只是不再等它。
        client.write_all(b"\r\n").await.unwrap();
    }
}
