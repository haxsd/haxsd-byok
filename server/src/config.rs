//! Loads and validates process-level server configuration.
use std::{env, fs, net::SocketAddr, path::PathBuf, time::Duration};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::{Error, Result};

const DATA_DIR_NAME: &str = ".haxsd-byok-devin-v3";
const DATABASE_FILE_NAME: &str = "haxsd-byok.db";
const DEV_MODE_FILE_NAME: &str = "dev-mode.json";
const DATA_DIR_ENV_NAME: &str = "HAXSD_BYOK_DATA_DIR";
const DATABASE_URL_ENV_NAME: &str = "HAXSD_BYOK_DATABASE_URL";
/// 显式允许监听非回环地址的开关（默认关闭）。见 `check_listen_addr`。
const ALLOW_REMOTE_LISTEN_ENV_NAME: &str = "HAXSD_BYOK_ALLOW_REMOTE_LISTEN";
const V0049_DATA_DIR_NAME: &str = ".cursor-local-assistant-v2";
const V0049_CONFIG_FILE_NAME: &str = "config.yaml";
const DEFAULT_PROVIDER_REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub fn managed_data_dir() -> Result<PathBuf> {
    if let Some(directory) = env::var_os(DATA_DIR_ENV_NAME) {
        let data_dir = isolated_data_dir(PathBuf::from(directory))?;
        fs::create_dir_all(&data_dir)?;
        return Ok(data_dir);
    }
    let home_dir = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve user home directory".into()))?;
    let data_dir = home_dir.join(DATA_DIR_NAME);
    fs::create_dir_all(&data_dir)?;
    #[cfg(unix)]
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700))?;
    Ok(data_dir)
}

fn isolated_data_dir(directory: PathBuf) -> Result<PathBuf> {
    if !directory.is_absolute() {
        return Err(Error::Config(format!(
            "{DATA_DIR_ENV_NAME} must be an absolute path"
        )));
    }
    Ok(directory)
}

/// 开发者模式镜像文件的路径。
pub fn dev_mode_file_path() -> Result<PathBuf> {
    Ok(managed_data_dir()?.join(DEV_MODE_FILE_NAME))
}

/// 读开发者模式镜像；文件缺失或损坏都按关闭处理（默认关）。
///
/// 日志过滤在数据库之前初始化，那时没有 store 可读；主窗口的 origin 又带随机端口，
/// localStorage 里的记录换端口就丢。于是"是否开发者模式"必须在数据目录里留一份文件，
/// 供启动早期读取。数据库里的设置是唯一事实来源，这里是它的镜像。
pub fn dev_mode_enabled() -> bool {
    let Ok(path) = dev_mode_file_path() else {
        return false;
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    serde_json::from_str::<DevModeMirror>(&raw)
        .map(|mirror| mirror.developer_mode)
        .unwrap_or(false)
}

/// 把开发者模式写进镜像文件；内容没变时不重写。
pub fn write_dev_mode_mirror(enabled: bool) -> Result<()> {
    let json = serde_json::to_string(&DevModeMirror {
        developer_mode: enabled,
    })?;
    let path = dev_mode_file_path()?;
    if let Ok(current) = fs::read_to_string(&path) {
        if current == json {
            return Ok(());
        }
    }
    fs::write(path, json)?;
    Ok(())
}

#[derive(serde::Deserialize, serde::Serialize)]
struct DevModeMirror {
    developer_mode: bool,
}

pub fn v0049_config_path() -> Result<PathBuf> {
    let home_dir = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve user home directory".into()))?;
    Ok(home_dir
        .join(V0049_DATA_DIR_NAME)
        .join(V0049_CONFIG_FILE_NAME))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

#[derive(Clone)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub request_url: String,
    pub api_key: String,
    pub custom_headers: reqwest::header::HeaderMap,
    pub max_output_tokens: Option<u64>,
    pub request_timeout: Duration,
}

#[derive(Clone)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub database_url: String,
    pub provider_request_timeout: Duration,
    pub provider_stream_idle_timeout: Duration,
    pub console: Option<ConsoleSource>,
    pub use_persisted_ports: bool,
    /// 面向用户的应用版本;桌面壳会覆盖为自身版本,用于插件 minAppVersion 门控。
    pub app_version: String,
}

#[derive(Clone)]
pub enum ConsoleSource {
    Directory(PathBuf),
    Proxy(url::Url),
}

/// 本机网关默认只服务本机：非回环监听地址必须显式打开开关，否则拒绝启动。
///
/// 网关后面是 Cursor 的接管配置、模型目录、账号信息与调用详情，而 Cursor 侧路由表本身没有
/// 独立的鉴权层（`api/cursor/handlers.rs`）。绑到 `0.0.0.0` 或某个具体网卡地址之后，同一网段
/// 的任何主机都能改写我们的接管配置、读调用详情、用已配置的 provider key 发请求——所以默认
/// 拒绝。报错里直接给出开关名、取值与后果，用户不必去翻文档；环境变量填了别的值时也停在
/// 拒绝这一侧（拼错 `1` 只会启动失败，不会静默外放）。
fn check_listen_addr(listen_addr: SocketAddr) -> Result<()> {
    check_listen_addr_with(listen_addr, allow_remote_listen())
}

/// 判据本体：与进程环境解耦，用例直接传开关取值（不去改环境变量，保证全量并行下稳定）。
fn check_listen_addr_with(listen_addr: SocketAddr, allow_remote: bool) -> Result<()> {
    if listen_addr.ip().is_loopback() || allow_remote {
        return Ok(());
    }
    Err(Error::Config(format!(
        "refusing to listen on {listen_addr}: the local gateway is loopback-only by default and \
         its Cursor routes have no separate authentication, so any host on the network could read \
         call details and rewrite the Cursor takeover configuration; set \
         {ALLOW_REMOTE_LISTEN_ENV_NAME}=1 only if you really need remote access"
    )))
}

/// 开关是否被显式打开：只有 `1` / `true` / `yes`（不分大小写、忽略首尾空白）算打开。
fn allow_remote_listen() -> bool {
    switch_value_allows_remote(env::var(ALLOW_REMOTE_LISTEN_ENV_NAME).ok().as_deref())
}

fn switch_value_allows_remote(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let listen_addr = env::var("CURSOR_LISTEN_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:3000".into())
            .parse()
            .map_err(|error| Error::Config(format!("invalid CURSOR_LISTEN_ADDR: {error}")))?;
        check_listen_addr(listen_addr)?;
        let request_timeout = match env::var("CURSOR_PROVIDER_TIMEOUT_SECONDS") {
            Ok(value) => Duration::from_secs(value.parse().map_err(|error| {
                Error::Config(format!("invalid CURSOR_PROVIDER_TIMEOUT_SECONDS: {error}"))
            })?),
            Err(env::VarError::NotPresent) => DEFAULT_PROVIDER_REQUEST_TIMEOUT,
            Err(error) => {
                return Err(Error::Config(format!(
                    "invalid CURSOR_PROVIDER_TIMEOUT_SECONDS: {error}"
                )))
            }
        };
        let console_dir = env::var_os("CURSOR_CONSOLE_DIR").map(PathBuf::from);
        let console_proxy = env::var("CURSOR_CONSOLE_PROXY")
            .ok()
            .map(|value| {
                value.parse().map_err(|error| {
                    Error::Config(format!("invalid CURSOR_CONSOLE_PROXY: {error}"))
                })
            })
            .transpose()?;
        let console = match (console_dir, console_proxy) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(
                    "CURSOR_CONSOLE_DIR and CURSOR_CONSOLE_PROXY cannot both be set".into(),
                ))
            }
            (Some(directory), None) => Some(ConsoleSource::Directory(directory)),
            (None, Some(proxy)) => Some(ConsoleSource::Proxy(proxy)),
            (None, None) => None,
        };
        Ok(Self {
            listen_addr,
            database_url: database_url_from_env()?,
            provider_request_timeout: request_timeout,
            provider_stream_idle_timeout: DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT,
            console,
            use_persisted_ports: false,
            app_version: env!("CARGO_PKG_VERSION").into(),
        })
    }

    pub fn desktop() -> Result<Self> {
        Ok(Self {
            listen_addr: "127.0.0.1:0"
                .parse()
                .expect("desktop listen address is static"),
            database_url: default_database_url()?,
            provider_request_timeout: DEFAULT_PROVIDER_REQUEST_TIMEOUT,
            provider_stream_idle_timeout: DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT,
            console: None,
            use_persisted_ports: true,
            app_version: env!("CARGO_PKG_VERSION").into(),
        })
    }
}

fn database_url_from_env() -> Result<String> {
    match env::var(DATABASE_URL_ENV_NAME) {
        Ok(database_url) => Ok(database_url),
        Err(env::VarError::NotPresent) => default_database_url(),
        Err(error) => Err(Error::Config(format!(
            "invalid {DATABASE_URL_ENV_NAME}: {error}"
        ))),
    }
}

fn default_database_url() -> Result<String> {
    let data_dir = managed_data_dir()?;
    database_url_for_dir(&data_dir)
}

fn database_url_for_dir(data_dir: &std::path::Path) -> Result<String> {
    let database_path = data_dir.join(DATABASE_FILE_NAME);
    let database_path = database_path
        .to_str()
        .ok_or_else(|| Error::Config("database path is not valid UTF-8".into()))?;
    Ok(format!("sqlite://{database_path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_directory_rejects_relative_paths() {
        assert!(isolated_data_dir(PathBuf::from("test-data")).is_err());
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            isolated_data_dir(temp.path().to_owned()).unwrap(),
            temp.path()
        );
    }

    #[test]
    fn provider_timeout_defaults_match_runtime_boundaries() {
        assert_eq!(
            DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT,
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            DEFAULT_PROVIDER_REQUEST_TIMEOUT,
            Duration::from_secs(60 * 60)
        );
    }

    #[test]
    fn haxsd_byok_uses_an_isolated_storage_identity() {
        assert_eq!(DATA_DIR_NAME, ".haxsd-byok-devin-v3");
        assert_eq!(DATABASE_FILE_NAME, "haxsd-byok.db");
        assert_eq!(DATA_DIR_ENV_NAME, "HAXSD_BYOK_DATA_DIR");
        assert_eq!(DATABASE_URL_ENV_NAME, "HAXSD_BYOK_DATABASE_URL");

        let temp = tempfile::tempdir().unwrap();
        assert!(database_url_for_dir(temp.path())
            .unwrap()
            .ends_with("haxsd-byok.db"));
    }

    fn addr(value: &str) -> SocketAddr {
        value.parse().unwrap()
    }

    /// 三态：回环放行、非回环默认拒绝、显式开关放行。
    #[test]
    fn a_non_loopback_listen_address_needs_an_explicit_switch() {
        // 回环（IPv4/IPv6）永远放行：默认形态就是它。
        for allowed in ["127.0.0.1:3000", "127.0.0.1:0", "[::1]:3000"] {
            assert!(
                check_listen_addr_with(addr(allowed), false).is_ok(),
                "{allowed} 是回环地址，必须放行"
            );
        }

        // 非回环默认拒绝，而且报错要能照做：带上开关名与后果。
        for refused in [
            "0.0.0.0:3000",
            "[::]:3000",
            "192.168.1.5:3000",
            "10.0.0.7:3000",
        ] {
            let error = check_listen_addr_with(addr(refused), false)
                .expect_err(&format!("{refused} 默认必须被拒绝"))
                .to_string();
            assert!(error.contains(refused), "{error}");
            assert!(error.contains(ALLOW_REMOTE_LISTEN_ENV_NAME), "{error}");
            assert!(error.contains("=1"), "{error}");
            assert!(error.contains("loopback-only"), "{error}");
        }

        // 显式开关打开之后放行。
        for allowed in ["0.0.0.0:3000", "192.168.1.5:3000"] {
            assert!(
                check_listen_addr_with(addr(allowed), true).is_ok(),
                "显式打开开关后 {allowed} 必须放行"
            );
        }
    }

    /// 开关的取值解析：只有明确的肯定值算打开，其它一律按关闭（拼错不会静默外放）。
    #[test]
    fn the_remote_listen_switch_only_accepts_an_explicit_yes() {
        assert!(switch_value_allows_remote(Some("1")));
        assert!(switch_value_allows_remote(Some("true")));
        assert!(switch_value_allows_remote(Some(" TRUE ")));
        assert!(switch_value_allows_remote(Some("yes")));

        assert!(!switch_value_allows_remote(None));
        assert!(!switch_value_allows_remote(Some("")));
        assert!(!switch_value_allows_remote(Some("0")));
        assert!(!switch_value_allows_remote(Some("false")));
        assert!(!switch_value_allows_remote(Some("on")));
    }

    /// 部署面回归：镜像绑的是 `0.0.0.0`，必须同时显式打开开关，否则容器一启动就退出。
    ///
    /// 这条用例钉的是"拒绝非回环监听"与本仓唯一一份容器配置之间的一致性——两边各改各的，
    /// 就会出现"本地全绿、镜像起不来"的回归（这一批正是这么发现的）。
    #[test]
    fn the_container_image_opts_into_remote_listen() {
        let dockerfile = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../Dockerfile"),
        )
        .expect("read the repository Dockerfile");
        // 取出设置监听地址的那一段 ENV（多行续行）：开关必须与它在同一段里。
        let lines = dockerfile.lines().collect::<Vec<_>>();
        let start = lines
            .iter()
            .position(|line| line.starts_with("ENV ") && line.contains("CURSOR_LISTEN_ADDR"))
            .expect("容器要设置监听地址");
        let mut block = String::new();
        for line in &lines[start..] {
            block.push_str(line);
            block.push('\n');
            if !line.trim_end().ends_with('\\') {
                break;
            }
        }
        assert!(
            block.contains("0.0.0.0") || block.contains("[::]"),
            "这条断言是为非回环监听准备的: {block}"
        );
        assert!(
            block.contains(&format!("{ALLOW_REMOTE_LISTEN_ENV_NAME}=1")),
            "绑非回环地址就必须在同一段 ENV 里显式打开开关，否则镜像启动即退出: {block}"
        );
        // 注释要说明为什么容器可以开这个开关（容器网络命名空间即信任边界）。
        assert!(
            dockerfile.contains("信任边界"),
            "Dockerfile 要写明为什么容器需要这个开关"
        );
    }
}
