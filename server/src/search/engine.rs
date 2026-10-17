//! Coordinates search and fetch operations.
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use url::Url;

#[derive(Clone, Debug)]
pub struct HtmlEngine {
    pub(crate) id: &'static str,
    url: String,
    result: String,
    title: String,
    link: String,
    snippet: String,
}

#[derive(Clone, Debug)]
pub struct JsonEngine {
    id: &'static str,
    url: String,
    items: &'static str,
    title: &'static str,
    link: &'static str,
    snippet: &'static str,
    link_template: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub enum SearchEngine {
    Html(HtmlEngine),
    Json(JsonEngine),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub chunk: String,
    pub engines: Vec<&'static str>,
    pub(crate) score: f64,
}

impl SearchHit {
    pub fn new(
        title: impl Into<String>,
        url: impl Into<String>,
        chunk: impl Into<String>,
        engines: Vec<&'static str>,
    ) -> Self {
        Self {
            title: title.into(),
            url: url.into(),
            chunk: chunk.into(),
            engines,
            score: 0.0,
        }
    }
}

impl HtmlEngine {
    pub fn new(
        id: &'static str,
        url: String,
        result: impl Into<String>,
        title: impl Into<String>,
        link: impl Into<String>,
        snippet: impl Into<String>,
    ) -> Self {
        Self {
            id,
            url,
            result: result.into(),
            title: title.into(),
            link: link.into(),
            snippet: snippet.into(),
        }
    }

    pub(crate) async fn search(
        &self,
        client: &reqwest::Client,
        query: &str,
    ) -> Result<Vec<SearchHit>, String> {
        let url = search_url(&self.url, query);
        let _permit = super::outbound::permit().await;
        let response = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, user_agent())
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1",
            )
            .timeout(Duration::from_secs(12))
            .send()
            .await
            .map_err(|error| format!("request failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let response_url = response.url().clone();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        // 与网页抓取共用 5 MiB 上限与同一套字符集解码(含 meta 嗅探与有损解码)。
        let body = super::fetch::limited_body(response)
            .await
            .map_err(|error| format!("response failed: {error}"))?;
        let body = super::fetch::decode(&body, &content_type)
            .map_err(|error| format!("response decoding failed: {error}"))?;
        self.parse(&body, &response_url)
    }

    fn parse(&self, body: &str, response_url: &Url) -> Result<Vec<SearchHit>, String> {
        let result = selector(&self.result)?;
        let title = selector(&self.title)?;
        let link = selector(&self.link)?;
        let snippet = selector(&self.snippet)?;
        let document = Html::parse_document(body);
        Ok(document
            .select(&result)
            .filter_map(|item| self.parse_item(item, &title, &link, &snippet, response_url))
            .take(10)
            .collect())
    }

    fn parse_item(
        &self,
        item: ElementRef<'_>,
        title: &Selector,
        link: &Selector,
        snippet: &Selector,
        response_url: &Url,
    ) -> Option<SearchHit> {
        let title = text(item.select(title).next()?);
        let href = item
            .select(link)
            .next()
            .and_then(|element| element.value().attr("href"))
            .or_else(|| item.value().attr("href"))?;
        let url = result_url(response_url, href)?;
        let chunk = item.select(snippet).next().map(text).unwrap_or_default();
        (!title.is_empty()).then_some(SearchHit::new(title, url, chunk, vec![self.id]))
    }
}

impl JsonEngine {
    pub fn new(
        id: &'static str,
        url: String,
        items: &'static str,
        title: &'static str,
        link: &'static str,
        snippet: &'static str,
        link_template: Option<&'static str>,
    ) -> Self {
        Self {
            id,
            url,
            items,
            title,
            link,
            snippet,
            link_template,
        }
    }

    async fn search(
        &self,
        client: &reqwest::Client,
        query: &str,
    ) -> Result<Vec<SearchHit>, String> {
        let _permit = super::outbound::permit().await;
        let response = client
            .get(search_url(&self.url, query))
            .header(reqwest::header::USER_AGENT, user_agent())
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(Duration::from_secs(12))
            .send()
            .await
            .map_err(|error| format!("request failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let body = super::fetch::limited_body(response)
            .await
            .map_err(|error| format!("response failed: {error}"))?;
        let body = serde_json::from_slice::<Value>(&body)
            .map_err(|error| format!("response failed: {error}"))?;
        let items = body
            .pointer(self.items)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("missing result array: {}", self.items))?;
        Ok(items
            .iter()
            .filter_map(|item| self.parse_item(item))
            .take(10)
            .collect())
    }

    fn parse_item(&self, item: &Value) -> Option<SearchHit> {
        let title = plain_text(&json_text(item.pointer(self.title)?));
        let link = json_text(item.pointer(self.link)?);
        let link = match self.link_template {
            Some(template) => template.replace(
                "{value}",
                &url::form_urlencoded::byte_serialize(link.as_bytes()).collect::<String>(),
            ),
            None => link,
        };
        let url = canonical_url(&link)?;
        let chunk = item
            .pointer(self.snippet)
            .map(json_text)
            .map(|value| plain_text(&value))
            .unwrap_or_default();
        (!title.is_empty()).then_some(SearchHit::new(title, url, chunk, vec![self.id]))
    }
}

impl SearchEngine {
    pub(crate) fn id(&self) -> &'static str {
        match self {
            Self::Html(engine) => engine.id,
            Self::Json(engine) => engine.id,
        }
    }

    pub(crate) async fn search(
        &self,
        client: &reqwest::Client,
        query: &str,
    ) -> Result<Vec<SearchHit>, String> {
        match self {
            Self::Html(engine) => engine.search(client, query).await,
            Self::Json(engine) => engine.search(client, query).await,
        }
    }
}

impl From<HtmlEngine> for SearchEngine {
    fn from(value: HtmlEngine) -> Self {
        Self::Html(value)
    }
}

impl From<JsonEngine> for SearchEngine {
    fn from(value: JsonEngine) -> Self {
        Self::Json(value)
    }
}

fn selector(value: &str) -> Result<Selector, String> {
    Selector::parse(value).map_err(|_| format!("invalid selector: {value}"))
}

fn text(element: ElementRef<'_>) -> String {
    element
        .text()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

fn search_url(template: &str, query: &str) -> String {
    template.replace(
        "{query}",
        &url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>(),
    )
}

fn user_agent() -> &'static str {
    // Identifies this product, not the sibling Cursor BYOK one it was forked from.
    "Mozilla/5.0 (compatible; haxsd-byok/0.1; +https://github.com)"
}

fn json_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => String::new(),
    }
}

fn plain_text(value: &str) -> String {
    let fragment = Html::parse_fragment(value);
    fragment
        .root_element()
        .text()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

fn canonical_url(value: &str) -> Option<String> {
    canonicalize(Url::parse(value).ok()?)
}

fn result_url(base: &Url, href: &str) -> Option<String> {
    canonicalize(base.join(href).ok()?)
}

fn canonicalize(mut url: Url) -> Option<String> {
    if let Some(target) = redirected_target(&url) {
        url = target;
    }
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    let retained = url
        .query_pairs()
        .filter(|(key, _)| {
            !key.starts_with("utm_")
                && !matches!(key.as_ref(), "gclid" | "fbclid" | "mc_cid" | "mc_eid")
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    if !retained.is_empty() {
        url.query_pairs_mut().extend_pairs(retained);
    }
    Some(url.to_string().trim_end_matches('/').to_string())
}

fn redirected_target(url: &Url) -> Option<Url> {
    let host = url.host_str()?;
    if host.contains("bing.com") && url.path() == "/ck/a" {
        return url
            .query_pairs()
            .find(|(name, _)| name == "u")
            .and_then(|(_, value)| value.strip_prefix("a1").map(str::to_string))
            .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
            .and_then(|value| String::from_utf8(value).ok())
            .and_then(|value| Url::parse(&value).ok());
    }
    let key = if host.contains("duckduckgo.com") {
        "uddg"
    } else if host.contains("google.") && url.path() == "/url" {
        "q"
    } else {
        return None;
    };
    url.query_pairs()
        .find(|(name, _)| name == key)
        .and_then(|(_, value)| Url::parse(&value).ok())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;
    use crate::search::test_support::StubServer;

    fn html_engine(stub: &StubServer) -> HtmlEngine {
        HtmlEngine::new(
            "stub",
            stub.url("/search?q={query}"),
            "div.result",
            "a.title",
            "a.title",
            "span.snippet",
        )
    }

    /// 搜索引擎的响应以前是 `response.text()`,没有任何体积上限;一个超大响应就能把服务端
    /// 内存撑起来。抓取侧的 5 MiB 上限现在由两边共用。
    #[tokio::test]
    async fn an_oversized_search_response_is_rejected_instead_of_read_into_memory() {
        let stub = StubServer::fixed(
            StatusCode::OK,
            "text/html",
            vec![b'x'; super::super::fetch::MAX_RESPONSE_SIZE + 1],
        )
        .await;

        let error = html_engine(&stub)
            .search(&reqwest::Client::new(), "q")
            .await
            .unwrap_err();

        assert!(error.contains("5 MiB"), "{error}");
    }

    /// 正常大小的响应仍然照常解析(上限不是把搜索引擎关掉)。
    #[tokio::test]
    async fn a_search_response_within_the_limit_is_parsed() {
        let stub = StubServer::fixed(
            StatusCode::OK,
            "text/html",
            br#"<html><body><div class="result"><a class="title" href="https://example.com/a">Title</a><span class="snippet">snip</span></div></body></html>"#
                .to_vec(),
        )
        .await;

        let hits = html_engine(&stub)
            .search(&reqwest::Client::new(), "q")
            .await
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/a");
        assert_eq!(hits[0].chunk, "snip");
    }

    /// 搜索结果里的跳转链接要还原成真实目标,并去掉跟踪参数与片段 —— 这些 URL 会被
    /// 模型当作后续 WebFetch 的输入。
    #[test]
    fn search_result_urls_are_unwrapped_and_cleaned() {
        let google =
            Url::parse("https://www.google.com/url?q=https://example.com/page&sa=U").unwrap();
        assert_eq!(canonicalize(google).unwrap(), "https://example.com/page");

        let duckduckgo =
            Url::parse("https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage").unwrap();
        assert_eq!(
            canonicalize(duckduckgo).unwrap(),
            "https://example.com/page"
        );

        let tracked = Url::parse("https://example.com/page?utm_source=x&keep=1#section").unwrap();
        assert_eq!(
            canonicalize(tracked).unwrap(),
            "https://example.com/page?keep=1"
        );

        let unsupported = Url::parse("ftp://example.com/file").unwrap();
        assert!(canonicalize(unsupported).is_none());
    }

    /// bing 的跳转把目标塞在 base64url 的 `u=a1...` 里。
    #[test]
    fn bing_redirect_targets_are_decoded_from_their_parameter() {
        let target = "https://example.com/bing-target";
        let encoded = URL_SAFE_NO_PAD.encode(target);
        let url = Url::parse(&format!("https://www.bing.com/ck/a?u=a1{encoded}")).unwrap();
        assert_eq!(canonicalize(url).unwrap(), target);
    }
}
