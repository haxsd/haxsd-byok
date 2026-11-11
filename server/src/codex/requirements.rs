//! Codex 受管 requirements 的本地只读投影。
//!
//! 官方 0.167（commit c40729ea）把 requirements 当作独立约束层：`apply_exact_to_config`
//! 用 requirements 的 `model_provider`、`model_catalog_json` 精确覆盖用户配置，并把
//! `model_providers` 的每个条目按名字替换（`codex-rs/config/src/config_requirements.rs`）。
//! 本模块只提取会改变本机请求出口的三个字段；审批、沙箱等约束原样留给 Codex，
//! 不解析、不写入、也不试图绕过它们。

use crate::{Error, Result};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item};

/// requirements 的来源。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequirementsSource {
    /// 没有受管来源：测试与未接入受管策略的装配使用它。
    Unmanaged,
    /// 明确的本地 `requirements.toml`；文件不存在表示没有受管要求。
    File(PathBuf),
    /// 生产系统来源：每次 `load` 重新解析注册表/ProgramData 路径，不缓存启动时的路径。
    System,
}

impl RequirementsSource {
    pub fn unmanaged() -> Self {
        Self::Unmanaged
    }

    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File(path.into())
    }

    /// 生产来源：每次 `load` 重新解析本机路径并读取内容，不缓存启动时的路径。
    ///
    /// 与官方 0.167 的 Windows 解析顺序一致：先看
    /// `HKLM\SOFTWARE\Policies\OpenAI\Codex\RequirementsPath`，否则用
    /// `%ProgramData%\OpenAI\Codex\requirements.toml`。测试不能走这里——它们必须注入
    /// 明确的临时文件，避免读到真实机器的策略。
    pub fn system() -> Self {
        Self::System
    }

    pub(crate) fn load(&self) -> Result<ManagedRequirements> {
        match self {
            Self::Unmanaged => Ok(ManagedRequirements::default()),
            Self::File(path) => load_requirements_file(path),
            Self::System => load_requirements_file(&system_requirements_path()),
        }
    }
}

fn load_requirements_file(path: &Path) -> Result<ManagedRequirements> {
    if !path.is_absolute() {
        return Err(Error::Config(
            "Codex requirements path must be absolute".into(),
        ));
    }
    match fs::read_to_string(path) {
        Ok(text) => ManagedRequirements::parse(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ManagedRequirements::default())
        }
        Err(error) => Err(error.into()),
    }
}

/// requirements 中与本次发布路由有关的那部分。
#[derive(Debug, Default)]
pub(crate) struct ManagedRequirements {
    model_provider: Option<String>,
    model_catalog_json: Option<PathBuf>,
    model_providers: HashMap<String, ProviderRoute>,
}

impl ManagedRequirements {
    fn parse(text: &str) -> Result<Self> {
        let document = text.parse::<DocumentMut>().map_err(|error| {
            // 同 config：TomlError 的 Display 含源行，这里只返回安全的解析原因。
            Error::Config(format!(
                "invalid Codex requirements TOML: {}",
                error.message()
            ))
        })?;
        let model_provider = string_field(document.get("model_provider"), "model_provider")?;
        let model_catalog_json =
            string_field(document.get("model_catalog_json"), "model_catalog_json")?
                .map(PathBuf::from);
        let model_providers = match document.get("model_providers") {
            None => HashMap::new(),
            Some(item) => {
                let providers = item.as_table_like().ok_or_else(|| {
                    Error::Config("Codex requirements model_providers must be a table".into())
                })?;
                let mut parsed = HashMap::new();
                for (name, entry) in providers.iter() {
                    parsed.insert(name.to_owned(), ProviderRoute::parse(entry, name)?);
                }
                parsed
            }
        };
        Ok(Self {
            model_provider,
            model_catalog_json,
            model_providers,
        })
    }

    /// 管理员要求与本次发布路由的差异；空表示要求没有改变请求出口。
    pub(crate) fn route_conflicts(
        &self,
        published: &PublishedRoute,
    ) -> Vec<ManagedRequirementConflict> {
        let mut conflicts = Vec::new();
        if let Some(required) = &self.model_provider {
            if required != &published.provider {
                conflicts.push(ManagedRequirementConflict {
                    field: "model_provider".into(),
                    required: required.clone(),
                    published: published.provider.clone(),
                });
            }
        }
        if let Some(required) = &self.model_catalog_json {
            if required != &published.catalog {
                conflicts.push(ManagedRequirementConflict {
                    field: "model_catalog_json".into(),
                    required: required.display().to_string(),
                    published: published.catalog.display().to_string(),
                });
            }
        }
        if let Some(required) = self.model_providers.get(&published.provider) {
            let published_provider = published.provider_route();
            if required != &published_provider {
                conflicts.push(ManagedRequirementConflict {
                    field: format!("model_providers.{}", published.provider),
                    required: render_provider(required),
                    published: render_provider(&published_provider),
                });
            }
        }
        conflicts
    }
}

/// 本应用发布的路由：请求出口的三个坐标 + 目录指针。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublishedRoute {
    pub provider: String,
    pub catalog: PathBuf,
    pub base_url: String,
    pub token: String,
}

impl PublishedRoute {
    fn provider_route(&self) -> ProviderRoute {
        ProviderRoute {
            base_url: Some(self.base_url.clone()),
            env_key: None,
            experimental_bearer_token: Some(self.token.clone()),
            wire_api: "responses".into(),
            requires_openai_auth: false,
        }
    }
}

/// 一项真正改变受管路由的要求。凭据在渲染时脱敏。
#[derive(Clone, Debug, Serialize)]
pub struct ManagedRequirementConflict {
    pub field: String,
    pub required: String,
    pub published: String,
}

/// 请求出口与凭据相关字段。
///
/// 只比较 `base_url`、`env_key`、`experimental_bearer_token`、`wire_api`、
/// `requires_openai_auth`。`auth`/`http_headers`/`query_params`/`model_catalog_url`/
/// `supports_websockets` 等字段不在当前"请求出口"定义内，未做同等比较；它们不都是
/// 展示字段，是否纳入需要单独的产品定义与证据。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProviderRoute {
    base_url: Option<String>,
    env_key: Option<String>,
    experimental_bearer_token: Option<String>,
    wire_api: String,
    requires_openai_auth: bool,
}

impl ProviderRoute {
    fn parse(item: &Item, name: &str) -> Result<Self> {
        let table = item.as_table_like().ok_or_else(|| {
            Error::Config(format!(
                "Codex requirements model_providers.{name} must be a table"
            ))
        })?;
        let field = |key: &str| format!("model_providers.{name}.{key}");
        Ok(Self {
            base_url: string_field(table.get("base_url"), &field("base_url"))?,
            env_key: string_field(table.get("env_key"), &field("env_key"))?,
            experimental_bearer_token: string_field(
                table.get("experimental_bearer_token"),
                &field("experimental_bearer_token"),
            )?,
            wire_api: string_field(table.get("wire_api"), &field("wire_api"))?
                .unwrap_or_else(|| "responses".into()),
            requires_openai_auth: bool_field(
                table.get("requires_openai_auth"),
                &field("requires_openai_auth"),
            )?
            .unwrap_or(false),
        })
    }
}

fn string_field(item: Option<&Item>, field: &str) -> Result<Option<String>> {
    match item {
        None => Ok(None),
        Some(item) => item
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| Error::Config(format!("Codex requirements {field} must be a string"))),
    }
}

fn bool_field(item: Option<&Item>, field: &str) -> Result<Option<bool>> {
    match item {
        None => Ok(None),
        Some(item) => item
            .as_bool()
            .map(Some)
            .ok_or_else(|| Error::Config(format!("Codex requirements {field} must be a boolean"))),
    }
}

fn render_provider(route: &ProviderRoute) -> String {
    const REDACTED: &str = "[redacted]";
    format!(
        "base_url={}, env_key={}, wire_api={}, requires_openai_auth={}, token={}",
        route.base_url.as_deref().unwrap_or("<unset>"),
        route.env_key.as_deref().unwrap_or("<unset>"),
        route.wire_api,
        route.requires_openai_auth,
        if route.experimental_bearer_token.is_some() {
            REDACTED
        } else {
            "<unset>"
        }
    )
}

#[cfg(windows)]
fn system_requirements_path() -> PathBuf {
    if let Some(path) = registry_requirements_path() {
        return path;
    }
    program_data_dir()
        .join("OpenAI")
        .join("Codex")
        .join("requirements.toml")
}

#[cfg(not(windows))]
fn system_requirements_path() -> PathBuf {
    PathBuf::from("/etc/codex/requirements.toml")
}

#[cfg(windows)]
fn registry_requirements_path() -> Option<PathBuf> {
    windows_registry::LOCAL_MACHINE
        .open(r"SOFTWARE\Policies\OpenAI\Codex")
        .ok()
        .and_then(|key| key.get_string("RequirementsPath").ok())
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
fn program_data_dir() -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_ProgramData, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
    };

    let mut path = std::ptr::null_mut::<u16>();
    let result = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_ProgramData,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &mut path,
        )
    };
    if result < 0 || path.is_null() {
        return PathBuf::from(r"C:\ProgramData");
    }
    // SAFETY: SHGetKnownFolderPath 返回以 NUL 结尾的 UTF-16 字符串，所有权归调用方。
    let path = unsafe {
        let mut len = 0usize;
        while *path.add(len) != 0 {
            len += 1;
        }
        let text = std::ffi::OsString::from_wide(std::slice::from_raw_parts(path, len));
        CoTaskMemFree(path.cast());
        PathBuf::from(text)
    };
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published() -> PublishedRoute {
        PublishedRoute {
            provider: "haxsd-byok".into(),
            catalog: PathBuf::from(r"C:\codex-home\haxsd-byok-models-abc.json"),
            base_url: "http://127.0.0.1:43120/v1".into(),
            token: "local-test-token".into(),
        }
    }

    fn conflicts(requirements: &str) -> Vec<ManagedRequirementConflict> {
        ManagedRequirements::parse(requirements)
            .unwrap()
            .route_conflicts(&published())
    }

    /// 只限制审批/沙箱的 requirements 不改变请求出口。
    #[test]
    fn approval_and_sandbox_requirements_do_not_change_the_route() {
        let conflicts = conflicts(
            "allowed_approval_policies = [\"never\"]\n\
             allowed_sandbox_modes = [\"read-only\"]\n\
             [windows]\nallow_mxc = false\n",
        );
        assert!(conflicts.is_empty(), "{conflicts:?}");
    }

    /// 同值的 provider 要求不是冲突。
    #[test]
    fn an_identical_managed_provider_is_not_a_conflict() {
        let conflicts = conflicts(
            "model_provider = \"haxsd-byok\"\n\
             model_catalog_json = 'C:\\codex-home\\haxsd-byok-models-abc.json'\n\
             [model_providers.haxsd-byok]\n\
             name = \"haxsd byok\"\n\
             base_url = \"http://127.0.0.1:43120/v1\"\n\
             wire_api = \"responses\"\n\
             requires_openai_auth = false\n\
             experimental_bearer_token = \"local-test-token\"\n",
        );
        assert!(conflicts.is_empty(), "{conflicts:?}");
    }

    /// 强制另一个 provider 会改变实际请求出口。
    #[test]
    fn a_different_managed_provider_is_a_conflict() {
        let conflicts = conflicts("model_provider = \"openai\"\n");
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].field, "model_provider");
        assert_eq!(conflicts[0].required, "openai");
        assert_eq!(conflicts[0].published, "haxsd-byok");
    }

    /// 同名 provider 换地址：官方会按条目替换，请求不再到本机网关。
    #[test]
    fn a_same_named_provider_with_another_endpoint_is_a_conflict() {
        let conflicts = conflicts(
            "[model_providers.haxsd-byok]\n\
             base_url = \"http://127.0.0.1:43121/v1\"\n\
             wire_api = \"responses\"\n\
             requires_openai_auth = false\n\
             experimental_bearer_token = \"local-test-token\"\n",
        );
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].field, "model_providers.haxsd-byok");
        assert!(conflicts[0].published.contains("43120"), "{conflicts:?}");
        assert!(conflicts[0].required.contains("43121"), "{conflicts:?}");
        assert!(
            !conflicts[0].required.contains("local-test-token"),
            "requirements 里的凭据不能回显: {conflicts:?}"
        );
    }

    /// 目录指针被要求换成别处：客户端读到的模型目录不再是我们发布的那份。
    #[test]
    fn a_different_managed_catalog_is_a_conflict() {
        let conflicts = conflicts("model_catalog_json = 'C:\\elsewhere\\models.json'\n");
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].field, "model_catalog_json");
    }

    /// 文件不存在与显式空值都不构成约束。
    #[test]
    fn a_missing_requirements_file_is_not_a_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let source = RequirementsSource::file(directory.path().join("absent.toml"));
        let managed = source.load().unwrap();
        assert!(managed.route_conflicts(&published()).is_empty());
    }

    /// 来源由调用方注入：inspect 不会自己去找 ProgramData/HKLM。
    #[test]
    fn requirements_are_loaded_only_from_the_injected_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("requirements.toml");
        fs::write(&path, "model_provider = \"openai\"\n").unwrap();
        let managed = RequirementsSource::file(&path).load().unwrap();
        assert_eq!(
            managed.route_conflicts(&published())[0].field,
            "model_provider"
        );
    }

    /// 系统来源每次 load 重新解析路径，不在启动时固化 `File(...)`。
    #[test]
    fn system_source_is_resolved_on_each_load() {
        assert!(matches!(
            RequirementsSource::system(),
            RequirementsSource::System
        ));
    }

    /// 消费字段类型非法时必须失败关闭，不能当成默认值/空表。
    #[test]
    fn a_non_string_model_provider_is_rejected() {
        assert!(ManagedRequirements::parse("model_provider = 5\n").is_err());
    }

    #[test]
    fn a_non_table_model_providers_is_rejected() {
        assert!(ManagedRequirements::parse("model_providers = \"oops\"\n").is_err());
    }

    #[test]
    fn a_non_table_provider_entry_is_rejected() {
        assert!(
            ManagedRequirements::parse("model_providers = { haxsd-byok = \"oops\" }\n").is_err()
        );
    }

    #[test]
    fn a_non_boolean_requires_openai_auth_is_rejected() {
        assert!(ManagedRequirements::parse(
            "[model_providers.haxsd-byok]\nrequires_openai_auth = \"false\"\n"
        )
        .is_err());
    }

    #[test]
    fn a_non_string_provider_endpoint_or_token_is_rejected() {
        for field in [
            "base_url",
            "env_key",
            "experimental_bearer_token",
            "wire_api",
        ] {
            assert!(
                ManagedRequirements::parse(&format!("[model_providers.haxsd-byok]\n{field} = 5\n"))
                    .is_err(),
                "{field}"
            );
        }
    }
}
