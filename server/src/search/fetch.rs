//! Fetches and extracts web content.
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use bytes::BytesMut;
use dom_smoothie::{Config, Readability, TextMode};
use futures_util::StreamExt;
use reqwest::{
    header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, USER_AGENT},
    redirect::Policy,
    Response,
};
use tokio::{net::lookup_host, time::timeout};
use url::{Host, Url};

use crate::store::Store;

use super::{WebCache, WebCacheEntry};

/// 抓取与搜索引擎响应共用同一条体积上限:两条路径都是"把远端字节读进内存",抓取侧一直
/// 有 5 MiB,搜索引擎侧原来直接 `response.text()`/`response.json()`(没有任何上限),
/// 一个超大响应就能把服务端内存撑起来。
pub(super) const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedPage {
    pub url: String,
    pub markdown: String,
    pub cache: Option<WebCacheEntry>,
}

#[derive(Debug, thiserror::Error)]
#[error("web fetch failed: {0}")]
pub struct FetchError(String);

#[derive(Clone)]
pub struct WebFetch {
    client: FetchClient,
    cache: WebCache,
}

#[derive(Clone)]
enum FetchClient {
    Managed(Store),
    Direct,
}

impl WebFetch {
    pub fn built_in() -> Self {
        Self {
            client: FetchClient::Direct,
            cache: WebCache::default(),
        }
    }

    pub(crate) fn managed(store: Store, cache: WebCache) -> Self {
        Self {
            client: FetchClient::Managed(store),
            cache,
        }
    }

    pub async fn fetch(&self, value: &str) -> Result<FetchedPage, FetchError> {
        let mut page = timeout(FETCH_TIMEOUT, self.fetch_inner(value))
            .await
            .map_err(|_| failure("request timed out"))??;
        self.attach_cache(&mut page).await;
        Ok(page)
    }

    /// 缓存是纯副作用:正文已经在手里了,写不进缓存只该降级(模型拿不到"分页读全量正文"
    /// 的入口),不该把整次抓取判死。`WebCache::store` 在服务未就绪或磁盘写失败时返回 Err,
    /// 这里一律记 warn 并保持 `cache = None`,让 `complete_web_fetch` 走无缓存分支。
    async fn attach_cache(&self, page: &mut FetchedPage) {
        match self.cache.store(&page.markdown).await {
            Ok(entry) => page.cache = entry,
            Err(error) => {
                // 失败时不能留下任何位置:提醒与 OutputLocation 都从 `page.cache` 派生,
                // 指向别的文件会让模型去读不属于这次抓取的内容。
                page.cache = None;
                tracing::warn!(
                    url = %page.url,
                    %error,
                    "fetched page could not be written to the web cache; returning it without a cache location"
                );
            }
        }
    }

    async fn fetch_inner(&self, value: &str) -> Result<FetchedPage, FetchError> {
        let mut clients = ClientCache::default();
        let mut url = parse_url(value)?;
        for redirect in 0..=MAX_REDIRECTS {
            let response = self.request(&url, &mut clients).await?;
            if response.status().is_redirection() {
                if redirect == MAX_REDIRECTS {
                    return Err(failure("too many redirects"));
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| failure("redirect is missing Location"))?;
                url = parse_url(
                    url.join(location)
                        .map_err(|error| failure(format!("invalid redirect: {error}")))?
                        .as_str(),
                )?;
                continue;
            }
            if !response.status().is_success() {
                return Err(failure(format!("HTTP {}", response.status())));
            }
            return page(response).await;
        }
        unreachable!("redirect loop always returns")
    }

    async fn request(&self, url: &Url, clients: &mut ClientCache) -> Result<Response, FetchError> {
        // 出网名额在整段请求(含 DNS 与正文读取)期间持有;抓取路径的 30s 总超时也覆盖
        // 排队时间,所以极端拥塞下用户看到的是明确的超时,而不是无限期排队。
        let _permit = super::outbound::permit().await;
        let host = url
            .host_str()
            .ok_or_else(|| failure("URL is missing a host"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| failure("URL has no usable port"))?;
        let addresses = lookup_host((host, port))
            .await
            .map_err(|error| failure(format!("DNS lookup failed: {error}")))?
            .collect::<Vec<_>>();
        if addresses.is_empty() {
            return Err(failure("DNS lookup returned no addresses"));
        }
        let domain = matches!(url.host(), Some(Host::Domain(_)));
        if addresses
            .iter()
            .any(|address| !safe_resolution(address.ip(), domain))
        {
            return Err(failure("URL resolves to a non-public address"));
        }

        // 一次抓取内的重定向常常落在同一个 host 上:复用已经建好的客户端,别每跳重建一次
        // 连接池与 TLS 配置;解析结果变了就必须重建,否则会把请求发到已经作废的地址。
        let key = format!("{host}:{port}");
        let client = match clients.cached(&key, &addresses) {
            Some(client) => client,
            None => {
                let builder = match &self.client {
                    FetchClient::Managed(store) => crate::network::client_builder(store)
                        .await
                        .map_err(|error| failure(format!("HTTP client failed: {error}")))?,
                    FetchClient::Direct => reqwest::Client::builder().use_native_tls(),
                };
                let mut builder = builder
                    .redirect(Policy::none())
                    .connect_timeout(Duration::from_secs(10));
                if domain {
                    builder = builder.resolve_to_addrs(host, &addresses);
                }
                let client = builder
                    .build()
                    .map_err(|error| failure(format!("HTTP client failed: {error}")))?;
                clients.remember(&key, &addresses, client.clone());
                client
            }
        };
        client
            .get(url.clone())
            .header(
                USER_AGENT,
                "Mozilla/5.0 (compatible; haxsd-byok/0.1; +https://github.com)",
            )
            .header(
                ACCEPT,
                "text/markdown, text/plain;q=0.9, text/html;q=0.8, application/xhtml+xml;q=0.8, application/json;q=0.7, */*;q=0.1",
            )
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .send()
            .await
            .map_err(|error| failure(format!("request failed: {error}")))
    }
}

impl Default for WebFetch {
    fn default() -> Self {
        Self::built_in()
    }
}

/// 一次抓取内按 `host:port` 记住已经建好并 pin 过地址的客户端。
#[derive(Default)]
struct ClientCache {
    entries: HashMap<String, (Vec<SocketAddr>, reqwest::Client)>,
}

impl ClientCache {
    fn cached(&self, key: &str, addresses: &[SocketAddr]) -> Option<reqwest::Client> {
        let (pinned, client) = self.entries.get(key)?;
        (pinned == addresses).then(|| client.clone())
    }

    fn remember(&mut self, key: &str, addresses: &[SocketAddr], client: reqwest::Client) {
        self.entries
            .insert(key.to_owned(), (addresses.to_vec(), client));
    }
}

async fn page(response: Response) -> Result<FetchedPage, FetchError> {
    let url = response.url().to_string();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_RESPONSE_SIZE)
    {
        return Err(failure("response exceeds 5 MiB"));
    }
    let body = limited_body(response).await.map_err(failure)?;
    let text = decode(&body, &content_type)?;
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let markdown = match media_type.as_str() {
        "text/html" | "application/xhtml+xml" => {
            let source_url = url.clone();
            tokio::task::spawn_blocking(move || readable_markdown(&text, &source_url))
                .await
                .map_err(|error| failure(format!("content task failed: {error}")))??
        }
        "text/markdown" | "text/x-markdown" | "text/plain" => text,
        "application/json" => format!("```json\n{text}\n```"),
        "application/xml" | "text/xml" => format!("```xml\n{text}\n```"),
        value if value.starts_with("text/") => text,
        _ => return Err(failure(format!("unsupported content type: {media_type}"))),
    };
    if markdown.trim().is_empty() {
        return Err(failure("response contains no readable content"));
    }
    Ok(FetchedPage {
        url,
        markdown,
        cache: None,
    })
}

/// 收下一块字节,超过上限即失败。抽成纯函数是为了能对边界本身做单测。
pub(super) fn extend_bounded(body: &mut BytesMut, chunk: &[u8]) -> std::result::Result<(), String> {
    if body.len() + chunk.len() > MAX_RESPONSE_SIZE {
        return Err(format!(
            "response exceeds {} MiB",
            MAX_RESPONSE_SIZE / (1024 * 1024)
        ));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

pub(super) async fn limited_body(response: Response) -> std::result::Result<BytesMut, String> {
    let mut body = BytesMut::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("response failed: {error}"))?;
        extend_bounded(&mut body, &chunk)?;
    }
    Ok(body)
}

fn readable_markdown(html: &str, url: &str) -> Result<String, FetchError> {
    let mut readability = Readability::new(
        html,
        Some(url),
        Some(Config {
            max_elements_to_parse: 50_000,
            text_mode: TextMode::Markdown,
            ..Default::default()
        }),
    )
    .map_err(|error| failure(format!("HTML parse failed: {error}")))?;
    let article = readability
        .parse()
        .map_err(|error| failure(format!("article extraction failed: {error}")))?;
    let body = article.text_content.trim().to_string();
    let title = article.title.trim();
    let heading = format!("# {title}");
    Ok(if title.is_empty() || body.starts_with(&heading) {
        body
    } else {
        format!("# {title}\n\n{body}")
    })
}

pub(super) fn decode(bytes: &[u8], content_type: &str) -> Result<String, FetchError> {
    let encoding = match charset_label(content_type) {
        Some(label) => encoding_rs::Encoding::for_label(label.as_bytes())
            .ok_or_else(|| failure(format!("unsupported charset: {label}")))?,
        // 没有 charset 声明时不能直接假定 UTF-8:旧站点大量存在只在 <meta> 里声明编码的
        // 页面(GBK/Shift_JIS)。先按 HTML 自己的约定嗅探,再退回 UTF-8。
        None => sniff_charset(bytes).unwrap_or(encoding_rs::UTF_8),
    };
    // `decode` 自带 BOM 嗅探,并且是**有损**解码:非法字节变成 U+FFFD,而不是让整页失败。
    // 以前这里把 malformed 当致命错误,结果是"没有 charset 声明的非 UTF-8 页面一律抓不到"。
    let (text, _, malformed) = encoding.decode(bytes);
    if malformed {
        tracing::warn!(
            encoding = encoding.name(),
            "response is not valid for its declared encoding; decoded lossily"
        );
    }
    Ok(text.into_owned())
}

fn charset_label(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.trim().split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(['\'', '"']))
    })
}

/// 在文档开头找 `<meta charset=...>` 或 `content="text/html; charset=..."`。
/// 只扫前 4 KiB,与 HTML 规范给预扫描窗口同一个量级。
fn sniff_charset(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    const WINDOW: usize = 4096;
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(WINDOW)]).to_ascii_lowercase();
    let index = head.find("charset")?;
    let rest = head[index + "charset".len()..].trim_start();
    let label = rest
        .strip_prefix('=')?
        .trim_start()
        .trim_start_matches(['"', '\''])
        .split(|character: char| {
            character.is_ascii_whitespace() || matches!(character, '"' | '\'' | ';' | '>' | '/')
        })
        .next()
        .filter(|label| !label.is_empty())?;
    encoding_rs::Encoding::for_label(label.as_bytes())
}

/// Semble 的 `repo` 是模型给出的参数,直接交给 `git clone` 出网,所以必须套用与网页抓取
/// 同一套"只访问公网地址"的判定:明文 http 会把内容暴露在链路上,环回/私网/链路本地地址
/// 会把只读工具变成内网探测入口(云元数据端点尤其不能碰)。
///
/// 域名在这里解析一次并逐个判定;`git` 自己还会再解析一次,DNS 重绑定这条时序挡不住,
/// 但字面量内网地址与明文 scheme 在这一层就被拦下,与依赖层的 https 校验形成两道闸门。
pub(crate) async fn check_remote_git_url(value: &str) -> std::result::Result<(), String> {
    let url = Url::parse(value).map_err(|error| format!("invalid Git URL: {error}"))?;
    if url.scheme() != "https" {
        return Err("Git repository URL must use https".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Git repository URL credentials are not allowed".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "Git repository URL is missing a host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "Git repository URL has no usable port".to_string())?;
    let addresses = lookup_host((host, port))
        .await
        .map_err(|error| format!("DNS lookup failed: {error}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err("DNS lookup returned no addresses".into());
    }
    let domain = matches!(url.host(), Some(Host::Domain(_)));
    if addresses
        .iter()
        .any(|address| !safe_resolution(address.ip(), domain))
    {
        return Err("Git repository URL resolves to a non-public address".into());
    }
    Ok(())
}

fn parse_url(value: &str) -> Result<Url, FetchError> {
    let url = Url::parse(value).map_err(|error| failure(format!("invalid URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(failure("URL must use http or https"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(failure("URL credentials are not allowed"));
    }
    if url.host_str().is_none() {
        return Err(failure("URL is missing a host"));
    }
    Ok(url)
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn safe_resolution(ip: IpAddr, domain: bool) -> bool {
    is_public(ip) || (domain && is_benchmark_proxy_range(ip))
}

fn is_benchmark_proxy_range(ip: IpAddr) -> bool {
    let IpAddr::V4(ip) = ip else {
        return false;
    };
    u32::from(ip) >> 17 == u32::from(Ipv4Addr::new(198, 18, 0, 0)) >> 17
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let value = u32::from(ip);
    ![
        (0x0000_0000, 8),
        (0x0a00_0000, 8),
        (0x6440_0000, 10),
        (0x7f00_0000, 8),
        (0xa9fe_0000, 16),
        (0xac10_0000, 12),
        (0xc000_0000, 24),
        (0xc000_0200, 24),
        (0xc0a8_0000, 16),
        (0xc612_0000, 15),
        (0xc633_6400, 24),
        (0xcb00_7100, 24),
        (0xe000_0000, 3),
    ]
    .into_iter()
    .any(|(network, prefix)| value >> (32 - prefix) == network >> (32 - prefix))
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    segments[0] & 0xe000 == 0x2000 && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
}

fn failure(message: impl Into<String>) -> FetchError {
    FetchError(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缓存写失败(这里用"服务未就绪"这条 Err 分支代表)不能把已经抓到的正文一起丢掉。
    #[tokio::test]
    async fn a_failed_cache_write_still_returns_the_fetched_page() {
        let directory = tempfile::tempdir().unwrap();
        let fetch = WebFetch {
            client: FetchClient::Direct,
            cache: WebCache::at(directory.path().join("cache").join("web")).unwrap(),
        };
        let mut page = FetchedPage {
            url: "https://example.com/final".into(),
            markdown: "complete fetched content".into(),
            // 未 set_service_addr:store 返回 Err(Config),等价于缓存不可用。
            cache: Some(WebCacheEntry {
                url: "stale".into(),
                file_path: "stale".into(),
                size_bytes: 0,
                line_count: 0,
            }),
        };

        fetch.attach_cache(&mut page).await;

        assert_eq!(page.markdown, "complete fetched content");
        assert!(
            page.cache.is_none(),
            "缓存不可用时必须走无缓存分支,而不是留下半截条目"
        );
    }

    #[test]
    fn a_meta_declared_charset_is_used_when_the_header_has_none() {
        let html = b"<html><head><meta charset=\"shift_jis\"></head><body>\x82\xa0</body></html>";
        let text = decode(html, "text/html").unwrap();
        assert!(text.contains('\u{3042}'), "{text}");

        let legacy =
            b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=gbk\">\xd6\xd0";
        let text = decode(legacy, "text/html").unwrap();
        assert!(text.contains('\u{4e2d}'), "{text}");
    }

    #[test]
    fn malformed_bytes_are_decoded_lossily_instead_of_failing_the_fetch() {
        let text = decode(b"caf\xe9", "text/html").unwrap();
        assert!(text.starts_with("caf"));
        assert!(text.contains('\u{fffd}'), "{text}");

        // 声明了 UTF-8 但正文里有非法字节:同样降级,不再整页失败。
        let text = decode(b"caf\xe9", "text/html; charset=utf-8").unwrap();
        assert!(text.contains('\u{fffd}'), "{text}");
    }

    #[test]
    fn a_bom_still_wins_over_the_utf8_default() {
        let text = decode(b"\xff\xfea\x00", "text/html").unwrap();
        assert_eq!(text, "a");
    }

    #[test]
    fn an_unknown_declared_charset_is_still_rejected() {
        assert!(decode(b"x", "text/html; charset=definitely-not-an-encoding").is_err());
    }

    #[tokio::test]
    async fn remote_git_urls_are_held_to_the_web_fetch_rules() {
        for url in [
            "http://github.com/owner/repo.git",
            "https://user:token@github.com/owner/repo.git",
            "file:///tmp/repository",
            "https://127.0.0.1/owner/repo.git",
            "https://[::1]/owner/repo.git",
            "https://169.254.169.254/latest/meta-data",
            "https://10.0.0.5/owner/repo.git",
            "not a url",
        ] {
            assert!(
                check_remote_git_url(url).await.is_err(),
                "{url} 必须在出网之前被拒绝"
            );
        }
        // 公网字面量地址必须放行,否则这条闸门就成了"关掉 Semble"。
        assert!(check_remote_git_url("https://93.184.216.34/owner/repo.git")
            .await
            .is_ok());
    }

    /// 公网判定是抓取与 Semble 共用的安全边界,边界值必须逐条钉住。
    #[test]
    fn the_public_address_table_matches_its_documented_boundaries() {
        for blocked in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.5",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "239.255.255.255",
        ] {
            let address: IpAddr = blocked.parse().unwrap();
            assert!(!is_public(address), "{blocked} 不该被当成公网地址");
        }
        for public in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "100.128.0.1",
            "172.32.0.1",
            "198.20.0.1",
        ] {
            let address: IpAddr = public.parse().unwrap();
            assert!(is_public(address), "{public} 是公网地址");
        }

        for blocked in [
            "::1",
            "fd00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
        ] {
            let address: IpAddr = blocked.parse().unwrap();
            assert!(!is_public(address), "{blocked} 不该被当成公网地址");
        }
        for public in ["2606:4700::1111", "2001:4860:4860::8888"] {
            let address: IpAddr = public.parse().unwrap();
            assert!(is_public(address), "{public} 是公网地址");
        }
    }

    /// 基准代理段只对域名开放:字面量地址仍然要过公网判定。
    #[test]
    fn the_benchmark_proxy_range_is_only_opened_for_domains() {
        let proxy: IpAddr = "198.18.7.7".parse().unwrap();
        assert!(!safe_resolution(proxy, false));
        assert!(safe_resolution(proxy, true));
        assert!(!safe_resolution("127.0.0.1".parse().unwrap(), true));
    }

    #[test]
    fn only_plain_http_urls_without_credentials_are_fetched() {
        assert!(parse_url("https://example.com/a").is_ok());
        for rejected in [
            "ftp://example.com/a",
            "file:///etc/passwd",
            "https://user:token@example.com/a",
            "https://example.com:not-a-port/a",
        ] {
            assert!(parse_url(rejected).is_err(), "{rejected} 必须被拒绝");
        }
    }

    #[test]
    fn the_body_limit_rejects_the_first_chunk_that_crosses_it() {
        let mut body = BytesMut::new();
        extend_bounded(&mut body, &vec![b'x'; MAX_RESPONSE_SIZE]).unwrap();

        assert!(extend_bounded(&mut body, b"x").is_err());
        assert_eq!(body.len(), MAX_RESPONSE_SIZE);
    }

    /// 一次抓取内复用客户端的前提是"pin 的地址没变";地址变了必须重建,
    /// 否则请求会被发到已经作废的地址。
    #[test]
    fn a_client_is_reused_only_while_the_pinned_addresses_match() {
        let mut cache = ClientCache::default();
        let addresses = vec!["93.184.216.34:443".parse().unwrap()];

        assert!(cache.cached("example.com:443", &addresses).is_none());
        cache.remember("example.com:443", &addresses, reqwest::Client::new());

        assert!(cache.cached("example.com:443", &addresses).is_some());
        let rebound = vec!["93.184.216.35:443".parse().unwrap()];
        assert!(
            cache.cached("example.com:443", &rebound).is_none(),
            "解析结果变了必须重建"
        );
        assert!(cache.cached("other.example:443", &addresses).is_none());
    }
}
