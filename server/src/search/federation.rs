//! Aggregates and ranks results from search sources.
use std::{
    cmp::Ordering,
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::future::join_all;
use tokio::sync::OnceCell;

use crate::store::Store;

use super::{catalog, SearchEngine, SearchHit};

const RRF_K: f64 = 60.0;
const MAX_RESULTS: usize = 15;

/// 同一个 query 的短期结果缓存:重试与多轮追问常常问同一个问题,而每次搜索都是 23 个出网
/// 请求。TTL 取 5 分钟(搜索引擎自己的结果窗口远大于此),条目上限 16,防止长期运行攒下
/// 无界内存;只缓存成功结果,失败照旧重试。
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const CACHE_ENTRIES: usize = 16;

/// 查询 → (写入时刻, 结果)。缓存随 `WebSearch` 实例存活：一次 run 的引擎集合是固定的，
/// 换引擎配置后不会命中上一次 run 的旧结果。
type ResultCache = HashMap<String, (Instant, Vec<SearchHit>)>;

#[derive(Clone)]
pub struct WebSearch {
    client: SearchClient,
    engines: Vec<SearchEngine>,
    cache: Arc<parking_lot::Mutex<ResultCache>>,
}

#[derive(Clone)]
enum SearchClient {
    /// 客户端按实例记忆:以前每次 `search()` 都走一遍 `network::client`(读代理设置 + 建连接
    /// 池),一次工具轮里 23 个引擎共用同一个客户端就够。实例的生命周期是一个 run,所以不会
    /// 像进程级缓存那样在代理设置变化后长期发往旧路由。
    Managed {
        store: Store,
        client: Arc<OnceCell<reqwest::Client>>,
    },
    Direct(reqwest::Client),
}

#[derive(Debug, thiserror::Error)]
#[error("web search failed: {0}")]
pub struct SearchError(String);

impl WebSearch {
    pub fn built_in() -> Self {
        Self::with_engines(catalog::engines())
    }

    pub(crate) fn managed(store: Store) -> Self {
        Self {
            client: SearchClient::Managed {
                store,
                client: Arc::new(OnceCell::new()),
            },
            engines: catalog::engines(),
            cache: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    pub fn with_engines<I, E>(engines: I) -> Self
    where
        I: IntoIterator<Item = E>,
        E: Into<SearchEngine>,
    {
        Self {
            client: SearchClient::Direct(reqwest::Client::new()),
            engines: engines.into_iter().map(Into::into).collect(),
            cache: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    pub fn engine_ids(&self) -> Vec<&'static str> {
        self.engines.iter().map(SearchEngine::id).collect()
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, SearchError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(SearchError("query is empty".into()));
        }
        if let Some(cached) = self.cached_results(query) {
            tracing::debug!(
                query,
                results = cached.len(),
                "serving web search from cache"
            );
            return Ok(cached);
        }
        let client = self.client().await?;
        let responses = join_all(
            self.engines
                .iter()
                .map(|engine| engine.search(&client, query)),
        )
        .await;
        let mut merged = HashMap::<String, SearchHit>::new();
        let mut failures = Vec::new();
        for (engine, response) in self.engines.iter().zip(responses) {
            match response {
                Ok(results) if !results.is_empty() => {
                    tracing::debug!(
                        engine = engine.id(),
                        results = results.len(),
                        "search engine completed"
                    );
                    merge(&mut merged, engine.id(), results)
                }
                Ok(_) => {
                    tracing::debug!(engine = engine.id(), "search engine returned no results");
                    failures.push(engine.id().to_string());
                }
                Err(error) => {
                    tracing::warn!(engine = engine.id(), %error, "search engine failed");
                    failures.push(engine.id().to_string());
                }
            }
        }
        if merged.is_empty() {
            return Err(SearchError(format!(
                "no results from engines: {}",
                failures.join(", ")
            )));
        }
        let mut results = merged.into_values().collect::<Vec<_>>();
        results.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.url.cmp(&right.url))
        });
        results.truncate(MAX_RESULTS);
        self.remember_results(query, &results);
        Ok(results)
    }

    async fn client(&self) -> Result<reqwest::Client, SearchError> {
        match &self.client {
            SearchClient::Managed { store, client } => client
                .get_or_try_init(|| async {
                    crate::network::client(store)
                        .await
                        .map_err(|error| SearchError(format!("HTTP client failed: {error}")))
                })
                .await
                .cloned(),
            SearchClient::Direct(client) => Ok(client.clone()),
        }
    }
}

impl WebSearch {
    fn cached_results(&self, query: &str) -> Option<Vec<SearchHit>> {
        let cache = self.cache.lock();
        let (at, hits) = cache.get(query)?;
        (at.elapsed() < CACHE_TTL).then(|| hits.clone())
    }

    fn remember_results(&self, query: &str, hits: &[SearchHit]) {
        let mut cache = self.cache.lock();
        if cache.len() >= CACHE_ENTRIES && !cache.contains_key(query) {
            let oldest = cache
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                cache.remove(&oldest);
            }
        }
        cache.insert(query.to_owned(), (Instant::now(), hits.to_vec()));
    }
}

impl Default for WebSearch {
    fn default() -> Self {
        Self::built_in()
    }
}

fn merge(merged: &mut HashMap<String, SearchHit>, engine: &'static str, results: Vec<SearchHit>) {
    for (rank, mut result) in results.into_iter().enumerate() {
        let score = 1.0 / (RRF_K + rank as f64 + 1.0);
        match merged.get_mut(&result.url) {
            Some(existing) => {
                // Reciprocal Rank Fusion sums one term per ranked list. One
                // engine listing a URL more than once is still one list, so
                // only its best rank counts; a later repeat contributes
                // nothing but its snippet.
                if !existing.engines.contains(&engine) {
                    existing.score += score;
                    existing.engines.push(engine);
                }
                if result.chunk.len() > existing.chunk.len() {
                    existing.chunk = result.chunk;
                }
            }
            None => {
                result.score = score;
                merged.insert(result.url.clone(), result);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(url: &str, engine: &'static str, chunk: &str) -> SearchHit {
        SearchHit::new("title", url, chunk, vec![engine])
    }

    fn rrf(rank: usize) -> f64 {
        1.0 / (RRF_K + rank as f64 + 1.0)
    }

    #[test]
    fn an_engine_that_lists_one_url_twice_contributes_a_single_rrf_term() {
        let mut merged = HashMap::new();
        merge(
            &mut merged,
            "duckduckgo",
            vec![
                hit("https://example.com/p", "duckduckgo", "short"),
                hit("https://example.com/p", "duckduckgo", "longer snippet"),
            ],
        );

        let entry = &merged["https://example.com/p"];
        assert_eq!(entry.engines, vec!["duckduckgo"]);
        assert_eq!(entry.score, rrf(0));
        assert_eq!(entry.chunk, "longer snippet");
    }

    #[test]
    fn two_engines_that_agree_on_a_url_still_sum_both_ranks() {
        let mut merged = HashMap::new();
        merge(
            &mut merged,
            "google",
            vec![hit("https://example.com/p", "google", "a")],
        );
        merge(
            &mut merged,
            "bing",
            vec![
                hit("https://example.com/other", "bing", "b"),
                hit("https://example.com/p", "bing", "c"),
            ],
        );

        let entry = &merged["https://example.com/p"];
        assert_eq!(entry.engines, vec!["google", "bing"]);
        assert_eq!(entry.score, rrf(0) + rrf(1));
    }

    #[test]
    fn agreement_between_engines_outranks_one_engine_repeating_itself() {
        let mut merged = HashMap::new();
        merge(
            &mut merged,
            "duckduckgo",
            vec![
                hit("https://example.com/repeated", "duckduckgo", "a"),
                hit("https://example.com/repeated", "duckduckgo", "b"),
                hit("https://example.com/agreed", "duckduckgo", "c"),
            ],
        );
        merge(
            &mut merged,
            "brave",
            vec![hit("https://example.com/agreed", "brave", "d")],
        );

        assert!(
            merged["https://example.com/agreed"].score
                > merged["https://example.com/repeated"].score
        );
    }

    /// 同 query 在 TTL 内不再出网:一批工具调用重复同一个查询时,23×N 个请求会退化成一次。
    #[tokio::test]
    async fn a_repeated_query_is_served_from_the_cache() {
        let stub = super::super::test_support::StubServer::fixed(
            axum::http::StatusCode::OK,
            "application/json",
            br#"{"items":[{"title":"Hit","url":"https://example.com/a","snippet":"s"}]}"#.to_vec(),
        )
        .await;
        let engine = super::super::JsonEngine::new(
            "stub",
            stub.url("/search?q={query}"),
            "/items",
            "/title",
            "/url",
            "/snippet",
            None,
        );
        let search = WebSearch::with_engines([engine]);
        // 缓存属于本次 WebSearch 实例；查询串仍保持唯一，便于断言命中次数。
        let query = format!("cache-probe-{}", std::process::id());

        let first = search.search(&query).await.unwrap();
        assert_eq!(stub.requests(), 1);

        let second = search.search(&query).await.unwrap();

        assert_eq!(second, first);
        assert_eq!(stub.requests(), 1, "TTL 内不该再次出网");
    }

    /// 过期的条目不再命中(直接测缓存函数,避免真的等 5 分钟)。
    #[test]
    fn an_expired_cache_entry_is_not_served() {
        let query = format!("expired-probe-{}", std::process::id());
        let search = WebSearch::with_engines(Vec::<SearchEngine>::new());
        search.remember_results(&query, &[hit("https://example.com/a", "stub", "a")]);
        assert!(search.cached_results(&query).is_some());

        search
            .cache
            .lock()
            .insert(query.clone(), (Instant::now() - CACHE_TTL, Vec::new()));

        assert!(search.cached_results(&query).is_none());
    }
}
