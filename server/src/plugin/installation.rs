//! Downloads, verifies, extracts, and validates a pinned Deno runtime.
//!
//! 下载是这条路径上唯一会"慢慢失败"的一步：归档有几十兆，用户的网络与出网代理
//! 都可能出问题，而旧实现只有一条"总超时 10 分钟 + 失败就从头再来"的路。现在按
//! 三层处理：断点续传（失败不丢已经下到的部分）、停滞检测（卡住就让位给下一次
//! 尝试，而不是干等到整体超时）、有限重试（三次，每次接着断点）。
use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::header::{CONTENT_RANGE, RANGE};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use super::{
    asset::{RuntimeAsset, DENO_VERSION},
    runtime::PluginRuntimePhase,
};
use crate::{
    network,
    store::{ProxyMode, Store},
    Error, Result,
};

/// 一次初始化里的下载尝试次数。
const DOWNLOAD_ATTEMPTS: usize = 3;
/// 连续多久没有收到任何字节就判定为停滞，放弃本次尝试。
///
/// 它替掉了旧实现的"总超时 10 分钟"：卡住的连接要很快让位给下一次尝试，而
/// "一直在爬但很慢"不该被杀掉——那种情况由下面的整体上限兜。
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// 下载阶段的宽松上限：兜"每次都没到停滞窗口、却永远下不完"的情况。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// 每次重试前的等待；第一次尝试立即开始。
const RETRY_BACKOFF: [Duration; DOWNLOAD_ATTEMPTS - 1] =
    [Duration::from_secs(2), Duration::from_secs(5)];
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
/// 代理提示的总长上限，以及提示里引用系统代理地址的长度上限。
const PROXY_HINT_LIMIT: usize = 200;
const PROXY_ADDRESS_LIMIT: usize = 60;
/// 传输层错误原文（reqwest 会把 URL 一起带上）保留多少字符。
const TRANSPORT_REASON_LIMIT: usize = 160;

/// 下载阶段的时间参数。生产用默认值，测试把它压到毫秒级。
#[derive(Clone, Copy, Debug)]
struct DownloadTuning {
    stall_timeout: Duration,
    overall_timeout: Duration,
    backoff: [Duration; DOWNLOAD_ATTEMPTS - 1],
}

impl Default for DownloadTuning {
    fn default() -> Self {
        Self {
            stall_timeout: STALL_TIMEOUT,
            overall_timeout: DOWNLOAD_TIMEOUT,
            backoff: RETRY_BACKOFF,
        }
    }
}

/// 一次初始化为什么没成：写进 `PluginRuntimeStatus::error` 的一句话原因，
/// 外加这次失败的两个性质。
#[derive(Debug)]
pub(super) struct InstallFailure {
    reason: String,
    /// 网络层失败（连接、停滞、HTTP 状态、超时）：值得重试，也可能与出网代理设置有关。
    network: bool,
    /// 用户主动取消：`.part` 留着，状态回到未初始化。
    cancelled: bool,
}

impl InstallFailure {
    fn cancelled() -> Self {
        Self {
            reason: "插件运行时初始化已取消".into(),
            network: false,
            cancelled: true,
        }
    }

    /// 网络层失败：可能是代理、可能是对方，总之值得再试一次。
    fn network(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            network: true,
            cancelled: false,
        }
    }

    /// 与本机或内容有关的失败：重试改变不了结果。
    fn other(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            network: false,
            cancelled: false,
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// 界面要显示的那句话：原因，必要时再附一句照做就能解决的提示。
    pub(super) async fn status_message(&self, store: &Store) -> String {
        match self.proxy_hint(store).await {
            Some(hint) => format!("{}；{hint}", self.reason),
            None => self.reason.clone(),
        }
    }

    /// 「出网直连、本机却有系统代理」——这台机器上最常见的失败组合。
    ///
    /// 三个条件同时成立才提示：失败在网络层、出网模式是直连、系统代理确实配了
    /// 一个地址。少任何一条，这句提示都会把用户推去改一个与失败无关的设置。
    async fn proxy_hint(&self, store: &Store) -> Option<String> {
        if !self.network {
            return None;
        }
        let settings = store.proxy_settings_secret().await.ok()?;
        if settings.mode != ProxyMode::Direct {
            return None;
        }
        Some(proxy_hint_text(&network::configured_system_proxy()?))
    }
}

/// 提示文案本身。单独成函数是为了能直接断言它的用词与长度。
fn proxy_hint_text(address: &str) -> String {
    shorten(
        &format!(
            "当前出网代理是「直连」，而本机配置了系统代理 {}；把它改成「系统代理」或「自定义」后再试。",
            shorten(address, PROXY_ADDRESS_LIMIT)
        ),
        PROXY_HINT_LIMIT,
    )
}

impl From<io::Error> for InstallFailure {
    fn from(error: io::Error) -> Self {
        Self::other(format!("本地读写失败：{error}"))
    }
}

impl From<Error> for InstallFailure {
    fn from(error: Error) -> Self {
        match error {
            Error::Cancelled => Self::cancelled(),
            Error::Io(error) => Self::from(error),
            error => Self::other(crate::diagnostics::error_chain(&error)),
        }
    }
}

/// 一次下载尝试为什么没成。
#[derive(Debug)]
enum AttemptFailure {
    Cancelled,
    /// 连续 `silence` 没有收到任何字节。
    Stalled {
        silence: Duration,
    },
    /// 整个下载阶段超过了宽松上限。
    Expired,
    /// 服务端给了非成功状态。
    Status(reqwest::StatusCode),
    /// 续传没谈成：服务端回 416，或回 206 但起点不是我们要的位置。
    RangeRejected(String),
    /// 连接被中断、读取失败等传输层问题。
    Transport(String),
    /// 本地读写失败（打开、写入、扫描断点）。
    Local(String),
    TooLarge,
    /// 完整文件对不上 pin 住的 SHA-256。
    Checksum {
        expected: &'static str,
        actual: String,
    },
}

impl AttemptFailure {
    /// 重试能不能改变结果。内容不对、本地读写失败、体积超限都不属于"再试一次"。
    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Stalled { .. } | Self::Status(_) | Self::RangeRejected(_) | Self::Transport(_)
        )
    }

    /// 是不是网络层问题。
    fn network(&self) -> bool {
        match self {
            Self::Cancelled | Self::Local(_) | Self::TooLarge | Self::Checksum { .. } => false,
            Self::Stalled { .. }
            | Self::Expired
            | Self::Status(_)
            | Self::RangeRejected(_)
            | Self::Transport(_) => true,
        }
    }

    /// 给用户看的一句话。
    fn reason(&self) -> String {
        match self {
            Self::Cancelled => "下载已取消".into(),
            Self::Stalled { silence } => format!(
                "下载停滞：连续 {} 秒没有收到任何数据",
                silence.as_secs().max(1)
            ),
            Self::Expired => "下载超时：整体时限内没有完成，请检查网络后重试".into(),
            Self::Status(status) => format!("下载失败：服务端返回 HTTP {}", status.as_u16()),
            Self::RangeRejected(details) => {
                format!("下载失败：断点续传没谈成（{details}），请重试")
            }
            Self::Transport(details) => format!("下载中断：{details}"),
            Self::Local(details) => format!("本地读写失败：{details}"),
            Self::TooLarge => format!(
                "运行时文件超过 {} MB 上限",
                MAX_ARCHIVE_BYTES / (1024 * 1024)
            ),
            Self::Checksum { expected, actual } => format!(
                "校验失败：SHA-256 与预期不符（期望 {}，实际 {}）",
                shorten(expected, 12),
                shorten(actual, 12)
            ),
        }
    }
}

impl From<AttemptFailure> for InstallFailure {
    fn from(failure: AttemptFailure) -> Self {
        match failure {
            AttemptFailure::Cancelled => Self::cancelled(),
            failure if failure.network() => Self::network(failure.reason()),
            failure => Self::other(failure.reason()),
        }
    }
}

pub(super) async fn install(
    root: &Path,
    store: &Store,
    asset: RuntimeAsset,
    cancellation: CancellationToken,
    on_progress: impl Fn(PluginRuntimePhase, u64, Option<u64>),
) -> Result<(), InstallFailure> {
    install_with(
        root,
        store,
        asset,
        cancellation,
        on_progress,
        DownloadTuning::default(),
    )
    .await
}

async fn install_with(
    root: &Path,
    store: &Store,
    asset: RuntimeAsset,
    cancellation: CancellationToken,
    on_progress: impl Fn(PluginRuntimePhase, u64, Option<u64>),
    tuning: DownloadTuning,
) -> Result<(), InstallFailure> {
    let paths = RuntimePaths::new(root, &asset);
    tokio::fs::create_dir_all(&paths.download_dir).await?;
    tokio::fs::create_dir_all(&paths.install_dir).await?;
    remove_if_exists(&paths.executable_staging).await?;
    remove_if_exists(&paths.ready_marker).await?;
    // 注意：`.part` 不在这里删。它是上一次没下完的断点，续传就从它开始。

    let client = network::client(store).await?;
    download_archive(&client, &paths, &asset, &cancellation, &on_progress, tuning).await?;

    // 到这里 `.part` 已经通过 SHA-256：剩下的步骤与"下载"无关了，无论成败都删掉它，
    // 免得下一次对着一个已经完整的文件发 `Range` 请求、白挨一个 416。
    let result = install_archive(&paths, &asset, &cancellation, &on_progress).await;
    if result.is_err() {
        let _ = remove_if_exists(&paths.executable_staging).await;
        let _ = remove_if_exists(&paths.executable).await;
        let _ = remove_if_exists(&paths.ready_marker).await;
    }
    let _ = remove_if_exists(&paths.archive).await;
    result
}

/// 把归档下到 `.part` 并校验：断点续传、停滞检测、有限重试都在这里。
///
/// 返回 `Ok(bytes)` 时 `.part` 是完整且校验通过的文件；失败时它是下一次可续的
/// 断点——除非失败说明内容本身就不可信，那时已经删掉。
async fn download_archive(
    client: &reqwest::Client,
    paths: &RuntimePaths,
    asset: &RuntimeAsset,
    cancellation: &CancellationToken,
    on_progress: &impl Fn(PluginRuntimePhase, u64, Option<u64>),
    tuning: DownloadTuning,
) -> Result<u64, AttemptFailure> {
    let deadline = Instant::now() + tuning.overall_timeout;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let failure = match download_once(
            client,
            paths,
            asset,
            cancellation,
            on_progress,
            tuning.stall_timeout,
            deadline,
        )
        .await
        {
            Ok(downloaded) => return Ok(downloaded),
            Err(failure) => failure,
        };
        if !failure.retryable() || attempt == DOWNLOAD_ATTEMPTS {
            return Err(failure);
        }
        tracing::warn!(
            attempt,
            reason = %failure.reason(),
            "plugin runtime download attempt failed; retrying from the partial file"
        );
        wait_or_cancel(cancellation, tuning.backoff[attempt - 1]).await?;
    }
}

async fn download_once(
    client: &reqwest::Client,
    paths: &RuntimePaths,
    asset: &RuntimeAsset,
    cancellation: &CancellationToken,
    on_progress: &impl Fn(PluginRuntimePhase, u64, Option<u64>),
    stall_timeout: Duration,
    deadline: Instant,
) -> Result<u64, AttemptFailure> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(AttemptFailure::Expired);
    }
    if cancellation.is_cancelled() {
        return Err(AttemptFailure::Cancelled);
    }

    // 断点：`.part` 已经下到的字节数。非空就要求服务端从这里接着发。
    let mut resume_from = match tokio::fs::metadata(&paths.archive).await {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(AttemptFailure::Local(format!("读取断点文件失败: {error}"))),
    };
    if resume_from > MAX_ARCHIVE_BYTES {
        // 断点比允许的上限还大：它不可能是这个归档，别拿它当起点。
        remove_archive(&paths.archive).await;
        resume_from = 0;
    }

    let mut request = client.get(asset.source.as_str());
    if resume_from > 0 {
        request = request.header(RANGE, format!("bytes={resume_from}-"));
    }
    let response = tokio::select! {
        _ = cancellation.cancelled() => return Err(AttemptFailure::Cancelled),
        response = tokio::time::timeout(stall_timeout.min(remaining), request.send()) => response,
    };
    let response = match response {
        // 窗口内连响应头都没等到，也算停滞。
        Err(_) => {
            return Err(AttemptFailure::Stalled {
                silence: stall_timeout,
            })
        }
        Ok(Err(error)) => {
            return Err(AttemptFailure::Transport(shorten(
                &error.to_string(),
                TRANSPORT_REASON_LIMIT,
            )))
        }
        Ok(Ok(response)) => response,
    };
    if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        // 服务端不接受这个起点（多半是本地的残留文件比它那份还长）：丢掉断点重来。
        remove_archive(&paths.archive).await;
        return Err(AttemptFailure::RangeRejected("HTTP 416".into()));
    }
    let response = response.error_for_status().map_err(|error| {
        error.status().map_or_else(
            || AttemptFailure::Transport(shorten(&error.to_string(), TRANSPORT_REASON_LIMIT)),
            AttemptFailure::Status,
        )
    })?;

    // 206 是接着断点发，200 是服务端忽略了 `Range`：后者必须从头写，否则新旧两段
    // 会拼成一个既不是旧文件也不是新文件的四不像。
    let mut downloaded = resume_from;
    let total_bytes = if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        let mut total = None;
        if let Some((start, _, declared)) = content_range(&response) {
            if start != resume_from {
                remove_archive(&paths.archive).await;
                return Err(AttemptFailure::RangeRejected(format!(
                    "服务端从第 {start} 字节开始发，本地断点是第 {resume_from} 字节"
                )));
            }
            total = declared;
        }
        total.or_else(|| response.content_length().map(|left| resume_from + left))
    } else {
        downloaded = 0;
        response.content_length()
    };
    if total_bytes.is_some_and(|size| size > MAX_ARCHIVE_BYTES) {
        remove_archive(&paths.archive).await;
        return Err(AttemptFailure::TooLarge);
    }

    on_progress(PluginRuntimePhase::Downloading, downloaded, total_bytes);
    let mut archive = if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.archive)
            .await
    } else {
        tokio::fs::File::create(&paths.archive).await
    }
    .map_err(|error| AttemptFailure::Local(format!("打开断点文件失败: {error}")))?;

    let mut stream = response.bytes_stream();
    loop {
        if Instant::now() >= deadline {
            return Err(AttemptFailure::Expired);
        }
        let next = tokio::select! {
            _ = cancellation.cancelled() => return Err(AttemptFailure::Cancelled),
            next = tokio::time::timeout(stall_timeout, stream.next()) => next,
        };
        let chunk = match next {
            Err(_) => {
                return Err(AttemptFailure::Stalled {
                    silence: stall_timeout,
                })
            }
            Ok(None) => break,
            Ok(Some(Err(error))) => {
                return Err(AttemptFailure::Transport(shorten(
                    &error.to_string(),
                    TRANSPORT_REASON_LIMIT,
                )))
            }
            Ok(Some(Ok(chunk))) => chunk,
        };
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > MAX_ARCHIVE_BYTES {
            remove_archive(&paths.archive).await;
            return Err(AttemptFailure::TooLarge);
        }
        archive
            .write_all(&chunk)
            .await
            .map_err(|error| AttemptFailure::Local(format!("写入断点文件失败: {error}")))?;
        on_progress(PluginRuntimePhase::Downloading, downloaded, total_bytes);
    }
    archive
        .flush()
        .await
        .map_err(|error| AttemptFailure::Local(format!("写入断点文件失败: {error}")))?;
    archive
        .sync_all()
        .await
        .map_err(|error| AttemptFailure::Local(format!("写入断点文件失败: {error}")))?;
    drop(archive);

    // 摘要必须对着整个文件算：前半段可能是上一次留下的。
    on_progress(PluginRuntimePhase::Verifying, downloaded, total_bytes);
    let actual = sha256_file(&paths.archive)
        .await
        .map_err(|error| AttemptFailure::Local(format!("读取下载文件失败: {error}")))?;
    if actual != asset.sha256 {
        remove_archive(&paths.archive).await;
        return Err(AttemptFailure::Checksum {
            expected: asset.sha256,
            actual,
        });
    }
    Ok(downloaded)
}

/// 解压 `.part`、就位可执行文件、跑一次 `--version`，最后写下 `.ready`。
///
/// 进到这里说明 `.part` 已经通过 SHA-256：再失败就与下载无关了，调用方会把
/// 归档删掉。
async fn install_archive(
    paths: &RuntimePaths,
    asset: &RuntimeAsset,
    cancellation: &CancellationToken,
    on_progress: &impl Fn(PluginRuntimePhase, u64, Option<u64>),
) -> Result<(), InstallFailure> {
    let archive_bytes = tokio::fs::metadata(&paths.archive).await?.len();
    on_progress(
        PluginRuntimePhase::Installing,
        archive_bytes,
        Some(archive_bytes),
    );
    let archive_path = paths.archive.clone();
    let staging_path = paths.executable_staging.clone();
    let executable_name = asset.executable_name();
    let extracted = tokio::task::spawn_blocking(move || {
        extract_runtime_archive(&archive_path, &staging_path, executable_name)
    })
    .await;
    extracted.map_err(|error| InstallFailure::other(format!("解压任务失败: {error}")))??;
    ensure_not_cancelled(cancellation)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(
            &paths.executable_staging,
            std::fs::Permissions::from_mode(0o700),
        )
        .await?;
    }
    remove_if_exists(&paths.executable).await?;
    tokio::fs::rename(&paths.executable_staging, &paths.executable).await?;
    ensure_not_cancelled(cancellation)?;

    on_progress(
        PluginRuntimePhase::Validating,
        archive_bytes,
        Some(archive_bytes),
    );
    validate_runtime(&paths.executable, cancellation).await?;
    ensure_not_cancelled(cancellation)?;
    tokio::fs::write(&paths.ready_marker, format!("deno {DENO_VERSION}\n")).await?;
    tracing::info!(
        version = DENO_VERSION,
        target = asset.target,
        path = %paths.executable.display(),
        "plugin runtime initialized"
    );
    Ok(())
}

/// 退避等待；等着的时候也要响应取消。
async fn wait_or_cancel(
    cancellation: &CancellationToken,
    delay: Duration,
) -> Result<(), AttemptFailure> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(AttemptFailure::Cancelled),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

/// 断点已经不可信：删掉它。删不掉只是记录一条日志——下一次尝试照样会从头下。
async fn remove_archive(archive: &Path) {
    if let Err(error) = remove_if_exists(archive).await {
        tracing::warn!(%error, "cannot remove the untrusted download");
    }
}

/// `Content-Range: bytes 20000-65535/65536` → `(20000, 65535, Some(65536))`。
fn content_range(response: &reqwest::Response) -> Option<(u64, u64, Option<u64>)> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((
        start.trim().parse().ok()?,
        end.trim().parse().ok()?,
        total.trim().parse().ok(),
    ))
}

/// 从头（包括上一次留下的断点部分）算出整个文件的 SHA-256。
async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 把可能很长的原文压到 `limit` 个字符，超出就截断并加省略号。
fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut shortened: String = text.chars().take(limit.saturating_sub(1)).collect();
    shortened.push('…');
    shortened
}

struct RuntimePaths {
    download_dir: PathBuf,
    install_dir: PathBuf,
    archive: PathBuf,
    executable: PathBuf,
    executable_staging: PathBuf,
    ready_marker: PathBuf,
}

impl RuntimePaths {
    fn new(root: &Path, asset: &RuntimeAsset) -> Self {
        let download_dir = root.join(".downloads");
        let install_dir = root
            .join("deno")
            .join(format!("v{DENO_VERSION}"))
            .join(asset.target);
        let executable = install_dir.join(asset.executable_name());
        Self {
            archive: download_dir.join(format!("{}.part", asset.archive_name())),
            executable_staging: install_dir.join(format!("{}.part", asset.executable_name())),
            ready_marker: install_dir.join(".ready"),
            download_dir,
            install_dir,
            executable,
        }
    }
}

pub(super) fn runtime_complete(root: &Path, asset: &RuntimeAsset) -> bool {
    let paths = RuntimePaths::new(root, asset);
    paths.executable.is_file() && paths.ready_marker.is_file()
}

pub(super) fn runtime_executable(root: &Path, asset: &RuntimeAsset) -> PathBuf {
    RuntimePaths::new(root, asset).executable
}

fn extract_runtime_archive(archive: &Path, output: &Path, executable_name: &str) -> Result<()> {
    let file = std::fs::File::open(archive)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| Error::Config(format!("invalid Deno runtime archive: {error}")))?;
    let mut executable = archive
        .by_name(executable_name)
        .map_err(|error| Error::Config(format!("Deno executable missing from archive: {error}")))?;
    let mut destination = std::fs::File::create(output)?;
    io::copy(&mut executable, &mut destination)?;
    destination.sync_all()?;
    Ok(())
}

async fn validate_runtime(executable: &Path, cancellation: &CancellationToken) -> Result<()> {
    let mut command = tokio::process::Command::new(executable);
    super::detach_console(&mut command);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::select! {
        _ = cancellation.cancelled() => return Err(Error::Cancelled),
        result = tokio::time::timeout(VALIDATION_TIMEOUT, command.output()) => {
            result.map_err(|_| Error::Config("Deno runtime validation timed out".into()))??
        }
    };
    if !output.status.success() {
        return Err(Error::Config(format!(
            "Deno runtime validation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let expected = format!("deno {DENO_VERSION}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version_line = stdout.lines().next().unwrap_or_default().trim();
    if version_line != expected && !version_line.starts_with(&format!("{expected} ")) {
        return Err(Error::Config(format!(
            "unexpected Deno runtime version: {version_line}"
        )));
    }
    Ok(())
}

fn ensure_not_cancelled(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

async fn remove_if_exists(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::{
        plugin::runtime::{PluginRuntime, PluginRuntimeState, PluginRuntimeStatus},
        store::ProxySettingsInput,
    };

    #[test]
    fn uses_versioned_runtime_directory() {
        let root = PathBuf::from("/tmp/plugin-runtime");
        let asset = super::super::asset::RuntimeAsset::for_platform("macos", "aarch64").unwrap();
        let paths = RuntimePaths::new(&root, &asset);
        assert_eq!(
            paths.executable,
            root.join("deno")
                .join(format!("v{DENO_VERSION}"))
                .join(asset.target)
                .join("deno")
        );
        assert_eq!(
            paths.archive,
            root.join(".downloads")
                .join("deno-aarch64-apple-darwin.zip.part")
        );
    }

    /// 内容确定的一份假归档：测试只关心字节对不对。
    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    fn digest(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// 把生产用的 asset 换成指向本地桩服务的版本：平台照旧，地址与摘要由测试给。
    fn stub_asset(source: &str, sha256: &str) -> RuntimeAsset {
        let mut asset = RuntimeAsset::current().expect("the test host is a supported platform");
        asset.source = source.to_owned();
        asset.sha256 = Box::leak(sha256.to_owned().into_boxed_str());
        asset
    }

    async fn test_store(mode: ProxyMode) -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        set_proxy(&store, mode, "").await;
        (directory, store)
    }

    async fn set_proxy(store: &Store, mode: ProxyMode, address: &str) {
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

    /// 桩要发回去的响应：状态行、额外的头、以及正文。
    ///
    /// 正文写多少与声明多少都由调用方决定：写得比 `Content-Length` 短就是"发到
    /// 一半断了"。
    struct Reply {
        status: &'static str,
        headers: Vec<String>,
        body: Vec<u8>,
        /// 正文发到这里就停住并保持连接打开（制造停滞）；`None` 表示发完结束。
        stall_after: Option<usize>,
    }

    impl Reply {
        fn whole(status: &'static str, headers: Vec<String>, body: Vec<u8>) -> Self {
            Self {
                status,
                headers,
                body,
                stall_after: None,
            }
        }
    }

    /// 本地 HTTP 服务桩：第 `index` 个连接（从 0 数）交给 `respond` 决定怎么回。
    ///
    /// 每个连接只服务一个请求，响应一律带 `Connection: close`——这样"第几次尝试"
    /// 与连接序号一一对应，`Range` 这类请求细节也好断言。`stall_after` 的连接会一直
    /// 挂着不断开，用来制造"服务端不动了"。
    struct Stub {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl Stub {
        async fn start<F>(respond: F) -> Self
        where
            F: Fn(usize, Option<u64>) -> Reply + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let respond = Arc::new(respond);
            let recorded = requests.clone();
            tokio::spawn(async move {
                let mut index = 0;
                while let Ok((mut socket, _)) = listener.accept().await {
                    let head = read_request_head(&mut socket).await;
                    recorded.lock().unwrap().push(head.clone());
                    let reply = respond(index, range_start(&head));
                    index += 1;
                    tokio::spawn(async move {
                        let _ = write_reply(&mut socket, &reply).await;
                        if reply.stall_after.is_some() {
                            // 连接既不回字节也不关闭：客户端只能靠停滞窗口脱身。
                            std::future::pending::<()>().await;
                        }
                    });
                }
            });
            Self {
                base_url: format!("http://{address}"),
                requests,
            }
        }

        fn archive_url(&self) -> String {
            format!("{}/deno-runtime.zip", self.base_url)
        }

        /// 收到的请求头原文，按连接顺序。
        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }

        fn ranges(&self) -> Vec<Option<u64>> {
            self.requests()
                .iter()
                .map(|head| range_start(head))
                .collect()
        }
    }

    async fn read_request_head(socket: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 512];
        loop {
            let read = match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            buffer.extend_from_slice(&chunk[..read]);
            // GET 没有正文：读到空行就是一个完整的请求。
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    /// `Range: bytes=20000-` 里的起点。
    fn range_start(head: &str) -> Option<u64> {
        let value = head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("range")
                .then_some(value.trim())
        })?;
        value
            .strip_prefix("bytes=")?
            .split('-')
            .next()?
            .parse()
            .ok()
    }

    async fn write_reply(socket: &mut TcpStream, reply: &Reply) -> io::Result<()> {
        let mut head = format!("HTTP/1.1 {}\r\nConnection: close\r\n", reply.status);
        for header in &reply.headers {
            head.push_str(header);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        socket.write_all(head.as_bytes()).await?;
        match reply.stall_after {
            Some(sent) => socket.write_all(&reply.body[..sent]).await,
            None => socket.write_all(&reply.body).await,
        }
    }

    /// 整份响应：声明整份长度，只发前 `sent` 个字节就断开。
    fn cut_short(status: &'static str, body: Vec<u8>, total: usize, sent: usize) -> Reply {
        Reply::whole(
            status,
            vec![format!("Content-Length: {total}")],
            body[..sent].to_vec(),
        )
    }

    /// 206：从 `offset` 发到结尾。
    fn resume_from(body: Vec<u8>, offset: u64) -> Reply {
        let tail = body[offset as usize..].to_vec();
        Reply::whole(
            "206 Partial Content",
            vec![
                format!("Content-Length: {}", tail.len()),
                format!(
                    "Content-Range: bytes {offset}-{}/{}",
                    body.len() - 1,
                    body.len()
                ),
            ],
            tail,
        )
    }

    fn test_tuning() -> DownloadTuning {
        DownloadTuning {
            stall_timeout: Duration::from_millis(200),
            overall_timeout: Duration::from_secs(20),
            backoff: [Duration::from_millis(10); DOWNLOAD_ATTEMPTS - 1],
        }
    }

    /// 下载阶段的现场：临时目录 + 指向桩服务的假归档。
    async fn download_harness(
        root: &Path,
        source: &str,
        sha256: &str,
    ) -> (RuntimePaths, RuntimeAsset) {
        let asset = stub_asset(source, sha256);
        let paths = RuntimePaths::new(root, &asset);
        tokio::fs::create_dir_all(&paths.download_dir)
            .await
            .unwrap();
        (paths, asset)
    }

    fn local_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    /// 服务端发到一半就断：下一次尝试带着 `Range` 从断点接着下，最终内容对得上。
    #[tokio::test]
    async fn a_broken_transfer_resumes_from_the_partial_file() {
        let body = Arc::new(payload(64 * 1024));
        let served = body.clone();
        let stub = Stub::start(move |index, range| {
            let offset = range.unwrap_or(0);
            if index == 0 {
                cut_short("200 OK", served.to_vec(), served.len(), 8000)
            } else {
                resume_from(served.to_vec(), offset)
            }
        })
        .await;

        let directory = tempfile::tempdir().unwrap();
        let (paths, asset) =
            download_harness(directory.path(), &stub.archive_url(), &digest(&body)).await;
        let progress = Arc::new(Mutex::new(Vec::new()));
        let downloaded = download_archive(
            &local_client(),
            &paths,
            &asset,
            &CancellationToken::new(),
            &|phase, downloaded, total| {
                progress.lock().unwrap().push((phase, downloaded, total));
            },
            test_tuning(),
        )
        .await
        .unwrap();

        assert_eq!(downloaded, body.len() as u64);
        assert_eq!(tokio::fs::read(&paths.archive).await.unwrap(), *body);

        let ranges = stub.ranges();
        assert_eq!(ranges.len(), 2, "应当正好重试一次");
        assert!(ranges[0].is_none(), "第一次尝试不该带 Range");
        let resume = ranges[1].expect("第二次尝试必须带 Range");
        assert!(
            resume > 0 && resume <= 8000,
            "断点必须是已经收到的那部分：{resume}"
        );
        // 进度条上看到的仍然是"整份"：206 只声明了剩下的长度。
        let progress = progress.lock().unwrap();
        assert_eq!(
            progress.last().unwrap().2,
            Some(body.len() as u64),
            "续传时的总量必须还是整份大小"
        );
        assert!(
            progress
                .iter()
                .any(|(_, downloaded, _)| *downloaded == resume),
            "续传的第一个进度值应当是断点位置"
        );
    }

    /// 服务端忽略 `Range` 直接回整份：旧断点必须丢掉，不能拼成两段。
    #[tokio::test]
    async fn a_server_that_ignores_range_rewrites_the_file() {
        let body = Arc::new(payload(48 * 1024));
        let served = body.clone();
        let stub = Stub::start(move |index, _| {
            if index == 0 {
                cut_short("200 OK", served.to_vec(), served.len(), 6000)
            } else {
                Reply::whole(
                    "200 OK",
                    vec![format!("Content-Length: {}", served.len())],
                    served.to_vec(),
                )
            }
        })
        .await;

        let directory = tempfile::tempdir().unwrap();
        let (paths, asset) =
            download_harness(directory.path(), &stub.archive_url(), &digest(&body)).await;
        let downloaded = download_archive(
            &local_client(),
            &paths,
            &asset,
            &CancellationToken::new(),
            &|_, _, _| {},
            test_tuning(),
        )
        .await
        .unwrap();

        assert_eq!(downloaded, body.len() as u64);
        assert!(
            stub.ranges()[1].is_some(),
            "第二次尝试应当请求续传，服务端才有机会忽略它"
        );
        assert_eq!(
            tokio::fs::read(&paths.archive).await.unwrap(),
            *body,
            "忽略 Range 的响应必须整份重写"
        );
    }

    /// 服务端发一半就不动了：在停滞窗口内放弃本次尝试，退避后接着断点重试成功。
    #[tokio::test]
    async fn a_stalled_transfer_gives_up_within_the_stall_window_and_retries() {
        let body = Arc::new(payload(32 * 1024));
        let served = body.clone();
        let stub = Stub::start(move |index, range| {
            if index == 0 {
                let sent = 4096;
                Reply {
                    status: "200 OK",
                    headers: vec![format!("Content-Length: {}", served.len())],
                    body: served[..sent].to_vec(),
                    stall_after: Some(sent),
                }
            } else {
                resume_from(served.to_vec(), range.unwrap_or(0))
            }
        })
        .await;

        let directory = tempfile::tempdir().unwrap();
        let (paths, asset) =
            download_harness(directory.path(), &stub.archive_url(), &digest(&body)).await;
        let tuning = test_tuning();
        let started = Instant::now();
        let downloaded = download_archive(
            &local_client(),
            &paths,
            &asset,
            &CancellationToken::new(),
            &|_, _, _| {},
            tuning,
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(downloaded, body.len() as u64);
        assert_eq!(tokio::fs::read(&paths.archive).await.unwrap(), *body);
        assert!(
            elapsed >= tuning.stall_timeout,
            "停滞窗口必须真的等满：{elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "停滞的尝试要很快让位，不能等到整体上限：{elapsed:?}"
        );
        assert_eq!(stub.requests().len(), 2);
    }

    /// 摘要对不上：不重试，也不留一个坏断点给下一次。
    #[tokio::test]
    async fn a_checksum_mismatch_drops_the_partial_file_without_retrying() {
        let body = Arc::new(payload(8 * 1024));
        let served = body.clone();
        let stub = Stub::start(move |_, _| {
            Reply::whole(
                "200 OK",
                vec![format!("Content-Length: {}", served.len())],
                served.to_vec(),
            )
        })
        .await;

        let directory = tempfile::tempdir().unwrap();
        let (paths, asset) = download_harness(
            directory.path(),
            &stub.archive_url(),
            &digest(b"a different archive"),
        )
        .await;
        let failure = download_archive(
            &local_client(),
            &paths,
            &asset,
            &CancellationToken::new(),
            &|_, _, _| {},
            test_tuning(),
        )
        .await
        .unwrap_err();

        assert!(
            failure.reason().contains("校验失败"),
            "unexpected reason: {}",
            failure.reason()
        );
        assert!(!failure.retryable(), "内容不对，重试没有意义");
        assert!(!paths.archive.exists(), "摘要对不上的文件不能留在断点位置");
        assert_eq!(stub.requests().len(), 1, "摘要失败不该触发重试请求");
    }

    /// 取消：已经下到的部分留着，状态回到未初始化。
    #[tokio::test]
    async fn cancelling_keeps_the_partial_file_and_returns_to_uninitialized() {
        let body = Arc::new(payload(32 * 1024));
        let served = body.clone();
        let stub = Stub::start(move |_, _| Reply {
            status: "200 OK",
            headers: vec![format!("Content-Length: {}", served.len())],
            body: served[..4096].to_vec(),
            stall_after: Some(4096),
        })
        .await;
        let (_store_directory, store) = test_store(ProxyMode::Direct).await;
        let directory = tempfile::tempdir().unwrap();
        let asset = stub_asset(&stub.archive_url(), &digest(&body));
        // 运行时把根目录定在 <root>/runtime（见 runtime_with），断点文件要在同一个根下找。
        let runtime_root = directory.path().join("runtime");
        let paths = RuntimePaths::new(&runtime_root, &asset);
        let runtime = crate::plugin::runtime::tests::runtime_with(directory.path(), asset.clone());

        runtime.initialize(store);
        wait_until(&runtime, |status| status.downloaded_bytes > 0).await;
        runtime.cancel_initialization();
        let status = wait_for_state(&runtime, PluginRuntimeState::Uninitialized).await;

        assert!(status.error.is_none(), "取消不是失败：{status:?}");
        let length = tokio::fs::metadata(&paths.archive).await.unwrap().len();
        assert!(length > 0, "取消不该丢掉已经下到的部分");
        assert_eq!(stub.requests().len(), 1, "取消之后不该再发请求");
    }

    /// 失败时界面看到的是真实原因，而不是笼统的"初始化失败"。
    #[tokio::test]
    async fn a_failed_initialization_reports_the_reason() {
        let body = Arc::new(payload(16 * 1024));
        let served = body.clone();
        let stub = Stub::start(move |_, _| {
            Reply::whole(
                "200 OK",
                vec![format!("Content-Length: {}", served.len())],
                served.to_vec(),
            )
        })
        .await;
        let (_store_directory, store) = test_store(ProxyMode::Direct).await;
        let directory = tempfile::tempdir().unwrap();
        let asset = stub_asset(&stub.archive_url(), &digest(b"a different archive"));
        let paths = RuntimePaths::new(directory.path(), &asset);
        let runtime = crate::plugin::runtime::tests::runtime_with(directory.path(), asset);

        runtime.initialize(store);
        let status = wait_for_state(&runtime, PluginRuntimeState::Failed).await;

        let error = status.error.expect("失败必须带原因");
        assert!(error.contains("校验失败"), "unexpected error: {error}");
        assert!(!paths.archive.exists());
    }

    /// 提示只在"网络层失败 + 出网直连 + 本机确实有系统代理"三条同时成立时出现。
    #[tokio::test]
    async fn the_proxy_hint_needs_a_direct_route_and_a_configured_system_proxy() {
        let (_directory, store) = test_store(ProxyMode::Direct).await;
        let network_failure = InstallFailure::network("下载失败：服务端返回 HTTP 503");
        let local_failure = InstallFailure::other("本地读写失败：磁盘已满");

        assert!(!local_failure.status_message(&store).await.contains("直连"));

        let expected = network::configured_system_proxy().is_some();
        assert_eq!(
            network_failure
                .status_message(&store)
                .await
                .contains("「直连」"),
            expected,
            "只有本机真的配了系统代理才该提示"
        );

        set_proxy(&store, ProxyMode::Custom, "http://127.0.0.1:7897").await;
        assert!(
            !network_failure
                .status_message(&store)
                .await
                .contains("直连"),
            "自定义模式用的是用户自己填的地址，不该提示他改模式"
        );
    }

    #[test]
    fn the_direct_mode_hint_names_the_mode_and_fits_the_budget() {
        let hint = proxy_hint_text("127.0.0.1:7897");
        assert!(hint.contains("「直连」"), "{hint}");
        assert!(hint.contains("127.0.0.1:7897"), "{hint}");
        assert!(hint.chars().count() <= PROXY_HINT_LIMIT, "{hint}");

        let long = proxy_hint_text(&"a".repeat(400));
        assert!(long.chars().count() <= PROXY_HINT_LIMIT, "{long}");
    }

    async fn wait_until(runtime: &PluginRuntime, condition: impl Fn(&PluginRuntimeStatus) -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if condition(&runtime.status()) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the runtime never reached the expected progress");
    }

    async fn wait_for_state(
        runtime: &PluginRuntime,
        state: PluginRuntimeState,
    ) -> PluginRuntimeStatus {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = runtime.status();
                if status.state == state {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the runtime never reached the expected state")
    }
}
