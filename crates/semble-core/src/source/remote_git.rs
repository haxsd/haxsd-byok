//! Restricted HTTPS Git acquisition for explicitly requested remote repositories.

use std::{
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use url::{Position, Url};

use crate::{Error, Result};

/// git 子进程的硬上限。`repo` 来自模型给出的工具参数,对端不答时工具调用会一直挂着;
/// 已有路径下 `git` 还可能停在凭据提示上,所以同时关掉交互式提示。
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

pub struct RemoteRepository {
    pub path: PathBuf,
    pub identity: String,
}

impl RemoteRepository {
    pub fn acquire(url: &str, cache_root: &Path) -> Result<Self> {
        let identity = checked_identity(url)?;
        let key = hex::encode(Sha256::digest(url.as_bytes()));
        let repos = cache_root.join("repos");
        std::fs::create_dir_all(&repos).map_err(|error| Error::io(&repos, error))?;
        let path = repos.join(key);
        if !path.join(".git").is_dir() {
            let temporary =
                repos.join(format!(".clone-{}-{}", std::process::id(), random_suffix()));
            let result = run_git(
                [
                    OsString::from("clone"),
                    OsString::from("--depth"),
                    OsString::from("1"),
                    // 进度输出会灌满管道;安静模式下只剩错误文本。
                    OsString::from("--quiet"),
                    OsString::from("--"),
                    OsString::from(url),
                    temporary.clone().into_os_string(),
                ],
                GIT_TIMEOUT,
            )?;
            if !result.status.success() {
                let _ = std::fs::remove_dir_all(&temporary);
                return Err(Error::Git(
                    String::from_utf8_lossy(&result.stderr).trim().to_owned(),
                ));
            }
            std::fs::rename(&temporary, &path).map_err(|error| Error::io(&path, error))?;
        }
        let output = run_git(
            [
                OsString::from("-C"),
                path.clone().into_os_string(),
                OsString::from("rev-parse"),
                OsString::from("HEAD"),
            ],
            GIT_TIMEOUT,
        )?;
        if !output.status.success() {
            return Err(Error::Git(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        let revision = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(Self {
            path,
            identity: format!("{identity}@{revision}"),
        })
    }
}

/// 只接受公开 https 仓库,并返回不含凭据的身份串(host:port/path)。
///
/// `repo` 是模型给出的参数:明文 http 会把内容暴露在链路上,URL 里的 userinfo 会被
/// `git clone` 写进 `.git/config` 落盘,私网与环回地址则把工具变成内网探测入口
/// (169.254.169.254 这类元数据端点尤其不能碰)。域名解析到私网不在这一层判定:git
/// 自己解析,这里只拦得住 URL 字面上就能看出来的那些。
fn checked_identity(url: &str) -> Result<String> {
    let parsed = Url::parse(url).map_err(|_| Error::UnsupportedUrl(url.to_owned()))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::UnsupportedUrl(url.to_owned()))?;
    if parsed.scheme() != "https" || !parsed.username().is_empty() || host_is_local(host) {
        return Err(Error::UnsupportedUrl(url.to_owned()));
    }
    Ok(parsed[Position::BeforeHost..Position::AfterPath].to_owned())
}

fn host_is_local(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.eq_ignore_ascii_case("localhost") || bare.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => blocked_v4(address),
        Ok(IpAddr::V6(address)) => blocked_v6(address),
        // 域名:是否解析到私网由 git/系统决定,这里不猜。
        Err(_) => false,
    }
}

fn blocked_v4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_broadcast()
        || address.is_multicast()
        || octets[0] == 0
        || (octets[0] == 100 && (64..128).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
}

fn blocked_v6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return blocked_v4(mapped);
    }
    let segments = address.segments();
    address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
}

/// 跑一次 git,带硬超时与"不要问凭据"的环境。超时后杀掉子进程,不让工具调用挂着。
fn run_git<const N: usize>(args: [OsString; N], timeout: Duration) -> Result<Output> {
    let mut child = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Git(error.to_string()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| Error::Git(error.to_string()));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Git(format!(
                    "git did not finish within {}s",
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(Error::Git(error.to_string())),
        }
    }
}

fn random_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| value.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_sources_reject_non_http_urls_before_git_runs() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            RemoteRepository::acquire("file:///tmp/repository", directory.path()),
            Err(Error::UnsupportedUrl(_))
        ));
        assert!(matches!(
            RemoteRepository::acquire("git@github.com:owner/repo.git", directory.path()),
            Err(Error::UnsupportedUrl(_))
        ));
    }

    /// `repo` 来自模型给出的工具参数,除了 https 与公开地址,别的都在这里挡掉。
    #[test]
    fn remote_sources_reject_plain_http_credentials_and_private_addresses() {
        let directory = tempfile::tempdir().unwrap();
        for url in [
            "http://github.com/owner/repo.git",
            "https://user:token@github.com/owner/repo.git",
            "https://user@github.com/owner/repo.git",
            "https://localhost/owner/repo.git",
            "https://api.localhost/owner/repo.git",
            "https://127.0.0.1/owner/repo.git",
            "https://127.1/owner/repo.git",
            "https://10.0.0.5/owner/repo.git",
            "https://172.16.4.2/owner/repo.git",
            "https://192.168.1.10/owner/repo.git",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/owner/repo.git",
            "https://[fd00::1]/owner/repo.git",
        ] {
            assert!(
                matches!(
                    RemoteRepository::acquire(url, directory.path()),
                    Err(Error::UnsupportedUrl(_))
                ),
                "{url} 必须在跑 git 之前被拒绝"
            );
        }
    }

    #[test]
    fn identity_keeps_host_and_path_but_drops_scheme_credentials_and_query() {
        assert_eq!(
            checked_identity("https://github.com/owner/repo.git?ref=main#readme").unwrap(),
            "github.com/owner/repo.git"
        );
        assert_eq!(
            checked_identity("https://github.com:8443/owner/repo.git").unwrap(),
            "github.com:8443/owner/repo.git"
        );
        assert_eq!(
            checked_identity("https://github.com/owner/repo.git").unwrap(),
            "github.com/owner/repo.git"
        );
    }
}
