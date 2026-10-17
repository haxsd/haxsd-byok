//! Persists fetched web content and serves it from the existing server.
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::Router;
use parking_lot::RwLock;
use tower_http::services::ServeDir;
use uuid::Uuid;

use crate::{config::managed_data_dir, Error, Result};

const CACHE_ROUTE: &str = "/web-cache";

/// 保留窗口:抓取结果的提醒会让模型"稍后再回来读这份文件"(分页读全量正文),所以窗口
/// 不能短;7 天既覆盖一次正常会话的所有后续轮次,也覆盖用户隔天回来接着问。
const CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// 总量上限:单份最多 5 MiB,按实测强度(约 0.6 MB/天)这个上限够跑很久;它兜的是
/// "长期运行、只写不删"这个默认结局,而不是日常用量。
const CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// 清扫频率:目录扫描是纯本地操作,但没必要每次抓取都扫一遍。
const CACHE_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebCacheEntry {
    pub url: String,
    pub file_path: String,
    pub size_bytes: i64,
    pub line_count: i64,
}

#[derive(Clone, Default)]
pub struct WebCache {
    inner: Option<Arc<WebCacheInner>>,
}

struct WebCacheInner {
    directory: PathBuf,
    service_addr: RwLock<Option<SocketAddr>>,
    last_sweep_ms: AtomicU64,
}

impl WebCache {
    pub fn managed() -> Result<Self> {
        Self::at(managed_data_dir()?.join("cache").join("web"))
    }

    pub fn at(directory: PathBuf) -> Result<Self> {
        fs::create_dir_all(&directory)?;
        Ok(Self {
            inner: Some(Arc::new(WebCacheInner {
                directory,
                service_addr: RwLock::new(None),
                last_sweep_ms: AtomicU64::new(0),
            })),
        })
    }

    pub fn set_service_addr(&self, address: SocketAddr) {
        if let Some(inner) = &self.inner {
            *inner.service_addr.write() = Some(local_address(address));
        }
    }

    pub async fn store(&self, content: &str) -> Result<Option<WebCacheEntry>> {
        let Some(inner) = &self.inner else {
            return Ok(None);
        };
        let address = inner.service_addr.read().ok_or_else(|| {
            Error::Config("web cache is unavailable before the server starts listening".into())
        })?;
        let file_name = format!("{}.txt", Uuid::new_v4());
        let path = inner.directory.join(&file_name);
        let file_path = path.to_string_lossy().into_owned();
        let size_bytes = content.len() as i64;
        let line_count = content.lines().count() as i64;
        let bytes = content.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || fs::write(path, bytes))
            .await
            .map_err(|error| Error::Store(format!("web cache write task failed: {error}")))??;
        self.sweep_if_due().await;
        Ok(Some(WebCacheEntry {
            url: format!("http://{address}{CACHE_ROUTE}/{file_name}"),
            file_path,
            size_bytes,
            line_count,
        }))
    }

    /// 目录"只写不删"是这个功能的默认结局:UUID 命名一次一写、内容相同也不去重,而
    /// `cache/web` 既不进统计也不在界面上。回收挂在写入之后(一小时最多一次),这样不需要
    /// 维护循环认识这个目录;清扫按"保留窗口 + 总量上限"两条线走,先删过期,再按最旧优先
    /// 删到上限以内。
    async fn sweep_if_due(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        let now_ms = epoch_ms();
        let last_sweep_ms = inner.last_sweep_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last_sweep_ms) < CACHE_SWEEP_INTERVAL.as_millis() as u64 {
            return;
        }
        inner.last_sweep_ms.store(now_ms, Ordering::Relaxed);
        let directory = inner.directory.clone();
        let sweep =
            tokio::task::spawn_blocking(move || prune(&directory, CACHE_MAX_AGE, CACHE_MAX_BYTES))
                .await;
        match sweep {
            Ok(Ok(0)) => {}
            Ok(Ok(removed)) => tracing::info!(removed, "pruned the fetched web content cache"),
            Ok(Err(error)) => tracing::warn!(%error, "web content cache sweep failed"),
            Err(error) => tracing::warn!(%error, "web content cache sweep task failed"),
        }
    }

    pub fn router(&self) -> Router {
        let Some(inner) = &self.inner else {
            return Router::new();
        };
        Router::new().nest_service(CACHE_ROUTE, ServeDir::new(inner.directory.clone()))
    }

    #[cfg(test)]
    fn directory(&self) -> &std::path::Path {
        &self.inner.as_ref().expect("enabled web cache").directory
    }
}

fn local_address(address: SocketAddr) -> SocketAddr {
    match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), address.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), address.port())
        }
        _ => address,
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis() as u64)
}

/// 删掉超过 `max_age` 的文件;剩下的按最旧优先删到总量不超过 `max_bytes`。
/// 返回删除的文件数。
fn prune(directory: &Path, max_age: Duration, max_bytes: u64) -> std::io::Result<usize> {
    let now = SystemTime::now();
    let mut kept = Vec::new();
    let mut removed = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(now);
        // 时钟回拨时 duration_since 会报错;那种文件按"刚写过"处理,不删。
        if now.duration_since(modified).is_ok_and(|age| age > max_age) {
            if fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
            continue;
        }
        kept.push((entry.path(), modified, metadata.len()));
    }
    let mut total = kept.iter().map(|(_, _, len)| *len).sum::<u64>();
    if total > max_bytes {
        kept.sort_by_key(|(_, modified, _)| *modified);
        for (path, _, len) in kept {
            if total <= max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
                removed += 1;
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use tempfile::tempdir;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;

    fn write_with_age(path: &Path, content: &[u8], age: Duration) {
        fs::write(path, content).unwrap();
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    #[tokio::test]
    async fn stores_uuid_named_content_and_serves_it_from_existing_router() {
        let directory = tempdir().unwrap();
        let cache = WebCache::at(directory.path().join("cache").join("web")).unwrap();
        cache.set_service_addr("0.0.0.0:4312".parse().unwrap());

        let entry = cache
            .store("complete fetched content")
            .await
            .unwrap()
            .unwrap();
        let name = entry.url.rsplit('/').next().unwrap();
        let id = name.strip_suffix(".txt").unwrap();
        assert!(Uuid::parse_str(id).is_ok());
        assert_eq!(
            std::fs::read_to_string(cache.directory().join(name)).unwrap(),
            "complete fetched content"
        );
        assert_eq!(entry.url, format!("http://127.0.0.1:4312/web-cache/{name}"));
        assert_eq!(entry.size_bytes, 24);
        assert_eq!(entry.line_count, 1);

        let response = cache
            .router()
            .oneshot(
                Request::get(format!("/web-cache/{name}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "complete fetched content"
        );
    }

    /// 清扫的两条线:过期的先删,剩下的按最旧优先删到总量上限以内。
    #[test]
    fn the_sweep_drops_expired_files_then_the_oldest_ones_over_the_size_cap() {
        let root = tempdir().unwrap();
        let directory = root.path().join("web");
        fs::create_dir_all(&directory).unwrap();
        write_with_age(
            &directory.join("expired.txt"),
            b"xxxxx",
            CACHE_MAX_AGE + Duration::from_secs(60),
        );
        write_with_age(
            &directory.join("old.txt"),
            b"aaaaa",
            Duration::from_secs(600),
        );
        write_with_age(&directory.join("new.txt"), b"bbbbb", Duration::from_secs(1));

        // 上限 6 字节:两个保留文件共 10 字节,只能留下最新的那个。
        let removed = prune(&directory, CACHE_MAX_AGE, 6).unwrap();

        assert_eq!(removed, 2);
        assert!(!directory.join("expired.txt").exists());
        assert!(!directory.join("old.txt").exists());
        assert!(directory.join("new.txt").exists());
    }

    #[test]
    fn the_sweep_keeps_everything_inside_the_window_and_cap() {
        let root = tempdir().unwrap();
        let directory = root.path().join("web");
        fs::create_dir_all(&directory).unwrap();
        write_with_age(&directory.join("a.txt"), b"aaaaa", Duration::from_secs(60));
        write_with_age(&directory.join("b.txt"), b"bbbbb", Duration::from_secs(30));

        assert_eq!(prune(&directory, CACHE_MAX_AGE, 1024).unwrap(), 0);
        assert!(directory.join("a.txt").exists());
        assert!(directory.join("b.txt").exists());
    }

    /// 抓取路径(store)自己负责回收:没有维护循环认识这个目录。
    #[tokio::test]
    async fn storing_content_prunes_the_directory_once_the_sweep_is_due() {
        let root = tempdir().unwrap();
        let directory = root.path().join("cache").join("web");
        let cache = WebCache::at(directory.clone()).unwrap();
        cache.set_service_addr("0.0.0.0:4312".parse().unwrap());
        write_with_age(
            &directory.join("expired.txt"),
            b"xxxxx",
            CACHE_MAX_AGE + Duration::from_secs(60),
        );

        let entry = cache.store("fresh content").await.unwrap().unwrap();

        assert!(!directory.join("expired.txt").exists());
        assert_eq!(
            fs::read_to_string(
                cache
                    .directory()
                    .join(entry.url.rsplit('/').next().unwrap())
            )
            .unwrap(),
            "fresh content"
        );
    }
}
