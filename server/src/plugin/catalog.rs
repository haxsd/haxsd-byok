//! Discovers plugin manifests and evaluates serializable TypeScript definitions.
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::STANDARD, Engine};

use super::{
    definition::PluginDefinitionLoader,
    descriptor::PluginModuleDefinition,
    manifest::{validate_id, PluginManifest},
};
use crate::{config, Error, Result};

const MANIFEST_FILE_NAME: &str = "plugin.json";
const MAX_ICON_BYTES: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct PluginCatalog {
    roots: Vec<PathBuf>,
    definition_loader: PluginDefinitionLoader,
    app_version: String,
}

#[derive(Clone)]
pub(crate) struct PluginEntry {
    pub directory: PathBuf,
    pub entry: PathBuf,
    pub manifest: PluginManifest,
    pub definition: PluginModuleDefinition,
    pub icon: String,
}

impl PluginCatalog {
    pub fn managed(app_version: String) -> Result<Self> {
        let installed = config::managed_data_dir()?.join("plugins/installed");
        fs::create_dir_all(&installed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&installed, fs::Permissions::from_mode(0o700))?;
        }
        // 内置插件按版本预装进 installed;版本一致时不写盘。
        super::builtin::install(&installed)?;
        // 扫描顺序即优先级:debug 下源码目录优先,保证内置插件热改生效;
        // 发布构建只有 installed 一个根。
        #[cfg(debug_assertions)]
        let roots = vec![
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins/build-in"),
            installed,
        ];
        #[cfg(not(debug_assertions))]
        let roots = vec![installed];
        Ok(Self {
            roots,
            definition_loader: PluginDefinitionLoader::managed()?,
            app_version,
        })
    }

    pub(crate) fn loader(&self) -> &PluginDefinitionLoader {
        &self.definition_loader
    }

    /// 插件目录的当前指纹：每个根目录、每个插件子目录、以及子目录里每个文件的
    /// (修改时间, 大小)。
    ///
    /// registry 用它给"求值过的插件表"做失效判定：求值一个插件要跑一次 Deno，不能每次
    /// 都重来；但改了插件、修好清单、预装或卸载之后必须立刻生效，不能要求重启。
    /// 带上文件大小是因为同一秒内改写文件时 mtime 可能不变，而长度一定变。
    pub(crate) fn fingerprint(&self) -> String {
        let mut stamp = String::new();
        for root in &self.roots {
            stamp.push_str(&root.display().to_string());
            append_stamp(&mut stamp, root);
            let Ok(mut directories) = child_directories(root) else {
                continue;
            };
            directories.sort();
            for directory in directories {
                stamp.push_str(&directory.display().to_string());
                append_stamp(&mut stamp, &directory);
                let Ok(files) = fs::read_dir(&directory) else {
                    continue;
                };
                let mut names = files
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                names.sort();
                for name in names {
                    stamp.push_str(&name);
                    append_stamp(&mut stamp, &directory.join(&name));
                }
            }
        }
        stamp
    }

    pub(crate) async fn entries(&self, executable: &Path) -> Vec<PluginEntry> {
        let mut plugins = BTreeMap::new();
        for root in &self.roots {
            let mut directories = match child_directories(root) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(path = %root.display(), %error, "failed to scan plugin directory");
                    continue;
                }
            };
            directories.sort();
            for directory in directories {
                match load_plugin(
                    &directory,
                    &self.definition_loader,
                    executable,
                    &self.app_version,
                )
                .await
                {
                    Ok(entry) => {
                        if plugins.contains_key(&entry.manifest.id) {
                            tracing::warn!(plugin = %entry.manifest.id, path = %directory.display(), "ignoring duplicate plugin");
                        } else {
                            plugins.insert(entry.manifest.id.clone(), entry);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(path = %directory.display(), %error, "ignoring invalid plugin")
                    }
                }
            }
        }
        plugins.into_values().collect()
    }

    pub(crate) fn manifests(&self) -> Vec<(PluginManifest, String)> {
        let mut plugins = BTreeMap::new();
        for root in &self.roots {
            let Ok(mut directories) = child_directories(root) else {
                continue;
            };
            directories.sort();
            for directory in directories {
                let loaded = (|| -> Result<_> {
                    let manifest: PluginManifest =
                        serde_json::from_slice(&fs::read(directory.join(MANIFEST_FILE_NAME))?)?;
                    manifest.validate(&directory)?;
                    require_app_version(&manifest, &self.app_version)?;
                    let icon = icon_data_url(&directory, &manifest.icon)?;
                    Ok((manifest, icon))
                })();
                if let Ok((manifest, icon)) = loaded {
                    plugins
                        .entry(manifest.id.clone())
                        .or_insert((manifest, icon));
                }
            }
        }
        plugins.into_values().collect()
    }
}

/// 把一个路径的 (修改时间, 大小) 追加进指纹。读不到（刚被删掉）也算一种状态。
fn append_stamp(stamp: &mut String, path: &Path) {
    match fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |elapsed| elapsed.as_nanos());
            stamp.push_str(&format!(":{modified}:{};", metadata.len()));
        }
        Err(_) => stamp.push_str(":missing;"),
    }
}

fn child_directories(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut directories = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
            directories.push(entry.path());
        }
    }
    Ok(directories)
}

/// 应用过旧时拒绝加载,让插件的 minAppVersion 声明生效。
fn require_app_version(manifest: &PluginManifest, app_version: &str) -> Result<()> {
    let Some(minimum) = &manifest.min_app_version else {
        return Ok(());
    };
    if super::manifest::version_at_least(app_version, minimum) {
        return Ok(());
    }
    Err(Error::Config(format!(
        "plugin '{}' requires app version {minimum} or newer (current {app_version})",
        manifest.id
    )))
}

async fn load_plugin(
    directory: &Path,
    loader: &PluginDefinitionLoader,
    executable: &Path,
    app_version: &str,
) -> Result<PluginEntry> {
    let manifest: PluginManifest =
        serde_json::from_slice(&fs::read(directory.join(MANIFEST_FILE_NAME))?)?;
    manifest.validate(directory)?;
    require_app_version(&manifest, app_version)?;
    let icon = icon_data_url(directory, &manifest.icon)?;
    let entry = directory.join(&manifest.entry).canonicalize()?;
    let definition = loader.load(executable, directory, &entry).await?;
    validate_definition(&manifest.id, &definition)?;
    Ok(PluginEntry {
        directory: directory.to_path_buf(),
        entry,
        manifest,
        definition,
        icon,
    })
}

/// 显示文本必须是非空字符串,或全为非空字符串的 locale 映射。
fn validate_localized_text(value: &serde_json::Value, label: &str) -> Result<()> {
    match value {
        serde_json::Value::String(text) if !text.trim().is_empty() => Ok(()),
        serde_json::Value::Object(map)
            if !map.is_empty()
                && map
                    .values()
                    .all(|entry| entry.as_str().is_some_and(|text| !text.trim().is_empty())) =>
        {
            Ok(())
        }
        _ => Err(Error::Config(format!(
            "{label} must be a non-empty string or a locale map of non-empty strings"
        ))),
    }
}

fn validate_definition(plugin_id: &str, definition: &PluginModuleDefinition) -> Result<()> {
    if definition.providers.is_empty() {
        return Err(Error::Config(format!(
            "plugin '{plugin_id}' must define at least one provider"
        )));
    }
    let mut provider_ids = std::collections::HashSet::new();
    for provider in &definition.providers {
        validate_id(&provider.id, "plugin provider id")?;
        validate_localized_text(
            &provider.display_name,
            &format!(
                "plugin '{plugin_id}' provider '{}' displayName",
                provider.id
            ),
        )?;
        if provider.provider_type.trim().is_empty() {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' provider '{}' requires providerType",
                provider.id
            )));
        }
        if !provider_ids.insert(provider.id.clone()) {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' contains duplicate provider '{}'",
                provider.id
            )));
        }
        if let Some(resource_type) = &provider.resource_type {
            if !definition
                .resources
                .iter()
                .any(|resource| &resource.resource_type == resource_type)
            {
                return Err(Error::Config(format!(
                    "plugin '{plugin_id}' provider '{}' consumes undeclared resource '{resource_type}'",
                    provider.id
                )));
            }
        }
    }
    let mut resource_types = std::collections::HashSet::new();
    for resource in &definition.resources {
        validate_id(&resource.resource_type, "plugin resource type")?;
        validate_localized_text(
            &resource.display_name,
            &format!(
                "plugin '{plugin_id}' resource '{}' displayName",
                resource.resource_type
            ),
        )?;
        if !resource_types.insert(resource.resource_type.clone()) {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' contains duplicate resource type '{}'",
                resource.resource_type
            )));
        }
        for method in &resource.add {
            validate_id(&method.id, "plugin add method id")?;
            if !matches!(
                method.method_type.as_str(),
                super::descriptor::OAUTH2_ADD_METHOD
                    | super::descriptor::OAUTH2_AUTHORIZATION_CODE_ADD_METHOD
            ) {
                return Err(Error::Config(format!(
                    "plugin '{plugin_id}' add method '{}' uses unsupported type '{}'",
                    method.id, method.method_type
                )));
            }
            if method.method_type == super::descriptor::OAUTH2_ADD_METHOD
                && method.callback.is_some()
            {
                return Err(Error::Config(format!(
                    "plugin '{plugin_id}' device OAuth method '{}' cannot declare callback settings",
                    method.id
                )));
            }
            if let Some(callback) = &method.callback {
                if callback.port == Some(0) {
                    return Err(Error::Config(format!(
                        "plugin '{plugin_id}' OAuth callback port must be greater than zero"
                    )));
                }
                if let Some(host) = callback.host.as_deref() {
                    if !super::descriptor::validate_callback_host(host) {
                        return Err(Error::Config(format!(
                            "plugin '{plugin_id}' OAuth callback host '{host}' must be '127.0.0.1' or 'localhost'"
                        )));
                    }
                }
                if let Some(path) = callback.path.as_deref() {
                    if !path.starts_with('/')
                        || path.len() > 128
                        || path.contains('?')
                        || path.contains('#')
                        || path.contains("//")
                    {
                        return Err(Error::Config(format!(
                            "plugin '{plugin_id}' contains invalid OAuth callback path '{path}'"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn icon_data_url(directory: &Path, relative: &str) -> Result<String> {
    let root = directory.canonicalize()?;
    let path = directory.join(relative).canonicalize()?;
    if !path.starts_with(&root) {
        return Err(Error::Config(format!(
            "plugin icon escapes its directory: {relative}"
        )));
    }
    if fs::metadata(&path)?.len() > MAX_ICON_BYTES {
        return Err(Error::Config(format!(
            "plugin icon exceeds {MAX_ICON_BYTES} bytes: {relative}"
        )));
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = match extension.as_str() {
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        _ => {
            return Err(Error::Config(format!(
                "unsupported plugin icon: {relative}"
            )))
        }
    };
    Ok(format!(
        "data:{mime};base64,{}",
        STANDARD.encode(fs::read(path)?)
    ))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::plugin) fn catalog(root: &Path) -> PluginCatalog {
        PluginCatalog {
            roots: vec![root.join("installed")],
            definition_loader: PluginDefinitionLoader::for_test(root).unwrap(),
            app_version: "1.4.2".into(),
        }
    }

    fn authorization_code_definition(host: Option<&str>) -> PluginModuleDefinition {
        use super::super::descriptor::{
            AddMethodDefinition, OAuthCallbackDefinition, ProviderDefinition, ResourceDefinition,
        };
        PluginModuleDefinition {
            providers: vec![ProviderDefinition {
                id: "claude".into(),
                display_name: serde_json::json!("Claude"),
                description: serde_json::Value::Null,
                provider_type: "anthropic".into(),
                resource_type: Some("account".into()),
                has_models: true,
            }],
            resources: vec![ResourceDefinition {
                resource_type: "account".into(),
                display_name: serde_json::json!("Account"),
                add: vec![AddMethodDefinition {
                    method_type: super::super::descriptor::OAUTH2_AUTHORIZATION_CODE_ADD_METHOD
                        .into(),
                    id: "login".into(),
                    display_name: serde_json::json!("Log in"),
                    description: serde_json::Value::Null,
                    callback: Some(OAuthCallbackDefinition {
                        port: Some(54545),
                        path: Some("/callback".into()),
                        host: host.map(str::to_owned),
                    }),
                }],
                import: None,
                actions: vec![],
                can_refresh: true,
                can_remove: false,
            }],
        }
    }

    #[test]
    fn oauth_callback_host_must_be_a_loopback_alias() {
        for host in [None, Some("127.0.0.1"), Some("localhost")] {
            validate_definition("plugin", &authorization_code_definition(host)).unwrap();
        }
        for host in ["example.com", "0.0.0.0", "::1", "127.0.0.1.evil.test"] {
            let definition = authorization_code_definition(Some(host));
            assert!(
                validate_definition("plugin", &definition).is_err(),
                "host {host} must be rejected"
            );
        }
    }

    #[test]
    fn repository_examples_have_valid_static_manifests() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins/build-in");
        let sdk = tempfile::tempdir().unwrap();
        let catalog = PluginCatalog {
            roots: vec![root],
            definition_loader: PluginDefinitionLoader::for_test(sdk.path()).unwrap(),
            app_version: env!("CARGO_PKG_VERSION").into(),
        };
        assert!(!catalog.manifests().is_empty());
    }

    /// 内置插件的导入图必须完整：每个 `cursor-byok:*` 说明符都要在该插件自己的
    /// `deno.json` import map 里，且映射到的文件真的存在；相对导入同理；此外不许出现
    /// `npm:` / `jsr:` / `http(s):` 这类需要联网或额外权限的说明符。
    ///
    /// 这条用例是给 CI 里的 `deno test` 当前哨：本机没有 Deno（也拉不动 GitHub release），
    /// 导入路径写错只会在 CI 上炸，而这里的失败信息直接指出是哪个文件哪一行。
    #[test]
    fn built_in_plugin_imports_resolve_within_the_repository() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins/build-in");
        let mut plugins = fs::read_dir(&root)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        plugins.sort();
        assert!(!plugins.is_empty(), "{root:?} 里应当有内置插件");

        for plugin in &plugins {
            let name = plugin.file_name().unwrap().to_string_lossy().into_owned();
            let manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(plugin.join("plugin.json")).unwrap()).unwrap();
            let entry = manifest["entry"]
                .as_str()
                .expect("plugin.json needs an entry");
            assert!(
                plugin.join(entry).is_file(),
                "{name}: 清单里的入口 {entry} 不存在"
            );

            let deno: serde_json::Value =
                serde_json::from_slice(&fs::read(plugin.join("deno.json")).unwrap()).unwrap();
            let imports = deno["imports"]
                .as_object()
                .unwrap_or_else(|| panic!("{name}: deno.json 必须有 imports"));

            for file in typescript_files(plugin) {
                let source = fs::read_to_string(&file).unwrap();
                for specifier in imported_specifiers(&source) {
                    let where_ = format!("{name}/{}", file.file_name().unwrap().to_string_lossy());
                    if specifier.starts_with("cursor-byok:") {
                        let mapped = imports.get(&specifier).and_then(|value| value.as_str());
                        let Some(mapped) = mapped else {
                            panic!("{where_}: 说明符 {specifier} 不在 deno.json 的 imports 里");
                        };
                        assert!(
                            plugin.join(mapped).is_file(),
                            "{where_}: {specifier} 映射到不存在的 {mapped}"
                        );
                    } else if specifier.starts_with("./") || specifier.starts_with("../") {
                        assert!(
                            plugin.join(&specifier).is_file(),
                            "{where_}: 相对导入 {specifier} 不存在"
                        );
                    } else {
                        panic!(
                            "{where_}: {specifier} 需要联网或额外权限，内置插件的测试必须完全离线"
                        );
                    }
                }
            }
        }
    }

    fn typescript_files(directory: &Path) -> Vec<PathBuf> {
        let mut files = fs::read_dir(directory)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("ts"))
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    /// 抽出 `from "x"` / `import "x"` 里的说明符。
    fn imported_specifiers(source: &str) -> Vec<String> {
        source
            .lines()
            .filter(|line| {
                let line = line.trim_start();
                !line.starts_with("//")
                    && (line.contains("from \"") || line.starts_with("import \""))
            })
            .filter_map(|line| {
                let after = line
                    .split("from \"")
                    .nth(1)
                    .or_else(|| line.trim_start().strip_prefix("import \""))?;
                after.split('"').next().map(str::to_owned)
            })
            .collect()
    }
}
