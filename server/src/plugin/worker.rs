//! Runs one long-lived, sandboxed Deno process per active plugin.
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;

use super::{
    catalog::PluginEntry,
    definition::{file_url, PluginDefinitionLoader},
    protocol::{HostMessage, WorkerMessage},
};
use crate::{provider::CallRecorder, store::Store, Error, Result};

const INVOCATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// 连续多少次一元调用超时就把 worker 换掉。
///
/// 单次超时只是那一次失败（上游慢、网络抖），但真卡死的 worker（插件侧同步死循环会把
/// Deno 事件循环连同 stdin 一起停住）此后每次调用都会超时：攒够这个数就 kill，下一次
/// 调用重新 spawn。健康 worker 只要回一次结果就重新计数。
const MAX_CONSECUTIVE_TIMEOUTS: usize = 2;
const MAX_NETWORK_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_STREAM_BYTES: u64 = 256 * 1024 * 1024;

/// 一次流式调用的输出:零或多个事件,然后恰好一个最终结果。
#[derive(Debug)]
pub enum WorkerStreamItem {
    Event(serde_json::Value),
    Result(Result<serde_json::Value>),
}

type Pending = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<WorkerStreamItem>>>>;
type StreamLines = Arc<Mutex<mpsc::Receiver<Result<String>>>>;

#[derive(Clone)]
pub struct PluginWorker {
    inner: Arc<PluginWorkerInner>,
}

struct PluginWorkerInner {
    plugin_id: String,
    executable: PathBuf,
    directory: PathBuf,
    entry: PathBuf,
    loader: PluginDefinitionLoader,
    host: HostContext,
    process: Mutex<Option<WorkerProcess>>,
    pending: Pending,
}

/// 连续超时的计数（见 [`MAX_CONSECUTIVE_TIMEOUTS`]）。
#[derive(Default)]
struct TimeoutTracker {
    consecutive: std::sync::atomic::AtomicUsize,
}

impl TimeoutTracker {
    /// worker 回了一次结果：它又活了，重新计数。
    fn succeeded(&self) {
        self.consecutive.store(0, Ordering::Release);
    }

    /// 记一次超时。返回 true 表示该把这个 worker 换掉了。
    fn timed_out(&self) -> bool {
        self.consecutive.fetch_add(1, Ordering::AcqRel) + 1 >= MAX_CONSECUTIVE_TIMEOUTS
    }
}

struct WorkerProcess {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
}

struct InvocationState {
    cancellation: CancellationToken,
    recorder: Option<CallRecorder>,
    recorder_claimed: AtomicBool,
}

impl InvocationState {
    fn claim_recorder(&self) -> Option<CallRecorder> {
        self.recorder.as_ref().and_then(|recorder| {
            self.recorder_claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .ok()
                .map(|_| recorder.clone())
        })
    }
}

#[derive(Clone)]
struct HostContext {
    plugin_id: String,
    network_hosts: Arc<HashSet<String>>,
    store: Store,
    invocations: Arc<Mutex<HashMap<String, Arc<InvocationState>>>>,
    streams: Arc<Mutex<HashMap<String, StreamLines>>>,
    /// 连续超时计数：worker 回结果时清零，`invoke` 超时时累加。
    timeouts: Arc<TimeoutTracker>,
    http: Arc<PluginHttpClient>,
}

/// 插件宿主网络请求共用的客户端。
///
/// 以前每次 `network.fetch` / `stream.open` 都 `build()` 一个新的 `reqwest::Client`：连接池、
/// TLS 会话与 DNS 缓存随请求一起丢掉，而插件里高频的是 OAuth 轮询与目录同步，等于每次
/// 都重新握手。这里复用同一份，只在出网路由真的变了（用户改了代理设置，或系统代理中途
/// 消失）或者连接类失败之后重建——判据与 provider 路径的 `OutboundClient` 一致。
///
/// 没有直接用 `NetworkClients` 的缓存槽：那几份客户端的重定向策略与超时是按各自调用方
/// 定的，而插件这条路必须 `redirect(none)`——白名单只校验第一个 URL，跟随重定向就可能
/// 落到未声明的站点上。
struct PluginHttpClient {
    store: Store,
    clients: crate::network::NetworkClients,
    cached: Mutex<Option<Arc<reqwest::Client>>>,
}

impl PluginHttpClient {
    fn new(store: Store) -> Self {
        Self {
            clients: crate::network::NetworkClients::new(store.clone()),
            store,
            cached: Mutex::new(None),
        }
    }

    async fn get(&self) -> Result<Arc<reqwest::Client>> {
        if let Some(client) = self.cached.lock().await.clone() {
            if !self.clients.outbound_route_changed().await {
                return Ok(client);
            }
            tracing::debug!("plugin outbound route changed; rebuilding the host network client");
        }
        let client = Arc::new(plugin_http_client(&self.store).await?);
        *self.cached.lock().await = Some(client.clone());
        Ok(client)
    }

    /// 连接类失败后的兜底：按当前配置重建一份，交给调用方重试一次。
    async fn rebuild(&self) -> Option<Arc<reqwest::Client>> {
        self.clients.invalidate().await;
        match plugin_http_client(&self.store).await {
            Ok(client) => {
                let client = Arc::new(client);
                *self.cached.lock().await = Some(client.clone());
                Some(client)
            }
            Err(error) => {
                tracing::warn!(%error, "failed to rebuild the plugin host network client");
                None
            }
        }
    }
}

async fn plugin_http_client(store: &Store) -> Result<reqwest::Client> {
    Ok(crate::network::client_builder(store)
        .await?
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .build()?)
}

impl PluginWorker {
    pub fn new(
        plugin: &PluginEntry,
        executable: PathBuf,
        loader: PluginDefinitionLoader,
        store: Store,
    ) -> Self {
        let plugin_id = plugin.manifest.id.clone();
        Self {
            inner: Arc::new(PluginWorkerInner {
                host: HostContext {
                    plugin_id: plugin_id.clone(),
                    network_hosts: Arc::new(
                        plugin
                            .manifest
                            .permissions
                            .network
                            .iter()
                            .map(|host| host.to_ascii_lowercase())
                            .collect(),
                    ),
                    store: store.clone(),
                    invocations: Arc::new(Mutex::new(HashMap::new())),
                    streams: Arc::new(Mutex::new(HashMap::new())),
                    timeouts: Arc::new(TimeoutTracker::default()),
                    http: Arc::new(PluginHttpClient::new(store)),
                },
                plugin_id,
                executable,
                directory: plugin.directory.clone(),
                entry: plugin.entry.clone(),
                loader,
                process: Mutex::new(None),
                pending: Arc::new(Mutex::new(HashMap::new())),
            }),
        }
    }

    /// 一元调用:忽略事件,等待最终结果,受统一超时约束。
    pub async fn invoke(
        &self,
        method: &str,
        params: serde_json::Value,
        cancellation: CancellationToken,
    ) -> Result<serde_json::Value> {
        let mut items = self
            .invoke_streaming(method, params, cancellation, None)
            .await?;
        let result = tokio::time::timeout(INVOCATION_TIMEOUT, async {
            while let Some(item) = items.recv().await {
                if let WorkerStreamItem::Result(result) = item {
                    return result;
                }
            }
            Err(Error::Provider(format!(
                "plugin '{}' worker stopped",
                self.inner.plugin_id
            )))
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                // 连续超时到上限就换 worker：卡死的进程不会自己好，留着它每次调用
                // 都要再等满 10 分钟（插件列表因此整个卡住）。
                if self.inner.host.timeouts.timed_out() {
                    tracing::warn!(
                        plugin = %self.inner.plugin_id,
                        "restarting plugin worker after repeated invocation timeouts"
                    );
                    self.stop().await;
                }
                Err(Error::Provider(format!(
                    "plugin '{}' invocation timed out",
                    self.inner.plugin_id
                )))
            }
        }
    }

    /// 流式调用:事件按序转发,最终以恰好一个 Result 收尾。
    /// 取消通过传入的令牌传播到 Worker 与其挂起的宿主网络请求。
    pub async fn invoke_streaming(
        &self,
        method: &str,
        params: serde_json::Value,
        cancellation: CancellationToken,
        recorder: Option<CallRecorder>,
    ) -> Result<mpsc::UnboundedReceiver<WorkerStreamItem>> {
        let id = uuid::Uuid::new_v4().to_string();
        let request_cancellation = CancellationToken::new();
        self.inner.host.invocations.lock().await.insert(
            id.clone(),
            Arc::new(InvocationState {
                cancellation: request_cancellation.clone(),
                recorder,
                recorder_claimed: AtomicBool::new(false),
            }),
        );
        let (sender, receiver) = mpsc::unbounded_channel();
        self.inner
            .pending
            .lock()
            .await
            .insert(id.clone(), sender.clone());
        let send_result = async {
            let stdin = self.stdin().await?;
            write_message(
                &stdin,
                &HostMessage::Request {
                    id: &id,
                    method,
                    params: &params,
                },
            )
            .await
        }
        .await;
        if let Err(error) = send_result {
            self.cleanup(&id).await;
            return Err(error);
        }

        // 取消监视:通知 Worker,同时中止该请求挂起的宿主网络调用。
        let inner = self.inner.clone();
        let request_id = id.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    cancel_invocation(
                        &inner.host.invocations,
                        &inner.process,
                        &request_id,
                    )
                    .await;
                    let _ = sender.send(WorkerStreamItem::Result(Err(Error::Cancelled)));
                    inner.pending.lock().await.remove(&request_id);
                }
                _ = sender.closed() => {
                    // 消费者放弃了这条流（run 被丢弃、空闲超时、提交信息提前返回……）：
                    // 光删记录不够，得把取消传下去，否则 Deno 的 dispatch 与宿主 reqwest
                    // 连接会一直跑到上游自己结束。正常收尾后再丢接收端是常态，
                    // 那时记录已经被 worker 的结果清掉了，这里什么也不做。
                    cancel_invocation(
                        &inner.host.invocations,
                        &inner.process,
                        &request_id,
                    )
                    .await;
                    inner.pending.lock().await.remove(&request_id);
                }
            }
        });
        Ok(receiver)
    }

    pub async fn stop(&self) {
        if let Some(mut process) = self.inner.process.lock().await.take() {
            let _ = process.child.kill().await;
        }
        fail_pending(&self.inner.pending, "plugin worker stopped").await;
    }

    async fn cleanup(&self, id: &str) {
        self.inner.pending.lock().await.remove(id);
        self.inner.host.invocations.lock().await.remove(id);
    }

    async fn stdin(&self) -> Result<Arc<Mutex<ChildStdin>>> {
        let mut process = self.inner.process.lock().await;
        let dead = match process.as_mut() {
            Some(current) => current
                .child
                .try_wait()
                .map_err(|error| {
                    Error::Config(format!("cannot check plugin worker status: {error}"))
                })?
                .is_some(),
            None => true,
        };
        if dead {
            *process = Some(self.spawn().await?);
        }
        Ok(process
            .as_ref()
            .expect("plugin worker was started")
            .stdin
            .clone())
    }

    async fn spawn(&self) -> Result<WorkerProcess> {
        let entry_url = file_url(&self.inner.entry)?;
        let mut command = tokio::process::Command::new(&self.inner.executable);
        super::detach_console(&mut command);
        command
            .arg("run")
            .arg("--quiet")
            .arg("--no-config")
            .arg("--no-lock")
            .arg("--no-npm")
            .arg("--no-remote")
            .arg("--no-prompt")
            .arg(format!("--allow-read={}", self.inner.directory.display()))
            .arg(format!(
                "--allow-read={}",
                self.inner.loader.sdk_dir().display()
            ))
            .arg(format!(
                "--import-map={}",
                self.inner.loader.import_map().display()
            ))
            .arg(self.inner.loader.worker_path())
            .arg(entry_url.as_str())
            .env("DENO_DIR", self.inner.loader.deno_dir())
            .env("DENO_NO_UPDATE_CHECK", "1")
            .current_dir(&self.inner.directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            Error::Config(format!(
                "cannot start plugin worker {}: {error}",
                self.inner.executable.display()
            ))
        })?;
        let stdin =
            Arc::new(Mutex::new(child.stdin.take().ok_or_else(|| {
                Error::Config("cannot open plugin worker stdin".into())
            })?));
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Config("cannot open plugin worker stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Config("cannot open plugin worker stderr".into()))?;
        spawn_stdout_reader(
            self.inner.plugin_id.clone(),
            stdout,
            stdin.clone(),
            self.inner.pending.clone(),
            self.inner.host.clone(),
        );
        spawn_stderr_reader(self.inner.plugin_id.clone(), stderr);
        Ok(WorkerProcess { child, stdin })
    }
}

fn spawn_stdout_reader(
    plugin_id: String,
    stdout: tokio::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    host: HostContext,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let message = match serde_json::from_str::<WorkerMessage>(&line) {
                Ok(message) => message,
                Err(error) => {
                    tracing::warn!(plugin = %plugin_id, %error, "plugin worker wrote an invalid message");
                    continue;
                }
            };
            match message {
                WorkerMessage::Result { id, result, error } => {
                    // 结果到手就清掉这条调用的记录：它同时是"还在飞"的判据——
                    // 消费者之后丢掉接收端不该再被当成放弃了一次在飞调用。
                    host.invocations.lock().await.remove(&id);
                    host.timeouts.succeeded();
                    if let Some(sender) = pending.lock().await.remove(&id) {
                        let value = match error {
                            Some(error) => {
                                Err(Error::Provider(format!("plugin '{plugin_id}': {error}")))
                            }
                            None => Ok(result),
                        };
                        let _ = sender.send(WorkerStreamItem::Result(value));
                    }
                }
                WorkerMessage::Event { id, event } => {
                    if let Some(sender) = pending.lock().await.get(&id) {
                        let _ = sender.send(WorkerStreamItem::Event(event));
                    }
                }
                WorkerMessage::HostCall {
                    id,
                    request_id,
                    method,
                    params,
                } => {
                    let host = host.clone();
                    let stdin = stdin.clone();
                    tokio::spawn(async move {
                        let result = host.call(&request_id, &method, params).await;
                        match result {
                            Ok(result) => {
                                let _ = write_message(
                                    &stdin,
                                    &HostMessage::HostResult {
                                        id: &id,
                                        result: &result,
                                    },
                                )
                                .await;
                            }
                            Err(error) => {
                                let text = error.to_string();
                                let _ = write_message(
                                    &stdin,
                                    &HostMessage::HostError {
                                        id: &id,
                                        error: &text,
                                    },
                                )
                                .await;
                            }
                        }
                    });
                }
            }
        }
        fail_pending(&pending, &format!("plugin '{plugin_id}' worker exited")).await;
    });
}

fn spawn_stderr_reader(plugin_id: String, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::warn!(plugin = %plugin_id, message = %line, "plugin worker stderr");
        }
    });
}

async fn write_message(stdin: &Arc<Mutex<ChildStdin>>, message: &HostMessage<'_>) -> Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(&bytes)
        .await
        .map_err(|error| Error::Config(format!("cannot write to plugin worker: {error}")))?;
    stdin
        .flush()
        .await
        .map_err(|error| Error::Config(format!("cannot flush plugin worker stdin: {error}")))?;
    Ok(())
}

/// 取消一次还在飞的调用：通知 Worker（`cancel` 会让 Deno 侧 abort 那个 AbortController），
/// 并取消这条调用挂起的宿主网络调用（`fetch` / `stream.open` / 读循环都只认这个令牌）。
///
/// 返回 false 表示这条调用已经结束（`Result` 已经把它清掉）。那时再发 `cancel` 只是噪音：
/// worker 侧 `controllers` 早就在 `finally` 里删了，收到也是空操作。
async fn cancel_invocation(
    invocations: &Arc<Mutex<HashMap<String, Arc<InvocationState>>>>,
    process: &Mutex<Option<WorkerProcess>>,
    request_id: &str,
) -> bool {
    let Some(state) = invocations.lock().await.remove(request_id) else {
        return false;
    };
    state.cancellation.cancel();
    if let Some(process) = process.lock().await.as_ref() {
        let _ = write_message(&process.stdin, &HostMessage::Cancel { id: request_id }).await;
    }
    true
}

async fn fail_pending(pending: &Pending, message: &str) {
    for (_, sender) in std::mem::take(&mut *pending.lock().await) {
        let _ = sender.send(WorkerStreamItem::Result(Err(Error::Provider(
            message.into(),
        ))));
    }
}

fn recorded_network_request(
    params: &serde_json::Value,
) -> Result<(serde_json::Value, serde_json::Value)> {
    let mut recorded_headers = serde_json::Map::new();
    if let Some(headers) = params.get("headers").and_then(serde_json::Value::as_object) {
        for (name, value) in headers {
            let value = value.as_str().ok_or_else(|| {
                Error::Config(format!("plugin HTTP header '{name}' must be a string"))
            })?;
            if !crate::model::is_sensitive_header(name) {
                recorded_headers.insert(name.clone(), value.into());
            }
        }
    }
    let body = params
        .get("body")
        .and_then(serde_json::Value::as_str)
        .map(|body| serde_json::from_str(body).unwrap_or_else(|_| body.into()))
        .unwrap_or(serde_json::Value::Null);
    Ok((serde_json::Value::Object(recorded_headers), body))
}

impl HostContext {
    async fn call(
        &self,
        request_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match method {
            "network.fetch" => self.fetch(request_id, params).await,
            "network.stream.open" => self.stream_open(request_id, params).await,
            "network.stream.read" => self.stream_read(params).await,
            "network.stream.close" => {
                self.streams
                    .lock()
                    .await
                    .remove(required_string(&params, "streamId")?);
                Ok(serde_json::Value::Null)
            }
            _ => Err(Error::Protocol(format!(
                "unsupported plugin host method: {method}"
            ))),
        }
    }

    /// 只装配请求：校验 URL、白名单、方法与请求头，取复用的客户端。记录调用工件由
    /// `request` 负责，所以连接失败后的重发可以直接复用这一步而不重复记一条请求。
    async fn build_request(&self, params: &serde_json::Value) -> Result<reqwest::RequestBuilder> {
        let raw_url = required_string(params, "url")?;
        let url = url::Url::parse(raw_url)
            .map_err(|error| Error::Config(format!("invalid plugin network URL: {error}")))?;
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return Err(Error::Config(
                "plugin network URL must be HTTPS without credentials".into(),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Error::Config("plugin network URL has no host".into()))?
            .to_ascii_lowercase();
        if !self.network_hosts.contains(&host) {
            return Err(Error::Config(format!(
                "plugin '{}' cannot access host '{host}'",
                self.plugin_id
            )));
        }
        let method = params
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("GET")
            .parse::<reqwest::Method>()
            .map_err(|error| Error::Config(format!("invalid plugin HTTP method: {error}")))?;
        let client = self.http.get().await?;
        let mut request = client.request(method, url);
        if let Some(headers) = params.get("headers").and_then(serde_json::Value::as_object) {
            for (name, value) in headers {
                let value = value.as_str().ok_or_else(|| {
                    Error::Config(format!("plugin HTTP header '{name}' must be a string"))
                })?;
                request = request.header(name, value);
            }
        }
        if let Some(body) = params.get("body").and_then(serde_json::Value::as_str) {
            request = request.body(body.to_owned());
        }
        Ok(request)
    }

    async fn request(
        &self,
        request_id: &str,
        params: &serde_json::Value,
    ) -> Result<(
        reqwest::RequestBuilder,
        CancellationToken,
        Option<CallRecorder>,
    )> {
        let request = self.build_request(params).await?;
        let invocation = self.invocations.lock().await.get(request_id).cloned();
        let cancellation = invocation
            .as_ref()
            .map(|state| state.cancellation.clone())
            .unwrap_or_default();
        let recorder = invocation.and_then(|state| state.claim_recorder());
        if let Some(recorder) = &recorder {
            let (headers, body) = recorded_network_request(params)?;
            recorder.request(headers, &body).await?;
        }
        Ok((request, cancellation, recorder))
    }

    async fn fetch(
        &self,
        request_id: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let (request, cancellation, recorder) = self.request(request_id, &params).await?;
        let response = match tokio::select! {
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            response = request.timeout(Duration::from_secs(60)).send() => response,
        } {
            Ok(response) => response,
            // 复用客户端的代价：代理中途停掉时手里这份还指向已经没人监听的端口。连接类
            // 失败按当前配置重建一次再发一遍；请求级超时不重发（那次请求可能已经送到并
            // 被插件当成写操作处理过了）。
            Err(error) if error.is_connect() => {
                if self.http.rebuild().await.is_none() {
                    return Err(error.into());
                }
                let retry = self.build_request(&params).await?;
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(Error::Cancelled),
                    response = retry.timeout(Duration::from_secs(60)).send() => response?,
                }
            }
            Err(error) => return Err(error.into()),
        };
        let status = response.status().as_u16();
        if let Some(recorder) = &recorder {
            recorder.response_headers(status).await?;
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_NETWORK_RESPONSE_BYTES)
        {
            return Err(Error::Provider(
                "plugin network response is larger than allowed".into(),
            ));
        }
        let headers = header_map(&response);
        let mut response = response;
        let body = collect_bounded_body(&mut response, &cancellation).await?;
        if let Some(recorder) = &recorder {
            recorder.response_chunk(&body).await?;
        }
        Ok(
            serde_json::json!({ "status": status, "headers": headers, "body": String::from_utf8_lossy(&body) }),
        )
    }

    /// 打开流式响应:立即返回状态与响应头,响应体按行经 stream.read 拉取。
    async fn stream_open(
        &self,
        request_id: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let (request, cancellation, recorder) = self.request(request_id, &params).await?;
        let response = tokio::select! {
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            response = request.send() => response?,
        };
        let status = response.status().as_u16();
        if let Some(recorder) = &recorder {
            recorder.response_headers(status).await?;
        }
        let headers = header_map(&response);
        let (sender, receiver) = mpsc::channel::<Result<String>>(256);
        tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut body = response.bytes_stream();
            let mut buffered = Vec::<u8>::new();
            let mut total = 0_u64;
            loop {
                let chunk = tokio::select! {
                    _ = cancellation.cancelled() => {
                        let _ = sender.send(Err(Error::Cancelled)).await;
                        return;
                    }
                    chunk = body.next() => chunk,
                };
                let Some(chunk) = chunk else { break };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        let _ = sender.send(Err(Error::from(error))).await;
                        return;
                    }
                };
                total += chunk.len() as u64;
                if total > MAX_STREAM_BYTES {
                    let _ = sender
                        .send(Err(Error::Provider(
                            "plugin network stream is larger than allowed".into(),
                        )))
                        .await;
                    return;
                }
                if let Some(recorder) = &recorder {
                    if let Err(error) = recorder.response_chunk(&chunk).await {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                }
                buffered.extend_from_slice(&chunk);
                while let Some(position) = buffered.iter().position(|byte| *byte == b'\n') {
                    let mut line = buffered.drain(..=position).collect::<Vec<u8>>();
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    if sender
                        .send(Ok(String::from_utf8_lossy(&line).into_owned()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            if !buffered.is_empty() {
                let _ = sender
                    .send(Ok(String::from_utf8_lossy(&buffered).into_owned()))
                    .await;
            }
        });
        let stream_id = uuid::Uuid::new_v4().to_string();
        self.streams
            .lock()
            .await
            .insert(stream_id.clone(), Arc::new(Mutex::new(receiver)));
        Ok(serde_json::json!({
            "streamId": stream_id,
            "status": status,
            "headers": headers,
        }))
    }

    async fn stream_read(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let stream_id = required_string(&params, "streamId")?;
        let lines_handle = self
            .streams
            .lock()
            .await
            .get(stream_id)
            .cloned()
            .ok_or_else(|| Error::Protocol(format!("unknown plugin stream: {stream_id}")))?;
        let mut receiver = lines_handle.lock().await;
        let mut lines = Vec::new();
        match receiver.recv().await {
            Some(Ok(line)) => lines.push(line),
            Some(Err(error)) => {
                drop(receiver);
                self.streams.lock().await.remove(stream_id);
                return Err(error);
            }
            None => {
                drop(receiver);
                self.streams.lock().await.remove(stream_id);
                return Ok(serde_json::json!({ "lines": [], "done": true }));
            }
        }
        // 把已就绪的行一并带走,减少往返。
        while lines.len() < 256 {
            match receiver.try_recv() {
                Ok(Ok(line)) => lines.push(line),
                Ok(Err(error)) => {
                    drop(receiver);
                    self.streams.lock().await.remove(stream_id);
                    return Err(error);
                }
                Err(_) => break,
            }
        }
        Ok(serde_json::json!({ "lines": lines, "done": false }))
    }
}

/// 边收边判地把响应体收进内存，超过上限立刻失败。
///
/// 以前是先 `response.bytes()` 整份读进来再判长度：chunked 或压缩（被解码后
/// `content_length()` 为 `None`）的响应绕得开预检，一份十几 MB 的正文会先整个落在内存里。
/// 预检仍然保留，它是最便宜的快速路径。
async fn collect_bounded_body(
    response: &mut reqwest::Response,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            chunk = response.chunk() => chunk?,
        };
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len() as u64 + chunk.len() as u64 > MAX_NETWORK_RESPONSE_BYTES {
            return Err(Error::Provider(
                "plugin network response is larger than allowed".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
}

fn header_map(response: &reqwest::Response) -> std::collections::BTreeMap<String, String> {
    response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), value.to_string()))
        })
        .collect()
}

fn required_string<'a>(params: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::Protocol(format!("plugin host call requires string '{key}'")))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{
        model::{NewLlmCall, ProviderType},
        provider::{CallRecorder, FinishReason},
        store::Store,
    };

    use super::*;

    async fn recorder(detailed: bool, call_id: &str) -> (tempfile::TempDir, Store, CallRecorder) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        store.set_detailed_logging(detailed).await.unwrap();
        let recorder = CallRecorder::start(
            store.clone(),
            NewLlmCall {
                client_source: crate::model::CallClient::Internal,
                call_id: call_id.into(),
                run_id: "run".into(),
                conversation_id: "conversation".into(),
                provider_call_index: 0,
                model_hash: "plugin:test/provider/model".into(),
                provider_type: ProviderType::Plugin,
                provider_url: "plugin://test/provider".into(),
                request_type: ProviderType::Plugin,
                request_url: "plugin://test/provider".into(),
                model_id: "model".into(),
                display_name: "Model".into(),
                reasoning_effort: None,
                fast: false,
                message_count: 1,
                tool_count: 0,
                detailed: false,
            },
        )
        .await
        .unwrap();
        (directory, store, recorder)
    }

    fn network_params() -> serde_json::Value {
        serde_json::json!({
            "url": "https://example.com/v1/responses",
            "method": "POST",
            "headers": {
                "Authorization": "Bearer secret",
                "X-Api-Key": "secret-key",
                "Cookie": "session=secret",
                "content-type": "application/json",
                "x-client-request-id": "request-1"
            },
            "body": "{\"model\":\"test\",\"stream\":true}"
        })
    }

    async fn host_with_recorder(store: Store, recorder: CallRecorder) -> HostContext {
        let http = Arc::new(PluginHttpClient::new(store.clone()));
        let invocations = Arc::new(Mutex::new(HashMap::new()));
        invocations.lock().await.insert(
            "invocation".into(),
            Arc::new(InvocationState {
                cancellation: CancellationToken::new(),
                recorder: Some(recorder),
                recorder_claimed: AtomicBool::new(false),
            }),
        );
        HostContext {
            plugin_id: "test".into(),
            network_hosts: Arc::new(HashSet::from(["example.com".into()])),
            store,
            invocations,
            streams: Arc::new(Mutex::new(HashMap::new())),
            timeouts: Arc::new(TimeoutTracker::default()),
            http,
        }
    }

    #[test]
    fn recorded_plugin_request_omits_sensitive_headers() {
        let (headers, body) = recorded_network_request(&network_params()).unwrap();

        assert_eq!(
            headers,
            serde_json::json!({
                "content-type": "application/json",
                "x-client-request-id": "request-1"
            })
        );
        assert_eq!(body, serde_json::json!({ "model": "test", "stream": true }));
    }

    #[tokio::test]
    async fn detailed_plugin_network_recording_persists_request_and_raw_response() {
        let (_directory, store, recorder) = recorder(true, "detailed-plugin").await;
        let host = host_with_recorder(store.clone(), recorder.clone()).await;
        let params = network_params();
        let (_, _, first_recorder) = host.request("invocation", &params).await.unwrap();
        let (_, _, second_recorder) = host.request("invocation", &params).await.unwrap();
        let (_, body) = recorded_network_request(&params).unwrap();

        assert!(first_recorder.is_some());
        assert!(second_recorder.is_none());
        recorder.response_headers(200).await.unwrap();
        recorder
            .response_chunk(b"data: {\"type\":\"response.created\"}\n\n")
            .await
            .unwrap();
        recorder.response_chunk(b"data: [DONE]\n\n").await.unwrap();
        recorder.completed(FinishReason::Stop).await.unwrap();

        let request = store
            .llm_call_request("detailed-plugin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request.headers,
            serde_json::json!({
                "content-type": "application/json",
                "x-client-request-id": "request-1"
            })
        );
        assert_eq!(request.body, body);
        let chunks = store.llm_call_chunks("detailed-plugin").await.unwrap();
        let expected_response = "data: {\"type\":\"response.created\"}\n\ndata: [DONE]\n\n";
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.data.as_str())
                .collect::<String>(),
            expected_response
        );
        let summary = store.llm_call("detailed-plugin").await.unwrap().unwrap();
        assert_eq!(summary.http_status, Some(200));
        assert_eq!(summary.stream_event_count, 2);
        assert_eq!(summary.response_bytes, expected_response.len() as i64);
        assert!(summary.detailed);
    }

    #[tokio::test]
    async fn standard_plugin_network_recording_keeps_metrics_without_payloads() {
        let (_directory, store, recorder) = recorder(false, "standard-plugin").await;
        let host = host_with_recorder(store.clone(), recorder.clone()).await;
        let params = network_params();
        let (_, body) = recorded_network_request(&params).unwrap();
        let request_bytes = serde_json::to_string(&body).unwrap().len() as i64;
        let response = b"data: [DONE]\n\n";

        let (_, _, observed) = host.request("invocation", &params).await.unwrap();
        assert!(observed.is_some());
        recorder.response_headers(204).await.unwrap();
        recorder.response_chunk(response).await.unwrap();
        recorder.completed(FinishReason::Stop).await.unwrap();

        assert!(store
            .llm_call_request("standard-plugin")
            .await
            .unwrap()
            .is_none());
        assert!(store
            .llm_call_chunks("standard-plugin")
            .await
            .unwrap()
            .is_empty());
        let summary = store.llm_call("standard-plugin").await.unwrap().unwrap();
        assert_eq!(summary.http_status, Some(204));
        assert_eq!(summary.request_bytes, Some(request_bytes));
        assert_eq!(summary.response_bytes, response.len() as i64);
        assert_eq!(summary.stream_event_count, 1);
        assert!(!summary.detailed);
    }

    fn invocation() -> (
        Arc<Mutex<HashMap<String, Arc<InvocationState>>>>,
        CancellationToken,
    ) {
        let invocations = Arc::new(Mutex::new(HashMap::new()));
        let cancellation = CancellationToken::new();
        (invocations, cancellation)
    }

    /// 消费者放弃流时（run 被丢弃、空闲超时、上游提前返回）必须把取消传下去：
    /// 只删记录的话，Deno 的 dispatch 与宿主 reqwest 连接会一直跑到上游自己结束。
    #[tokio::test]
    async fn abandoning_a_live_invocation_cancels_it_and_clears_the_record() {
        let (invocations, cancellation) = invocation();
        invocations.lock().await.insert(
            "request".into(),
            Arc::new(InvocationState {
                cancellation: cancellation.clone(),
                recorder: None,
                recorder_claimed: AtomicBool::new(false),
            }),
        );
        let process = Mutex::new(None);

        assert!(
            cancel_invocation(&invocations, &process, "request").await,
            "还在飞的调用要通知 worker"
        );
        assert!(cancellation.is_cancelled(), "宿主网络调用要跟着停");
        assert!(invocations.lock().await.is_empty());
    }

    /// 正常收尾之后再丢接收端是常态：那时记录已经被 worker 的结果清掉，
    /// 不该再往 worker 写一条 cancel。
    #[tokio::test]
    async fn abandoning_after_the_result_does_not_cancel_anything() {
        let (invocations, cancellation) = invocation();
        let process = Mutex::new(None);

        assert!(!cancel_invocation(&invocations, &process, "request").await);
        assert!(!cancellation.is_cancelled());
    }

    /// 单次超时只是那次失败；连续到上限才该换 worker（换 worker 会连带失败同一插件的
    /// 其它在途调用，所以不能第一次超时就动手）。成功一次就重新计数。
    #[test]
    fn only_consecutive_timeouts_reach_the_restart_threshold() {
        let tracker = TimeoutTracker::default();

        assert!(!tracker.timed_out(), "第一次超时不重启");
        assert!(tracker.timed_out(), "连续第二次超时该换 worker 了");

        tracker.succeeded();
        assert!(!tracker.timed_out(), "worker 回过一次结果就重新计数");
        assert!(tracker.timed_out());
    }

    /// 响应体上限必须在读的过程中生效：以前整份 `response.bytes()` 读进内存后才判长度，
    /// 不结束的响应（chunked、无 content-length）会一直读下去。
    #[tokio::test]
    async fn a_response_body_over_the_limit_fails_while_it_is_still_being_read() {
        let endless = futures_util::stream::repeat_with(|| {
            Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'x'; 1024 * 1024]))
        });
        let mut response = reqwest::Response::from(axum::http::Response::new(
            reqwest::Body::wrap_stream(endless),
        ));

        let result = tokio::time::timeout(
            Duration::from_secs(30),
            collect_bounded_body(&mut response, &CancellationToken::new()),
        )
        .await
        .expect("超过上限必须立刻失败，而不是把整份响应读完");

        let error = result.expect_err("超限的响应不能被当成成功");
        assert!(
            error.to_string().contains("larger than allowed"),
            "错误要说清是超限：{error}"
        );
    }

    /// 上限之内的响应照常收完。
    #[tokio::test]
    async fn a_response_body_within_the_limit_is_collected_whole() {
        let body = vec![b'y'; MAX_NETWORK_RESPONSE_BYTES as usize];
        let mut response =
            reqwest::Response::from(axum::http::Response::new(reqwest::Body::from(body.clone())));

        let collected = collect_bounded_body(&mut response, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(collected, body);
    }

    /// 脚本化 worker 的握手 + 调用往返（不依赖 Deno，任何平台都能跑）。
    ///
    /// 这条用例起的是**真的子进程**：`server/tests/fixtures/plugin_worker_stub.rs` 用裸
    /// `rustc` 编成一个可执行文件，交给真正的 `PluginWorker` 去 spawn，于是进程管理、
    /// stdio 编解码、协议解析、宿主调用分派整条链路都被走了一遍。桩会把宿主递来的 argv
    /// 记下来、发一条越界的 `host_call`（清单白名单外的主机），再把宿主的答复带回结果里。
    #[tokio::test]
    async fn a_scripted_worker_completes_the_handshake_and_a_host_call_round_trip() {
        let stub = tempfile::tempdir().unwrap();
        let executable = compile_scripted_worker(stub.path());
        let (directory, store, _recorder) = recorder(false, "scripted-worker").await;
        let plugin = scripted_plugin(directory.path());
        let loader = PluginDefinitionLoader::for_test(directory.path()).unwrap();
        let worker = PluginWorker::new(&plugin, executable, loader, store);

        let result = worker
            .invoke(
                "resources.present",
                serde_json::json!({ "resources": [] }),
                CancellationToken::new(),
            )
            .await
            .expect("脚本化 worker 必须完成一次调用");

        assert_eq!(result["method"], "resources.present");
        let host = result["host"].as_str().expect("桩要带回宿主的答复");
        assert!(
            host.starts_with("error:") && host.contains("cannot access host 'evil.example.net'"),
            "越界的 host_call 必须被宿主按白名单回绝：{host}"
        );

        // 宿主交给运行时的参数就是它与 worker 之间的契约（改了参数、忘了同步 SDK，这里会红）。
        let argv = result["argv"]
            .as_array()
            .expect("桩要带回 argv")
            .iter()
            .filter_map(|argument| argument.as_str())
            .collect::<Vec<_>>();
        for expected in [
            "run",
            "--quiet",
            "--no-config",
            "--no-lock",
            "--no-npm",
            "--no-remote",
            "--no-prompt",
        ] {
            assert!(argv.contains(&expected), "argv 缺少 {expected}: {argv:?}");
        }
        assert!(
            argv.iter()
                .any(|argument| argument.starts_with("--import-map=")),
            "argv 必须带上 SDK 的 import map: {argv:?}"
        );
        assert_eq!(
            argv.iter()
                .filter(|argument| argument.starts_with("--allow-read="))
                .count(),
            2,
            "插件目录与 SDK 目录各要给一次读权限: {argv:?}"
        );
        assert!(
            argv.last()
                .is_some_and(|entry| entry.starts_with("file://")),
            "最后一个参数是入口文件的 file URL: {argv:?}"
        );
    }

    /// 用裸 `rustc` 把桩编成可执行文件。每个用例一个临时目录：并行跑时不会互相踩。
    fn compile_scripted_worker(root: &Path) -> std::path::PathBuf {
        let source = include_str!("../../tests/fixtures/plugin_worker_stub.rs");
        let source_path = root.join("plugin_worker_stub.rs");
        std::fs::write(&source_path, source).unwrap();
        let executable = root.join(if cfg!(windows) {
            "plugin_worker_stub.exe"
        } else {
            "plugin_worker_stub"
        });
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = std::process::Command::new(rustc)
            .arg("--edition")
            .arg("2021")
            .arg("-o")
            .arg(&executable)
            .arg(&source_path)
            .status()
            .expect("脚本化 worker 需要 rustc 才能编译");
        assert!(status.success(), "脚本化 worker 必须能编过");
        executable
    }

    /// 桩用的插件条目：白名单里只有 `example.com`，所以桩那条约 `evil.example.net` 的
    /// `host_call` 一定会被宿主按清单回绝。
    fn scripted_plugin(directory: &Path) -> PluginEntry {
        PluginEntry {
            directory: directory.to_path_buf(),
            entry: directory.join("main.ts"),
            manifest: serde_json::from_value(serde_json::json!({
                "apiVersion": crate::plugin::manifest::PLUGIN_API_VERSION,
                "id": "dev.cursorbyok.tests.scripted",
                "name": "Scripted",
                "version": "1.0.0",
                "icon": "",
                "entry": "main.ts",
                "permissions": { "network": ["example.com"] },
            }))
            .unwrap(),
            definition: serde_json::from_value(serde_json::json!({
                "providers": [],
                "resources": [],
            }))
            .unwrap(),
            icon: String::new(),
        }
    }

    /// 宿主网络请求共用一份客户端（连接池、TLS 会话、DNS 缓存都留着）；重建之后必须换成
    /// 新的一份，否则代理中途停掉时重试还是打同一个没人监听的端口。
    #[tokio::test]
    async fn the_host_network_client_is_reused_until_it_is_rebuilt() {
        let (_directory, store, recorder) = recorder(false, "client-reuse").await;
        let host = host_with_recorder(store, recorder).await;

        let first = host.http.get().await.unwrap();
        let again = host.http.get().await.unwrap();
        assert!(
            Arc::ptr_eq(&first, &again),
            "同一个出网路由下必须复用同一份客户端"
        );

        let rebuilt = host.http.rebuild().await.expect("rebuild must succeed");
        assert!(!Arc::ptr_eq(&first, &rebuilt), "重建必须换一份新的客户端");
        let after = host.http.get().await.unwrap();
        assert!(Arc::ptr_eq(&rebuilt, &after), "重建后取到的应是新那份");
    }
}
