//! Implements the configured external search provider adapter.
use std::{path::PathBuf, sync::Arc, time::Duration};

use semble_core::{ContentType, FindRelatedRequest, SearchEngine, SearchRequest, SembleConfig};
use serde::Deserialize;
use serde_json::Value;
use tokio::time::timeout;

use crate::{store::Store, Error, Result};

use super::fetch;

/// 引擎缓存。嵌入模型只该加载一次,但客户端是在构建那一刻按当时的出网路由配好的:纯
/// `OnceCell` 会让"用户在设置里换了代理"永远不生效(Semble 继续走旧路由,直到重启)。
/// 因此缓存里连路由指纹一起存,指纹变了就重建。
static ENGINE: parking_lot::Mutex<Option<(String, Arc<SearchEngine>)>> =
    parking_lot::Mutex::new(None);

/// `repo` 会变成 `git clone` 子进程、本地目录遍历与嵌入模型推理,整段工作原本没有任何
/// 对工具轮负责的超时:一个不答话的远端就能让这一轮永远停在"等待工具结果"上。120s 是
/// 单次工具调用的硬上限(依赖层的 git 子进程另有 120s 上限)。
const SEMBLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContentSelection {
    #[default]
    Code,
    Docs,
    Config,
    All,
}

#[derive(Debug, Deserialize)]
struct SearchArguments {
    query: String,
    repo: String,
    #[serde(default = "default_top_k")]
    top_k: usize,
    #[serde(default = "default_snippet_lines")]
    max_snippet_lines: Option<usize>,
    #[serde(default)]
    content: ContentSelection,
}

#[derive(Debug, Deserialize)]
struct FindRelatedArguments {
    repo: String,
    file_path: String,
    line: usize,
    #[serde(default = "default_top_k")]
    top_k: usize,
    #[serde(default = "default_snippet_lines")]
    max_snippet_lines: Option<usize>,
    #[serde(default)]
    content: ContentSelection,
}

enum Operation {
    Search(SearchArguments),
    FindRelated(FindRelatedArguments),
}

pub(crate) async fn execute(
    tool_name: &str,
    arguments: Value,
    store: Option<Store>,
    workspace_paths: &[String],
) -> std::result::Result<Value, String> {
    execute_within(SEMBLE_TIMEOUT, tool_name, arguments, store, workspace_paths).await
}

/// `execute` 的实现体,预算可传入 —— 只为了让用例把上限调小、验证"哪几段在预算里"。
/// 生产只有一个调用点,恒用 [`SEMBLE_TIMEOUT`]。
async fn execute_within(
    limit: Duration,
    tool_name: &str,
    arguments: Value,
    store: Option<Store>,
    workspace_paths: &[String],
) -> std::result::Result<Value, String> {
    let operation = match tool_name {
        "semblesearch" => {
            Operation::Search(serde_json::from_value(arguments).map_err(|error| error.to_string())?)
        }
        "semblefindrelated" => Operation::FindRelated(
            serde_json::from_value(arguments).map_err(|error| error.to_string())?,
        ),
        _ => return Err(format!("unsupported Semble tool: {tool_name}")),
    };
    let repo = operation.repo().to_owned();
    // 仓库校验也必须在这个预算里:它要解析远端 URL 的 DNS、canonicalize 本地目录 ——
    // 慢 DNS 或坏 repo 一样能把工具轮卡到比 Semble 超时更久(以前 `check_repository` 在
    // `bounded_within` 之外,那段完全没有上限)。
    bounded_within(limit, async move {
        check_repository(&repo, workspace_paths).await?;
        run(operation, store).await
    })
    .await
}

impl Operation {
    fn repo(&self) -> &str {
        match self {
            Operation::Search(arguments) => &arguments.repo,
            Operation::FindRelated(arguments) => &arguments.repo,
        }
    }
}
async fn run(operation: Operation, store: Option<Store>) -> std::result::Result<Value, String> {
    let engine = engine(store).await.map_err(|error| error.to_string())?;
    tokio::task::spawn_blocking(move || match operation {
        Operation::Search(arguments) => engine
            .search(SearchRequest {
                query: arguments.query,
                repo: arguments.repo.into(),
                top_k: arguments.top_k,
                max_snippet_lines: arguments.max_snippet_lines,
                content: content(arguments.content),
            })
            .and_then(json_value),
        Operation::FindRelated(arguments) => engine
            .find_related(FindRelatedRequest {
                repo: arguments.repo.into(),
                file_path: arguments.file_path,
                line: arguments.line,
                top_k: arguments.top_k,
                max_snippet_lines: arguments.max_snippet_lines,
                content: content(arguments.content),
            })
            .and_then(json_value),
    })
    .await
    .map_err(|error| format!("Semble search worker failed: {error}"))?
    .map_err(|error| error.to_string())
}

/// 给一段没有内建超时的工作套硬上限。超时后 `spawn_blocking` 里的线程还会跑完(它不可
/// 中断),但工具轮不再被它挂住。
async fn bounded_within<T>(
    limit: Duration,
    work: impl std::future::Future<Output = std::result::Result<T, String>>,
) -> std::result::Result<T, String> {
    match timeout(limit, work).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "Semble search timed out after {} seconds",
            limit.as_secs()
        )),
    }
}

/// `repo` 是模型给出的参数,服务端会拿它出网(`git clone`)或读本地目录建索引。两条路径都要
/// 有边界:远程只允许 https + 公网地址(与网页抓取同一套判定),本地路径 canonicalize 之后
/// 必须落在本会话的 workspace 根内 —— 否则这个没有审批的工具等于把服务端进程的用户权限
/// (以及内网可达性)交给模型,绕开客户端沙箱与用户确认。
async fn check_repository(
    repo: &str,
    workspace_paths: &[String],
) -> std::result::Result<(), String> {
    repository_check_delay().await;
    let repo = repo.trim();
    if repo.is_empty() {
        return Err("repo must not be empty".into());
    }
    if repo.contains("://") {
        return fetch::check_remote_git_url(repo).await;
    }
    let repo = repo.to_owned();
    let workspace_paths = workspace_paths.to_vec();
    tokio::task::spawn_blocking(move || check_local_repository(&repo, &workspace_paths))
        .await
        .map_err(|error| format!("repository check failed: {error}"))?
}

/// 测试钩子:把仓库校验拖慢,用来验证它确实落在工具预算之内(生产恒为 0)。
#[cfg(test)]
static REPOSITORY_CHECK_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

async fn repository_check_delay() {
    #[cfg(test)]
    {
        let millis = REPOSITORY_CHECK_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed);
        if millis > 0 {
            tokio::time::sleep(Duration::from_millis(millis)).await;
        }
    }
}

fn check_local_repository(
    repo: &str,
    workspace_paths: &[String],
) -> std::result::Result<(), String> {
    let path = std::fs::canonicalize(repo)
        .map_err(|error| format!("cannot open local repository {repo}: {error}"))?;
    if !path.is_dir() {
        return Err(format!("local repository {repo} is not a directory"));
    }
    // 根也 canonicalize:客户端的 workspace 路径写法不固定(分隔符、大小写、`..`),
    // canonical 形式在 Windows 上还带真实大小写,前缀判定才是可靠的。
    let roots = workspace_paths
        .iter()
        .filter_map(|root| {
            let root = root.trim();
            (!root.is_empty())
                .then(|| std::fs::canonicalize(root).ok())
                .flatten()
        })
        .collect::<Vec<PathBuf>>();
    if roots.is_empty() {
        return Err("local repositories are only allowed inside the session workspace".into());
    }
    if !roots.iter().any(|root| path.starts_with(root)) {
        return Err(format!(
            "local repository {repo} is outside the session workspace"
        ));
    }
    Ok(())
}

async fn engine(store: Option<Store>) -> Result<Arc<SearchEngine>> {
    let (fingerprint, builder) = match &store {
        Some(store) => {
            // 指纹取整份设置的 Debug 表示:任何字段变化(模式/地址/凭据)都会让旧客户端失效,
            // 不需要在这边重复 store/network 的字段清单;它只在内存里比较,不进日志。
            // 系统代理的运行时变化由 network.rs 的 watch_system_proxy + 直连兜底负责,但那是在
            // **建客户端时**读的:已经建好的引擎仍会走旧路由 —— 与改动前同一限制,不在本次范围。
            let settings = store.proxy_settings_secret().await?;
            let fingerprint = format!("{settings:?}");
            (
                fingerprint,
                crate::network::blocking_client_builder(store).await?,
            )
        }
        None => (
            "direct".to_string(),
            reqwest::blocking::Client::builder().use_native_tls(),
        ),
    };
    if let Some(engine) = cached_engine(&fingerprint) {
        return Ok(engine);
    }
    // 阻塞客户端也要有硬上限:异步侧虽然有整体预算,但 `spawn_blocking` 里的线程不可中断,
    // 网络黑洞会让它一直挂在读上、把阻塞池占满 —— 下一次工具轮连 worker 都起不来。
    let client = blocking_client(builder, SEMBLE_TIMEOUT)?;
    let engine = tokio::task::spawn_blocking(move || {
        SearchEngine::load_default_with_client(SembleConfig::default(), &client)
            .map(Arc::new)
            .map_err(|error| Error::Config(format!("load Semble search engine: {error}")))
    })
    .await
    .map_err(|error| Error::Config(format!("load Semble search engine: {error}")))??;
    *ENGINE.lock() = Some((fingerprint, engine.clone()));
    Ok(engine)
}

/// Semble 的阻塞客户端。`limit` 传进来(而不是直接读常量)是为了让用例用一个小上限验证
/// 超时真的生效 —— 生产只有一个调用点,恒传 [`SEMBLE_TIMEOUT`]。
fn blocking_client(
    builder: reqwest::blocking::ClientBuilder,
    limit: Duration,
) -> Result<reqwest::blocking::Client> {
    builder.timeout(limit).build().map_err(Into::into)
}

/// 缓存命中判定:出网路由指纹一致才复用。
fn cached_engine(fingerprint: &str) -> Option<Arc<SearchEngine>> {
    let cache = ENGINE.lock();
    let (cached, engine) = cache.as_ref()?;
    (cached == fingerprint).then(|| engine.clone())
}

fn json_value(response: semble_core::SearchResponse) -> semble_core::Result<Value> {
    serde_json::to_value(response)
        .map_err(|error| semble_core::Error::Serialization(error.to_string()))
}

fn content(selection: ContentSelection) -> Vec<ContentType> {
    match selection {
        ContentSelection::Code => vec![ContentType::Code],
        ContentSelection::Docs => vec![ContentType::Docs],
        ContentSelection::Config => vec![ContentType::Config],
        ContentSelection::All => vec![ContentType::Code, ContentType::Docs, ContentType::Config],
    }
}

fn default_top_k() -> usize {
    5
}

fn default_snippet_lines() -> Option<usize> {
    Some(10)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubEmbedder;

    impl semble_core::Embedder for StubEmbedder {
        fn id(&self) -> &str {
            "stub"
        }

        fn dimensions(&self) -> usize {
            8
        }

        fn encode(&self, texts: &[String]) -> semble_core::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.0; 8]).collect())
        }
    }

    /// 引擎缓存只在出网路由指纹一致时复用:纯 `OnceCell` 时代"在设置里换了代理"永远不生效
    /// (Semble 继续走旧路由,直到重启)。重建路径要联网下载嵌入模型,所以这里钉的是判定本身。
    #[test]
    fn the_engine_cache_is_reused_only_for_the_same_route_fingerprint() {
        let engine = Arc::new(SearchEngine::with_embedder(
            SembleConfig::default(),
            Arc::new(StubEmbedder),
        ));
        *ENGINE.lock() = Some(("route-a".to_string(), engine));

        assert!(cached_engine("route-a").is_some(), "指纹一致要复用");
        assert!(cached_engine("route-b").is_none(), "指纹变了必须重建");

        *ENGINE.lock() = None;
        assert!(cached_engine("route-a").is_none(), "空缓存不该命中");
    }

    /// `repo` 没有审批这道门,所以本地路径必须自己限在 workspace 根内。
    #[test]
    fn local_repositories_are_confined_to_the_session_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let project = workspace.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let roots = vec![workspace.path().to_string_lossy().into_owned()];

        assert!(check_local_repository(&project.to_string_lossy(), &roots).is_ok());
        assert!(check_local_repository(&workspace.path().to_string_lossy(), &roots).is_ok());
        let error = check_local_repository(&outside.path().to_string_lossy(), &roots).unwrap_err();
        assert!(error.contains("outside the session workspace"), "{error}");
    }

    /// 客户端没给 workspace 路径时不能默认放行 —— 那正是"服务端读任意目录"的原样。
    #[test]
    fn local_repositories_are_refused_without_a_known_workspace_root() {
        let directory = tempfile::tempdir().unwrap();
        let error = check_local_repository(&directory.path().to_string_lossy(), &[]).unwrap_err();
        assert!(error.contains("session workspace"), "{error}");
    }

    #[test]
    fn a_missing_local_repository_is_reported_instead_of_indexed() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("not-there");
        let roots = vec![directory.path().to_string_lossy().into_owned()];
        assert!(check_local_repository(&missing.to_string_lossy(), &roots).is_err());
    }

    /// 没有超时的话,一个卡住的远端能让工具轮永远停在等待结果上。
    #[tokio::test]
    async fn a_hung_worker_times_out_instead_of_blocking_the_tool_round() {
        let outcome = bounded_within(
            Duration::from_millis(20),
            std::future::pending::<std::result::Result<Value, String>>(),
        )
        .await;
        assert!(outcome.unwrap_err().contains("timed out"));
    }

    /// 仓库校验也在工具预算之内:慢 DNS / 坏 repo 不能绕过超时把工具轮卡住。
    ///
    /// 校验段被人为拖慢(测试钩子)并把预算调小:接线正确时 `execute` 在预算内以超时返回;
    /// 一旦把 `check_repository` 挪回 `bounded_within` 之外(老接法),这里会等满 300ms 并拿到
    /// 校验本身的错误 —— 两条断言都会红。钩子只挂 20ms 左右,同进程并行跑的其它用例最多
    /// 多等这 300ms。
    #[tokio::test]
    async fn the_repository_check_shares_the_tool_budget() {
        let limit = Duration::from_millis(20);
        REPOSITORY_CHECK_DELAY_MS.store(300, std::sync::atomic::Ordering::Relaxed);
        let started = std::time::Instant::now();
        let error = execute_within(
            limit,
            "semblesearch",
            serde_json::json!({ "repo": "/tmp/never-looked-at", "query": "x" }),
            None,
            &[],
        )
        .await
        .unwrap_err();
        REPOSITORY_CHECK_DELAY_MS.store(0, std::sync::atomic::Ordering::Relaxed);

        assert!(error.contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "校验段必须在预算内被切断,而不是等它自己结束({:?})",
            started.elapsed()
        );
    }

    /// 坏 repo 经过 `execute` 要在起引擎/出网之前就被拒(顺带证明校验段确实接在这条路上)。
    #[tokio::test]
    async fn a_bad_repository_is_rejected_before_the_engine_starts() {
        let error = execute_within(
            Duration::from_secs(30),
            "semblesearch",
            serde_json::json!({ "repo": "https://127.0.0.1/owner/repo.git", "query": "x" }),
            None,
            &[],
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("public") || error.contains("address"),
            "坏 repo 必须在出网之前被拒: {error}"
        );
    }

    /// 阻塞客户端必须有上限:Semble 的 git clone/模型下载都走它 —— 异步侧有整体预算,但
    /// `spawn_blocking` 线程不可中断,一直挂在读上会把阻塞池占满。
    #[test]
    fn the_blocking_client_gives_up_on_a_silent_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // 接住连接、握着不放也不回应:客户端必须自己超时
            // (把 stream 立刻丢掉会变成"连接被关",那就测不到读超时了)。
            let (stream, _) = listener.accept().expect("accept");
            std::thread::sleep(Duration::from_secs(5));
            drop(stream);
        });

        // 直连:这台机器可能配了环境代理,代理会把回环请求转走,测不到"服务端沉默"。
        let client = blocking_client(
            reqwest::blocking::Client::builder().no_proxy(),
            Duration::from_millis(100),
        )
        .expect("blocking client");
        let started = std::time::Instant::now();
        let error = client
            .get(format!("http://{address}/"))
            .send()
            .expect_err("silent server must time out");
        // 服务端要沉默 5 秒:客户端必须在自己的上限(100ms)附近放弃,而不是等它;
        // 也不能一碰就报错 —— 那说明根本没走到读超时。
        assert!(
            started.elapsed() >= Duration::from_millis(50)
                && started.elapsed() < Duration::from_secs(2),
            "阻塞客户端没有在自己的上限内放弃({:?}): {error}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_fast_worker_still_returns_its_result() {
        let outcome = bounded_within(Duration::from_secs(30), async {
            Ok(serde_json::json!({"ok": true}))
        })
        .await;
        assert_eq!(outcome.unwrap(), serde_json::json!({"ok": true}));
    }

    /// 远程 `repo` 走与网页抓取同一套判定,内网与明文 http 在出网之前就被拒。
    #[tokio::test]
    async fn remote_repositories_reuse_the_web_fetch_rules() {
        assert!(check_repository("http://10.0.0.5/owner/repo.git", &[])
            .await
            .is_err());
        assert!(check_repository("https://127.0.0.1/owner/repo.git", &[])
            .await
            .is_err());
        assert!(check_repository("", &[]).await.is_err());
    }
}
