//! 端到端覆盖 `server/src/search/**`：引擎 → 出网 → 解析 → 联邦合并 → 抓取/缓存。
//!
//! 这些文件加起来约千行，此前只有零散的单元测试（合并函数、抓取判定各几条），没有任何
//! 一条用例把"catalog 形状的引擎 → HTTP → 解析 → 排序/截断"这条链跑通。这里用环回桩
//! 同时扮演搜索引擎与抓取目标。
//!
//! 边界：工具适配层 `search::execute_semble` 是 `pub(crate)`，集成测试够不到它——它自己
//! 在 `search_provider.rs` 里有单元测试（本地仓库约束、超时、远程 URL 判定）；要在这里
//! 覆盖它需要把可见性改成 `pub`，那是生产改动，不在本任务写域内。

use std::{
    future::IntoFuture,
    sync::{Arc, Mutex},
};

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use cursor_server::search::{
    HtmlEngine, JsonEngine, SearchEngine, SearchHit, WebCache, WebFetch, WebSearch,
};
use serde::Deserialize;
use tokio::net::TcpListener;

/// 一次桩收到的请求：目标（含查询串）与 User-Agent。
#[derive(Clone)]
struct RecordedRequest {
    target: String,
    user_agent: String,
}

#[derive(Clone, Default)]
struct Stub {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl Stub {
    async fn start() -> (Self, String) {
        let stub = Self::default();
        let router = Router::new()
            .route("/html", get(html_results))
            .route("/json", get(json_results))
            .route("/bulk", get(bulk_results))
            .route("/fail", get(fail))
            .with_state(stub.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (stub, format!("http://{address}"))
    }

    fn record(&self, uri: &Uri, headers: &HeaderMap) {
        self.requests.lock().unwrap().push(RecordedRequest {
            target: uri.to_string(),
            user_agent: headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
        });
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

/// 一个 HTML 结果页：相对链接、utm 参数、片段、空标题、缺摘要各一条。
const HTML_PAGE: &str = r#"<!doctype html><html><body>
<div class="result"><h3>Alpha Beta</h3><a href="/docs/alpha?utm_source=news&keep=1#section">Alpha</a><p class="snippet">first <b>snippet</b>  here</p></div>
<div class="result"><h3>Gamma</h3><a href="https://example.com/gamma?gclid=abc">Gamma</a><p class="snippet">second</p></div>
<div class="result"><h3>   </h3><a href="/no-title">x</a><p class="snippet">dropped</p></div>
<div class="result"><h3>Relative</h3><a href="/relative/path/">Relative</a></div>
</body></html>"#;

const BING_TARGET: &str = "https://example.com/target?utm_source=x&keep=2";

/// 一个 JSON 结果页：模板链接、Bing 跳转链接、与 HTML 页共有的绝对链接、相对链接
/// （没有模板时必须丢弃）各一条。
fn json_page() -> String {
    let unwrapped = URL_SAFE_NO_PAD.encode(BING_TARGET);
    format!(
        r#"{{"web":{{"results":[
{{"name":"Alpha <b>Beta</b>","url":"/docs/alpha?utm_medium=email","snippet":"links &amp; text"}},
{{"name":"Bing Target","url":"https://www.bing.com/ck/a?u=a1{unwrapped}","snippet":"bing"}},
{{"name":"Gamma","url":"https://example.com/gamma","snippet":"json gamma"}},
{{"name":"Relative Only","url":"/relative-only","snippet":"dropped"}}
]}}}}"#
    )
}

#[derive(Deserialize)]
struct BulkQuery {
    prefix: String,
}

const BULK_ITEMS: usize = 12;

async fn html_results(State(stub): State<Stub>, uri: Uri, headers: HeaderMap) -> impl IntoResponse {
    stub.record(&uri, &headers);
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        HTML_PAGE,
    )
}

async fn json_results(State(stub): State<Stub>, uri: Uri, headers: HeaderMap) -> impl IntoResponse {
    stub.record(&uri, &headers);
    ([(header::CONTENT_TYPE, "application/json")], json_page())
}

/// 两批互不相交的结果，各 12 条（引擎自己只取前 10 条），用来验证联邦截断。
async fn bulk_results(
    State(stub): State<Stub>,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<BulkQuery>,
) -> impl IntoResponse {
    stub.record(&uri, &headers);
    let items = (0..BULK_ITEMS)
        .map(|index| {
            serde_json::json!({
                "name": format!("{} {index}", query.prefix),
                "url": format!("https://example.com/{}/{index}", query.prefix),
                "snippet": format!("snippet {index}"),
            })
        })
        .collect::<Vec<_>>();
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "items": items }).to_string(),
    )
}

async fn fail(State(stub): State<Stub>, uri: Uri, headers: HeaderMap) -> impl IntoResponse {
    stub.record(&uri, &headers);
    StatusCode::INTERNAL_SERVER_ERROR
}

fn html_engine(origin: &str, id: &'static str) -> SearchEngine {
    SearchEngine::from(HtmlEngine::new(
        id,
        format!("{origin}/html?q={{query}}"),
        "div.result",
        "h3",
        "a",
        "p.snippet",
    ))
}

fn json_engine(origin: &str, id: &'static str) -> SearchEngine {
    SearchEngine::from(JsonEngine::new(
        id,
        format!("{origin}/json?q={{query}}"),
        "/web/results",
        "/name",
        "/url",
        "/snippet",
        None,
    ))
}

fn bulk_engine(origin: &str, id: &'static str, prefix: &str) -> SearchEngine {
    SearchEngine::from(JsonEngine::new(
        id,
        format!("{origin}/bulk?prefix={prefix}&q={{query}}"),
        "/items",
        "/name",
        "/url",
        "/snippet",
        None,
    ))
}

#[tokio::test]
async fn an_html_engine_parses_and_canonicalizes_its_results() {
    let (stub, origin) = Stub::start().await;
    let search = WebSearch::with_engines([html_engine(&origin, "html-stub")]);

    let results = search.search("rust async").await.unwrap();

    let urls = results
        .iter()
        .map(|hit| hit.url.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        urls,
        vec![
            // 相对链接按响应地址补全，utm_* 与片段去掉，`keep` 保留。
            format!("{origin}/docs/alpha?keep=1"),
            // 绝对链接里的 gclid 去掉。
            "https://example.com/gamma".to_owned(),
            // 末尾斜杠去掉；这条没有摘要，chunk 为空。
            format!("{origin}/relative/path"),
        ]
    );
    // 空标题的那条被丢掉，剩下的标题与摘要都做过空白折叠。
    assert_eq!(results[0].title, "Alpha Beta");
    assert_eq!(results[0].chunk, "first snippet here");
    assert_eq!(results[2].chunk, "");
    assert_eq!(results[0].engines, vec!["html-stub"]);
    assert_eq!(results.len(), 3);

    // 查询串按 form-urlencoded 编码，UA 是产品自己的标识。
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].target, "/html?q=rust+async");
    assert!(
        requests[0].user_agent.contains("haxsd-byok"),
        "unexpected user agent: {}",
        requests[0].user_agent
    );
}

#[tokio::test]
async fn an_empty_query_is_rejected_before_any_request() {
    let (stub, origin) = Stub::start().await;
    let search = WebSearch::with_engines([html_engine(&origin, "html-stub")]);

    let error = search
        .search("   ")
        .await
        .expect_err("an empty query must fail");

    assert!(error.to_string().contains("query is empty"), "{error}");
    assert!(
        stub.requests().is_empty(),
        "空查询不该产生出网请求：{:?}",
        stub.requests()
            .iter()
            .map(|request| request.target.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_json_engine_applies_its_link_template_and_plain_texts_titles() {
    let (stub, origin) = Stub::start().await;
    let templated = SearchEngine::from(JsonEngine::new(
        "json-stub",
        format!("{origin}/json?q={{query}}"),
        "/web/results",
        "/name",
        "/url",
        "/snippet",
        Some("https://example.com/resolved?q={value}"),
    ));
    let search = WebSearch::with_engines([templated]);

    let results = search.search("anything").await.unwrap();

    // 每条都被套进模板：`{value}` 是原始 link 的 form-urlencoded 编码。
    let decoded = |url: &str| -> String {
        let encoded = url.split("?q=").nth(1).unwrap();
        url::form_urlencoded::parse(format!("v={encoded}").as_bytes())
            .next()
            .unwrap()
            .1
            .into_owned()
    };
    assert_eq!(results.len(), 4);
    assert!(results
        .iter()
        .all(|hit| hit.url.starts_with("https://example.com/resolved?q=")));
    assert_eq!(decoded(&results[0].url), "/docs/alpha?utm_medium=email");
    assert_eq!(
        decoded(&results[1].url),
        format!(
            "https://www.bing.com/ck/a?u=a1{}",
            URL_SAFE_NO_PAD.encode(BING_TARGET)
        )
    );
    assert_eq!(decoded(&results[2].url), "https://example.com/gamma");
    assert_eq!(decoded(&results[3].url), "/relative-only");
    assert_eq!(results[0].title, "Alpha Beta");
    assert_eq!(results[0].chunk, "links & text");
    assert_eq!(stub.requests().len(), 1);
}

#[tokio::test]
async fn a_json_engine_without_a_template_unwraps_redirects_and_drops_relative_links() {
    let (_stub, origin) = Stub::start().await;
    let search = WebSearch::with_engines([json_engine(&origin, "json-stub")]);

    let results = search.search("anything").await.unwrap();

    // 没有模板时链接必须是绝对地址：Bing 的 /ck/a 跳转被还原成真实目标（utm_* 去掉、
    // 其余查询参数保留），两条相对链接既不能补全也被丢弃。
    assert_eq!(
        results
            .iter()
            .map(|hit| hit.url.clone())
            .collect::<Vec<_>>(),
        vec![
            "https://example.com/target?keep=2",
            "https://example.com/gamma"
        ]
    );
    assert_eq!(results[0].title, "Bing Target");
    assert_eq!(results[0].engines, vec!["json-stub"]);
}

#[tokio::test]
async fn two_engines_agreeing_on_a_url_merge_into_one_hit() {
    let (stub, origin) = Stub::start().await;
    let search = WebSearch::with_engines([
        html_engine(&origin, "html-stub"),
        json_engine(&origin, "json-stub"),
    ]);

    let results = search.search("merge").await.unwrap();

    // 两边都列到的 URL 只出现一次，且带上两个引擎名。
    let shared = results
        .iter()
        .find(|hit| hit.url == "https://example.com/gamma")
        .expect("the shared URL must survive the merge");
    assert_eq!(shared.engines.len(), 2, "{:?}", shared.engines);
    assert!(shared.engines.contains(&"html-stub"));
    assert!(shared.engines.contains(&"json-stub"));
    // 各引擎内部出现的 URL 不会重复。
    let mut urls = results
        .iter()
        .map(|hit| hit.url.clone())
        .collect::<Vec<_>>();
    let total = urls.len();
    urls.sort();
    urls.dedup();
    assert_eq!(urls.len(), total, "merged results must not repeat a URL");
    assert_eq!(stub.requests().len(), 2);
}

#[tokio::test]
async fn a_failing_engine_does_not_hide_a_healthy_one() {
    let (_stub, origin) = Stub::start().await;
    let failing = SearchEngine::from(HtmlEngine::new(
        "broken",
        format!("{origin}/fail?q={{query}}"),
        "div.result",
        "h3",
        "a",
        "p.snippet",
    ));
    let search = WebSearch::with_engines([failing, json_engine(&origin, "json-stub")]);

    let results = search.search("partial").await.unwrap();

    assert!(!results.is_empty());
    assert!(results.iter().all(|hit| hit.engines == vec!["json-stub"]));
}

#[tokio::test]
async fn every_engine_failing_reports_which_ones() {
    let (_stub, origin) = Stub::start().await;
    let failing = |id: &'static str| {
        SearchEngine::from(HtmlEngine::new(
            id,
            format!("{origin}/fail?q={{query}}"),
            "div.result",
            "h3",
            "a",
            "p.snippet",
        ))
    };
    let search = WebSearch::with_engines([failing("broken-a"), failing("broken-b")]);

    let error = search
        .search("nothing")
        .await
        .expect_err("all engines failed");

    let message = error.to_string();
    assert!(message.contains("broken-a"), "{message}");
    assert!(message.contains("broken-b"), "{message}");
}

#[tokio::test]
async fn results_are_capped_at_fifteen_by_reciprocal_rank() {
    let (_stub, origin) = Stub::start().await;
    let search = WebSearch::with_engines([
        bulk_engine(&origin, "bulk-a", "a"),
        bulk_engine(&origin, "bulk-b", "b"),
    ]);

    let results = search.search("bulk").await.unwrap();

    // 桩每个引擎给 12 条，引擎各取前 10 条 → 合并 20 条 → 按 RRF 取前 15。
    let mut scored = Vec::new();
    for (engine, prefix) in [("bulk-a", "a"), ("bulk-b", "b")] {
        for rank in 0..10 {
            let score = 1.0 / (60.0 + rank as f64 + 1.0);
            scored.push((
                format!("https://example.com/{prefix}/{rank}"),
                score,
                engine,
            ));
        }
    }
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap()
            .then_with(|| left.0.cmp(&right.0))
    });
    scored.truncate(15);
    let expected = scored
        .iter()
        .map(|(url, _, _)| url.clone())
        .collect::<Vec<_>>();

    assert_eq!(results.len(), 15);
    assert_eq!(
        results
            .iter()
            .map(|hit| hit.url.clone())
            .collect::<Vec<_>>(),
        expected
    );
    // 每个 URL 只挂它自己引擎的名字。
    for hit in &results {
        assert_eq!(hit.engines.len(), 1, "{:?}", hit.engines);
    }
}

#[tokio::test]
async fn web_fetch_refuses_the_urls_it_must_not_reach() {
    let (stub, origin) = Stub::start().await;
    let fetch = WebFetch::built_in();
    let cases = [
        // 环回：DNS 能解析，但地址不公开。
        (format!("{origin}/html"), "non-public address"),
        // 凭据会随请求发出去。
        (
            "https://user:token@example.com/page".to_owned(),
            "credentials",
        ),
        // 非 http(s) 协议。
        ("ftp://example.com/page".to_owned(), "http or https"),
        // 根本不是 URL。
        ("not a url".to_owned(), "invalid URL"),
    ];

    for (url, expected) in cases {
        let error = fetch
            .fetch(&url)
            .await
            .expect_err("this URL must never be fetched");
        assert!(
            error.to_string().contains(expected),
            "{url} should fail with {expected}: {error}"
        );
    }
    assert!(
        stub.requests().is_empty(),
        "被拒绝的 URL 不该产生任何请求：{:?}",
        stub.requests()
            .iter()
            .map(|request| request.target.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn the_web_cache_round_trips_through_its_router() {
    let directory = tempfile::tempdir().unwrap();
    let cache = WebCache::at(directory.path().join("web")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, cache.router()).into_future());
    cache.set_service_addr(address);

    let content = "first line\nsecond line\n";
    let entry = cache.store(content).await.unwrap().unwrap();

    assert_eq!(entry.line_count, 2);
    assert_eq!(entry.size_bytes, content.len() as i64);
    assert_eq!(
        entry.url,
        format!(
            "http://{}/web-cache/{}",
            address,
            entry.file_path.rsplit(['/', '\\']).next().unwrap()
        )
    );
    assert_eq!(std::fs::read_to_string(&entry.file_path).unwrap(), content);

    // 服务端真的能把这份内容发回去（模型据此分页读全文）。
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(&entry.url)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), content);
}

#[tokio::test]
async fn a_cache_without_a_listening_server_reports_that_instead_of_writing() {
    let directory = tempfile::tempdir().unwrap();
    let cache = WebCache::at(directory.path().join("web")).unwrap();

    let error = cache
        .store("content")
        .await
        .expect_err("a cache without an address must refuse to write");
    assert!(
        error
            .to_string()
            .contains("before the server starts listening"),
        "{error}"
    );
    // 没有地址时也不该留下任何文件。
    assert_eq!(
        std::fs::read_dir(directory.path().join("web"))
            .unwrap()
            .count(),
        0
    );
    // 未配置缓存（默认值）是"没有缓存"，不是错误。
    assert!(WebCache::default()
        .store("content")
        .await
        .unwrap()
        .is_none());
}

#[test]
fn the_built_in_catalog_exposes_its_engines() {
    let search = WebSearch::built_in();
    let ids = search.engine_ids();

    assert!(ids.len() >= 3, "{ids:?}");
    assert!(ids.contains(&"google") && ids.contains(&"bing"), "{ids:?}");
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "引擎 id 不能重复：{ids:?}");
}

#[test]
fn search_hits_carry_their_engines() {
    let hit = SearchHit::new("title", "https://example.com/x", "chunk", vec!["engine"]);

    assert_eq!(hit.title, "title");
    assert_eq!(hit.url, "https://example.com/x");
    assert_eq!(hit.chunk, "chunk");
    assert_eq!(hit.engines, vec!["engine"]);
}
