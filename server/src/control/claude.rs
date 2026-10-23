//! Claude Code 的桌面管理接口；网关不写任何用户文件，只有显式应用/撤销才改客户端配置。
use std::collections::BTreeMap;

use axum::{
    extract::State,
    middleware,
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    anthropic::{ClaudeGateway, ClaudeSettings, ClaudeStatus},
    claude_code::{
        self,
        desktop::{self as claude_desktop, DesktopModel, DesktopPaths, DesktopRoute},
        ownership::{CliEnvironment, DesktopPolicy},
        settings::{self as cli_settings, CliModel, CliPaths, CliRoute, GatewayValues},
        CliTakeoverStatus, DesktopTakeoverStatus, Family, TakeoverState, TakeoverStatus,
    },
    model::ModelConfig,
    store::Store,
    Error, Result,
};

use super::{desktop_cors, reject_cross_site};

#[derive(Clone)]
struct ClaudeControl {
    gateway: ClaudeGateway,
    store: Store,
    takeover: TakeoverContext,
}

/// 接管路径的来源。生产环境每次请求按当前进程环境与用户目录解析；测试固定到临时目录，
/// 不读真实注册表、也不碰真实用户目录。
#[derive(Clone)]
enum TakeoverContext {
    System,
    #[cfg(test)]
    Fixed(Box<FixedTakeover>),
}

#[cfg(test)]
#[derive(Clone)]
struct FixedTakeover {
    cli: CliPaths,
    desktop: DesktopPaths,
    state_dir: std::path::PathBuf,
}

impl TakeoverContext {
    fn cli_paths(&self) -> Result<CliPaths> {
        match self {
            Self::System => CliPaths::discover(),
            #[cfg(test)]
            Self::Fixed(fixed) => Ok(fixed.cli.clone()),
        }
    }

    fn desktop_paths(&self) -> Result<DesktopPaths> {
        match self {
            Self::System => DesktopPaths::discover(),
            #[cfg(test)]
            Self::Fixed(fixed) => Ok(fixed.desktop.clone()),
        }
    }

    fn state_dir(&self) -> Result<std::path::PathBuf> {
        match self {
            Self::System => claude_code::state_dir(),
            #[cfg(test)]
            Self::Fixed(fixed) => Ok(fixed.state_dir.clone()),
        }
    }

    fn cli_environment(&self) -> CliEnvironment {
        match self {
            Self::System => CliEnvironment::detect(),
            #[cfg(test)]
            Self::Fixed(_) => CliEnvironment::default(),
        }
    }

    fn desktop_policy(&self) -> DesktopPolicy {
        match self {
            Self::System => DesktopPolicy::detect(),
            #[cfg(test)]
            Self::Fixed(_) => DesktopPolicy::default(),
        }
    }

    async fn desktop_running(&self) -> bool {
        match self {
            Self::System => claude_desktop::process_running().await,
            #[cfg(test)]
            Self::Fixed(_) => false,
        }
    }
}

pub fn router(gateway: ClaudeGateway, store: Store) -> Router {
    router_with(gateway, store, TakeoverContext::System)
}

fn router_with(gateway: ClaudeGateway, store: Store, takeover: TakeoverContext) -> Router {
    Router::new()
        .route("/__byok-api__/api/claude/settings", get(settings).put(save))
        .route("/__byok-api__/api/claude/status", get(status))
        .route("/__byok-api__/api/claude/token/rotate", post(rotate_token))
        .route("/__byok-api__/api/claude/takeover", get(takeover_status))
        .route("/__byok-api__/api/claude/takeover/cli", put(takeover_cli))
        .route(
            "/__byok-api__/api/claude/takeover/desktop",
            put(takeover_desktop),
        )
        .with_state(ClaudeControl {
            gateway,
            store,
            takeover,
        })
        .layer(desktop_cors())
        .layer(middleware::from_fn(reject_cross_site))
}

async fn settings(State(state): State<ClaudeControl>) -> Result<Json<ClaudeSettings>> {
    Ok(Json(state.store.claude_settings().await?))
}

async fn save(
    State(state): State<ClaudeControl>,
    Json(settings): Json<ClaudeSettings>,
) -> Result<Json<ClaudeSettings>> {
    // 端口准入在写库之前：被自己或别的程序占着时，宁可拒绝保存，也不要留下
    // "已保存、未监听"的状态。
    super::ports::ensure_claude_port_usable(
        &state.store,
        &settings,
        state.gateway.listening_port().await,
    )
    .await?;
    Ok(Json(state.gateway.save_settings(settings).await?))
}

async fn status(State(state): State<ClaudeControl>) -> Result<Json<ClaudeStatus>> {
    Ok(Json(state.gateway.status().await?))
}

/// 立即轮换访问令牌。
///
/// 已接管 Claude Desktop 时先拒绝：它的 profile 里也有令牌，Desktop 关闭时服务端
/// 无法把新令牌写进去，轮换只会让用户下次打开 Desktop 时拿着旧令牌报错。
/// 已接管 Claude Code 时同步重写 `settings.json`；重写失败时令牌已经换掉，状态会
/// 显示 stale，错误信息要求重新应用。
async fn rotate_token(State(state): State<ClaudeControl>) -> Result<Json<serde_json::Value>> {
    let facts = Facts::load(&state).await?;
    let gateway = facts.gateway_values();
    let state_dir = state.takeover.state_dir()?;

    let policy = state.takeover.desktop_policy();
    let desktop = claude_desktop::inspect(
        &state.takeover.desktop_paths()?,
        &state_dir,
        &policy,
        state.takeover.desktop_running().await,
        Some(&gateway),
    )?;
    match desktop.state {
        TakeoverState::Applied | TakeoverState::Stale => {
            return Err(Error::Config(
                "undo the Claude Desktop takeover before rotating the gateway token".into(),
            ));
        }
        TakeoverState::Running => {
            return Err(Error::Config(
                "quit Claude Desktop completely, then undo the takeover or rotate the gateway token"
                    .into(),
            ));
        }
        _ => {}
    }

    let cli_paths = state.takeover.cli_paths()?;
    let environment = state.takeover.cli_environment();
    let cli = cli_settings::inspect(&cli_paths, &state_dir, &environment, Some(&gateway))?;
    match cli.state {
        TakeoverState::Conflict | TakeoverState::Managed => {
            return Err(Error::Config(
                "resolve the Claude Code takeover conflict before rotating the gateway token; apply or undo the takeover first"
                    .into(),
            ));
        }
        _ => {}
    }
    let sync_cli = matches!(cli.state, TakeoverState::Applied | TakeoverState::Stale);

    let token = state.gateway.rotate_token().await?;
    let mut cli_synced = false;
    if sync_cli {
        let route = CliRoute {
            base_url: facts.status.base_url.clone(),
            token: token.clone(),
            models: facts.cli_models(),
        };
        cli_settings::apply(&cli_paths, &state_dir, &environment, &route).map_err(|error| {
            Error::Config(format!(
                "the gateway token was rotated, but the Claude Code settings could not be updated: {error}; apply the Claude Code takeover again"
            ))
        })?;
        cli_synced = true;
    }
    Ok(Json(
        serde_json::json!({ "token": token, "cli_synced": cli_synced }),
    ))
}

/// 正常退出时撤销 Claude 接管，避免客户端在网关停止后继续指向本机端口。
///
/// 进程被强杀或崩溃时没有机会运行这里；下次启动网关会重新监听同一端口，客户端
/// 继续可用。清理失败只记日志：进程已经在退出，没有第二次机会。
pub async fn shutdown_cleanup(gateway: ClaudeGateway, store: Store) {
    let state_dir = match claude_code::state_dir() {
        Ok(directory) => directory,
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the Claude takeover state directory; skipped cleanup");
            return;
        }
    };
    let facts = match Facts::load_parts(&gateway, &store).await {
        Ok(facts) => Some(facts),
        Err(error) => {
            tracing::warn!(%error, "cannot read the Claude gateway state; only undo takeovers that are still restorable");
            None
        }
    };
    let gateway_values = facts.as_ref().map(Facts::gateway_values);

    let environment = CliEnvironment::detect();
    match CliPaths::discover() {
        Ok(paths) => {
            match cli_settings::inspect(&paths, &state_dir, &environment, gateway_values.as_ref()) {
                Ok(status) if take_over(&status.state, status.restorable) => {
                    match cli_settings::undo(&paths, &state_dir, &environment) {
                        Ok(_) => {
                            tracing::info!("restored the Claude Code settings during shutdown")
                        }
                        Err(error) => {
                            tracing::warn!(%error, "cannot restore the Claude Code settings during shutdown");
                        }
                    }
                }
                Ok(status) => {
                    if leaves_something(&status.state) {
                        tracing::warn!(
                            state = ?status.state,
                            conflicts = status.conflicts.len(),
                            "left the Claude Code takeover in place during shutdown"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot inspect the Claude Code takeover during shutdown");
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the Claude Code settings path; skipped cleanup");
        }
    }

    let policy = DesktopPolicy::detect();
    match DesktopPaths::discover() {
        Ok(paths) => {
            let running = claude_desktop::process_running().await;
            match claude_desktop::inspect(
                &paths,
                &state_dir,
                &policy,
                running,
                gateway_values.as_ref(),
            ) {
                Ok(status) if running => {
                    if status.restorable {
                        tracing::warn!(
                            "Claude Desktop is running; left its takeover in place during shutdown"
                        );
                    }
                }
                Ok(status) if take_over(&status.state, status.restorable) => {
                    match claude_desktop::undo(&paths, &state_dir, &policy, false) {
                        Ok(_) => {
                            tracing::info!("restored the Claude Desktop profile during shutdown");
                        }
                        Err(error) => {
                            tracing::warn!(%error, "cannot restore the Claude Desktop profile during shutdown");
                        }
                    }
                }
                Ok(status) => {
                    if leaves_something(&status.state) {
                        tracing::warn!(
                            state = ?status.state,
                            conflicts = status.conflicts.len(),
                            "left the Claude Desktop takeover in place during shutdown"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot inspect the Claude Desktop takeover during shutdown");
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the Claude Desktop profile path; skipped cleanup");
        }
    }
}

fn take_over(state: &TakeoverState, restorable: bool) -> bool {
    restorable && matches!(state, TakeoverState::Applied | TakeoverState::Stale)
}

/// 有收据却没能撤销的状态：退出时值得留一条日志。
fn leaves_something(state: &TakeoverState) -> bool {
    matches!(
        state,
        TakeoverState::Applied
            | TakeoverState::Stale
            | TakeoverState::Conflict
            | TakeoverState::Managed
            | TakeoverState::Running
    )
}

async fn takeover_status(State(state): State<ClaudeControl>) -> Result<Json<TakeoverStatus>> {
    Ok(Json(build_takeover_status(&state).await?))
}

async fn takeover_cli(
    State(state): State<ClaudeControl>,
    Json(request): Json<CliTakeoverRequest>,
) -> Result<Json<CliTakeoverStatus>> {
    let facts = Facts::load(&state).await?;
    let paths = state.takeover.cli_paths()?;
    let state_dir = state.takeover.state_dir()?;
    let environment = state.takeover.cli_environment();
    let status = match request.action {
        TakeoverAction::Apply => {
            facts.require_gateway()?;
            let models = cli_models(&request, &facts)?;
            let route = CliRoute {
                base_url: facts.status.base_url.clone(),
                token: facts.status.token.clone(),
                models,
            };
            cli_settings::apply(&paths, &state_dir, &environment, &route)?
        }
        TakeoverAction::Undo => cli_settings::undo(&paths, &state_dir, &environment)?,
    };
    Ok(Json(status.with_missing_models(facts.missing_models())))
}

async fn takeover_desktop(
    State(state): State<ClaudeControl>,
    Json(request): Json<DesktopTakeoverRequest>,
) -> Result<Json<DesktopTakeoverStatus>> {
    let facts = Facts::load(&state).await?;
    let paths = state.takeover.desktop_paths()?;
    let state_dir = state.takeover.state_dir()?;
    let policy = state.takeover.desktop_policy();
    let status = match request.action {
        TakeoverAction::Apply => {
            facts.require_gateway()?;
            let models = desktop_models(&request, &facts)?;
            let route = DesktopRoute {
                base_url: facts.status.base_url.clone(),
                token: facts.status.token.clone(),
                models,
            };
            claude_desktop::apply(
                &paths,
                &state_dir,
                &policy,
                state.takeover.desktop_running().await,
                &route,
            )?
        }
        TakeoverAction::Undo => claude_desktop::undo(
            &paths,
            &state_dir,
            &policy,
            state.takeover.desktop_running().await,
        )?,
    };
    Ok(Json(status))
}

async fn build_takeover_status(state: &ClaudeControl) -> Result<TakeoverStatus> {
    let facts = Facts::load(state).await?;
    let cli_paths = state.takeover.cli_paths()?;
    let desktop_paths = state.takeover.desktop_paths()?;
    let state_dir = state.takeover.state_dir()?;
    let environment = state.takeover.cli_environment();
    let gateway = facts.gateway_values();
    let cli = cli_settings::inspect(&cli_paths, &state_dir, &environment, Some(&gateway))?
        .with_missing_models(facts.missing_models());
    let desktop = claude_desktop::inspect(
        &desktop_paths,
        &state_dir,
        &state.takeover.desktop_policy(),
        state.takeover.desktop_running().await,
        Some(&gateway),
    )?;
    Ok(TakeoverStatus { cli, desktop })
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TakeoverAction {
    Apply,
    Undo,
}

#[derive(Debug, Deserialize)]
struct CliTakeoverRequest {
    action: TakeoverAction,
    #[serde(default)]
    sonnet_model_hash: Option<String>,
    #[serde(default)]
    opus_model_hash: Option<String>,
    #[serde(default)]
    haiku_model_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DesktopTakeoverRequest {
    action: TakeoverAction,
    #[serde(default)]
    models: Vec<DesktopModelRequest>,
}

#[derive(Debug, Deserialize)]
struct DesktopModelRequest {
    model_hash: String,
    #[serde(default)]
    label: Option<String>,
}

struct Facts {
    settings: ClaudeSettings,
    status: ClaudeStatus,
    /// 网关当前实际持有的监听端口；`None` 表示没有监听器。
    listening_port: Option<u16>,
    models: BTreeMap<String, ModelConfig>,
    /// 当前应写入客户端的完整 `env` 映射。
    desired: BTreeMap<String, Value>,
    /// 网关已启用、正在监听、端口一致、默认模型存在，并且没有缺失模型。
    available: bool,
}

impl Facts {
    async fn load(state: &ClaudeControl) -> Result<Self> {
        Self::load_parts(&state.gateway, &state.store).await
    }

    async fn load_parts(gateway: &ClaudeGateway, store: &Store) -> Result<Self> {
        let settings = store.claude_settings().await?;
        let status = gateway.status().await?;
        let listening_port = gateway.listening_port().await;
        let models = store
            .models()
            .await?
            .into_iter()
            .map(|model| (model.model_hash.clone(), model))
            .collect();
        let mut facts = Self {
            settings,
            status,
            listening_port,
            models,
            desired: BTreeMap::new(),
            available: false,
        };
        facts.available = facts.gateway_available();
        facts.desired = facts.desired_values();
        Ok(facts)
    }

    fn gateway_values(&self) -> GatewayValues<'_> {
        GatewayValues {
            base_url: &self.status.base_url,
            token: &self.status.token,
            desired: &self.desired,
            available: self.available,
        }
    }

    /// 当前设置对应的 CLI 模型列表；模型库里已经删掉的哈希直接跳过，状态接口靠
    /// `available = false` 与键集合差异把它报成 stale。
    fn cli_models(&self) -> Vec<CliModel> {
        let mut models = Vec::new();
        for family in Family::ALL {
            let Some(hash) = configured_hash(&self.settings, family) else {
                continue;
            };
            let Some(model) = self.models.get(hash) else {
                continue;
            };
            models.push(CliModel {
                family,
                model_hash: model.model_hash.clone(),
                display_name: model.display_name.clone(),
            });
        }
        models
    }

    /// 当前应写入 CLI 的完整 env。构造失败时至少回退到入口地址、令牌与归属头：
    /// 状态接口不能因为一次参数校验失败变成 500。
    fn desired_values(&self) -> BTreeMap<String, Value> {
        let route = CliRoute {
            base_url: self.status.base_url.clone(),
            token: self.status.token.clone(),
            models: self.cli_models(),
        };
        match cli_settings::desired_values(&route) {
            Ok(desired) => desired,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "cannot build the Claude gateway env values; falling back to the base URL, token and attribution header"
                );
                cli_settings::base_values(&self.status.base_url, &self.status.token)
            }
        }
    }

    fn gateway_available(&self) -> bool {
        self.settings.enabled
            && self.status.listening
            && self.listening_port == Some(self.settings.port)
            && self.settings.configured_default().is_some()
            && self.missing_models().is_empty()
    }

    /// 接管写进客户端的是可用的网关地址与模型别名；三者缺一，客户端就会拿着坏地址
    /// 或坏模型发请求，所以应用前必须逐条确认，而不是写完再报错。
    fn require_gateway(&self) -> Result<()> {
        if !self.settings.enabled {
            return Err(Error::Config(
                "enable the Claude gateway before applying the takeover".into(),
            ));
        }
        let Some(default) = self.settings.configured_default() else {
            return Err(Error::Config(
                "select a default Claude model before applying the takeover".into(),
            ));
        };
        if !self.models.contains_key(default) {
            return Err(Error::Config(
                "the default Claude model no longer exists; re-select it in Claude settings".into(),
            ));
        }
        if !self.status.listening {
            return Err(Error::Config(
                "the Claude gateway is not listening; save the settings and check the port before applying the takeover"
                    .into(),
            ));
        }
        let listening_port = self.listening_port.ok_or_else(|| {
            Error::Config(
                "the Claude gateway is not listening; save the settings and check the port before applying the takeover"
                    .into(),
            )
        })?;
        // 设置与服务端监听端口必须一致：否则写进客户端的是一个没人监听的地址。
        if listening_port != self.settings.port || self.status.port != self.settings.port {
            return Err(Error::Config(format!(
                "the Claude gateway is listening on port {listening_port}, but the saved settings use port {}; save the settings again before applying the takeover",
                self.settings.port
            )));
        }
        Ok(())
    }

    fn missing_models(&self) -> Vec<String> {
        let mut missing = Vec::new();
        for (_, id) in self.settings.model_references() {
            if !self.models.contains_key(id) && !missing.iter().any(|seen| seen == id) {
                missing.push(id.to_owned());
            }
        }
        missing
    }
}

trait WithMissingModels {
    fn with_missing_models(self, missing: Vec<String>) -> Self;
}

impl WithMissingModels for CliTakeoverStatus {
    fn with_missing_models(mut self, missing: Vec<String>) -> Self {
        self.missing_models = missing;
        self
    }
}

fn cli_models(request: &CliTakeoverRequest, facts: &Facts) -> Result<Vec<CliModel>> {
    let mut models = Vec::new();
    for (family, provided, configured) in [
        (
            Family::Sonnet,
            request.sonnet_model_hash.as_deref(),
            facts.settings.sonnet_model_hash.as_deref(),
        ),
        (
            Family::Opus,
            request.opus_model_hash.as_deref(),
            facts.settings.opus_model_hash.as_deref(),
        ),
        (
            Family::Haiku,
            request.haiku_model_hash.as_deref(),
            facts.settings.haiku_model_hash.as_deref(),
        ),
    ] {
        let hash = match provided {
            Some(hash) if hash.trim().is_empty() => {
                return Err(Error::Config(format!(
                    "Claude {} model must not be blank",
                    family.tier()
                )));
            }
            Some(hash) => Some(hash.trim()),
            None => configured.filter(|hash| !hash.trim().is_empty()),
        };
        let Some(hash) = hash else {
            continue;
        };
        let model = facts.models.get(hash).ok_or_else(|| {
            Error::Config(format!("unknown Claude {} model: {hash}", family.tier()))
        })?;
        models.push(CliModel {
            family,
            model_hash: model.model_hash.clone(),
            display_name: model.display_name.clone(),
        });
    }
    Ok(models)
}

fn desktop_models(request: &DesktopTakeoverRequest, facts: &Facts) -> Result<Vec<DesktopModel>> {
    if request.models.is_empty() {
        return Err(Error::Config(
            "select at least one model for Claude Desktop".into(),
        ));
    }
    let mut models = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in &request.models {
        let hash = item.model_hash.trim();
        if hash.is_empty() {
            return Err(Error::Config(
                "Claude Desktop model hash must not be blank".into(),
            ));
        }
        if !seen.insert(hash.to_owned()) {
            return Err(Error::Config(format!(
                "duplicate Claude Desktop model: {hash}"
            )));
        }
        let model = facts
            .models
            .get(hash)
            .ok_or_else(|| Error::Config(format!("unknown Claude Desktop model: {hash}")))?;
        let label = item
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| model.display_name.clone());
        models.push(DesktopModel {
            model_hash: model.model_hash.clone(),
            label: Some(label),
            tier: family_for(model, &facts.settings),
        });
    }
    Ok(models)
}

fn family_for(model: &ModelConfig, settings: &ClaudeSettings) -> Option<Family> {
    Family::ALL
        .into_iter()
        .find(|family| configured_hash(settings, *family) == Some(model.model_hash.as_str()))
        .or_else(|| Family::from_hint(&model.display_name))
        .or_else(|| Family::from_hint(&model.model_id))
}

fn configured_hash(settings: &ClaudeSettings, family: Family) -> Option<&str> {
    match family {
        Family::Sonnet => settings.sonnet_model_hash.as_deref(),
        Family::Opus => settings.opus_model_hash.as_deref(),
        Family::Haiku => settings.haiku_model_hash.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{header, Method, Request, StatusCode},
    };
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::{
        model::ModelConfigInput,
        network::NetworkClients,
        provider::{Provider, ProviderStream},
    };
    use tokio_util::sync::CancellationToken;

    struct EmptyProvider;

    impl Provider for EmptyProvider {
        fn stream(
            &self,
            _invocation: crate::model::ModelInvocation,
            _cancellation: CancellationToken,
        ) -> ProviderStream {
            Box::pin(futures_util::stream::empty())
        }
    }

    async fn fixture() -> (tempfile::TempDir, Store, Router) {
        let directory = tempfile::tempdir().unwrap();
        let store = create_store(&directory).await;
        let model = create_model(&store, "Claude").await;
        store
            .set_claude_settings(ClaudeSettings {
                default_model_hash: Some(model),
                ..ClaudeSettings::default()
            })
            .await
            .unwrap();
        let provider = Arc::new(EmptyProvider) as Arc<dyn Provider>;
        let gateway =
            ClaudeGateway::new(store.clone(), provider, NetworkClients::new(store.clone()));
        (directory, store.clone(), router(gateway, store))
    }

    struct TakeoverFixture {
        _directory: tempfile::TempDir,
        app: Router,
        store: Store,
        cli: CliPaths,
        desktop: DesktopPaths,
        state_dir: std::path::PathBuf,
        port: u16,
        hashes: Vec<String>,
    }

    async fn takeover_fixture(enabled: bool) -> TakeoverFixture {
        takeover_fixture_with_gateway(enabled, true).await
    }

    /// 已启用、默认模型也在，但网关没有启动：apply 必须在这一步就拒绝。
    async fn takeover_fixture_not_listening() -> TakeoverFixture {
        takeover_fixture_with_gateway(true, false).await
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn takeover_fixture_with_gateway(enabled: bool, started: bool) -> TakeoverFixture {
        let directory = tempfile::tempdir().unwrap();
        let store = create_store(&directory).await;
        let mut hashes = Vec::new();
        for name in ["Default", "Sonnet", "Opus"] {
            hashes.push(create_model(&store, name).await);
        }
        let port = free_port();
        store
            .set_claude_settings(ClaudeSettings {
                enabled,
                port,
                default_model_hash: Some(hashes[0].clone()),
                sonnet_model_hash: Some(hashes[1].clone()),
                opus_model_hash: Some(hashes[2].clone()),
                ..ClaudeSettings::default()
            })
            .await
            .unwrap();
        let provider = Arc::new(EmptyProvider) as Arc<dyn Provider>;
        let gateway =
            ClaudeGateway::new(store.clone(), provider, NetworkClients::new(store.clone()));
        if started {
            gateway.start().await.unwrap();
        }
        let cli_directory = directory.path().join("claude-config");
        std::fs::create_dir_all(&cli_directory).unwrap();
        let cli = CliPaths::at_config_dir(cli_directory).unwrap();
        let local = directory.path().join("local");
        std::fs::create_dir_all(local.join("Claude")).unwrap();
        let desktop = DesktopPaths::at_local_app_data(local);
        let state_dir = directory.path().join("state");
        let app = router_with(
            gateway,
            store.clone(),
            TakeoverContext::Fixed(Box::new(FixedTakeover {
                cli: cli.clone(),
                desktop: desktop.clone(),
                state_dir: state_dir.clone(),
            })),
        );
        TakeoverFixture {
            _directory: directory,
            app,
            store,
            cli,
            desktop,
            state_dir,
            port,
            hashes,
        }
    }

    async fn create_store(directory: &tempfile::TempDir) -> Store {
        Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("claude-takeover.db").display()
        ))
        .await
        .unwrap()
    }

    async fn create_model(store: &Store, name: &str) -> String {
        store
            .create_model(&ModelConfigInput {
                sort_order: 0,
                display_name: name.into(),
                group_name: None,
                model_type: crate::model::ModelType::Anthropic,
                base_url: "https://api.anthropic.com".into(),
                use_full_url: false,
                api_key: "key".into(),
                tooltip_data: name.into(),
                model_id: format!("vendor-{}", name.to_ascii_lowercase()),
                reasoning_effort: None,
                openai_endpoint: String::new(),
                openai_extra_params_enabled: false,
                openai_extra_params: json!({}),
                custom_headers_enabled: false,
                custom_headers: json!({}),
                anthropic_extra_params_enabled: false,
                anthropic_extra_params: json!({}),
                context_window_tokens: Some(200_000),
                max_completion_tokens: None,
                anthropic_max_tokens: Some(8_192),
                anthropic_thinking_effort: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap()
            .model_hash
    }

    async fn send(
        app: &Router,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let response = app
            .clone()
            .oneshot(
                request
                    .body(Body::from(
                        body.map(|body| body.to_string()).unwrap_or_default(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn claude_writes_refuse_cross_site_callers() {
        let (_directory, _store, app) = fixture().await;
        for path in [
            "/__byok-api__/api/claude/settings",
            "/__byok-api__/api/claude/takeover/cli",
            "/__byok-api__/api/claude/takeover/desktop",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::PUT)
                        .uri(path)
                        .header(header::ORIGIN, "https://evil.example")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }

    #[tokio::test]
    async fn claude_reads_stay_open_to_local_callers() {
        let (_directory, _store, app) = fixture().await;
        for origin in [
            None,
            Some("http://127.0.0.1:19387"),
            Some("tauri://localhost"),
        ] {
            let mut request = Request::builder()
                .method(Method::GET)
                .uri("/__byok-api__/api/claude/settings");
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{origin:?}");
        }
    }

    #[tokio::test]
    async fn takeover_apply_and_undo_run_against_the_temporary_paths() {
        let fixture = takeover_fixture(true).await;
        let (status, body) = send(
            &fixture.app,
            Method::GET,
            "/__byok-api__/api/claude/takeover",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["cli"]["state"], "ready");
        assert_eq!(body["cli"]["restorable"], false);
        assert_eq!(body["cli"]["conflicts"], json!([]));
        assert_eq!(body["cli"]["managed_sources"], json!([]));
        assert_eq!(body["desktop"]["state"], "ready");
        assert_eq!(body["desktop"]["restorable"], false);
        assert_eq!(body["desktop"]["config_source"], "local");
        assert_eq!(
            body["cli"]["settings_path"],
            fixture.cli.settings.display().to_string()
        );

        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/cli",
            Some(json!({"action": "apply", "sonnet_model_hash": fixture.hashes[1]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "applied");
        assert_eq!(body["restorable"], true);
        assert_eq!(body["missing_models"], json!([]));
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture.cli.settings).unwrap()).unwrap();
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            format!("http://127.0.0.1:{}", fixture.port)
        );
        assert_eq!(settings["env"]["CLAUDE_CODE_ATTRIBUTION_HEADER"], "0");
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            fixture.hashes[1]
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"],
            "Sonnet"
        );
        assert!(fixture.state_dir.join("cli.json").is_file());

        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/desktop",
            Some(json!({
                "action": "apply",
                "models": [
                    {"model_hash": fixture.hashes[1], "label": "Sonnet"},
                    {"model_hash": fixture.hashes[2]},
                ],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "applied");
        assert_eq!(body["restorable"], true);
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture.desktop.meta).unwrap()).unwrap();
        let uuid = meta["appliedId"].as_str().unwrap();
        assert!(fixture.desktop.profile(uuid).is_file());
        assert!(fixture.state_dir.join("desktop.json").is_file());

        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/desktop",
            Some(json!({"action": "undo"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "ready");
        assert_eq!(body["restorable"], false);
        assert!(!fixture.desktop.profile(uuid).is_file());
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture.desktop.meta).unwrap()).unwrap();
        assert!(meta.get("appliedId").is_none());

        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/cli",
            Some(json!({"action": "undo"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "ready");
        assert_eq!(body["restorable"], false);
        assert!(!fixture.state_dir.join("cli.json").exists());
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture.cli.settings).unwrap()).unwrap();
        assert!(settings.get("env").is_none());
    }

    #[tokio::test]
    async fn takeover_requires_an_enabled_gateway_and_known_models() {
        let fixture = takeover_fixture(false).await;
        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/cli",
            Some(json!({"action": "apply"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("enable the Claude gateway"),
            "{body}"
        );

        let fixture = takeover_fixture(true).await;
        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/cli",
            Some(json!({"action": "apply", "sonnet_model_hash": "missing"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("unknown Claude sonnet model"),
            "{body}"
        );

        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/desktop",
            Some(json!({"action": "apply", "models": [{"model_hash": "missing"}]})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("unknown Claude Desktop model"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn takeover_apply_requires_a_listening_gateway_on_the_saved_port() {
        let fixture = takeover_fixture_not_listening().await;
        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/cli",
            Some(json!({"action": "apply"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("not listening"),
            "{body}"
        );

        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/desktop",
            Some(json!({
                "action": "apply",
                "models": [{"model_hash": fixture.hashes[0]}],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("not listening"),
            "{body}"
        );
        // 拒绝发生在写文件之前。
        assert!(!fixture.cli.settings.exists());
        assert!(!fixture.desktop.meta.exists());
    }

    fn cli_settings_document(fixture: &TakeoverFixture) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&fixture.cli.settings).unwrap()).unwrap()
    }

    async fn rotate(fixture: &TakeoverFixture) -> (StatusCode, Value) {
        send(
            &fixture.app,
            Method::POST,
            "/__byok-api__/api/claude/token/rotate",
            None,
        )
        .await
    }

    #[tokio::test]
    async fn rotating_the_token_changes_the_one_status_reports() {
        let fixture = takeover_fixture(true).await;
        let (_, before) = send(
            &fixture.app,
            Method::GET,
            "/__byok-api__/api/claude/status",
            None,
        )
        .await;
        let old = before["token"].as_str().unwrap().to_owned();

        let (status, body) = rotate(&fixture).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let new = body["token"].as_str().unwrap().to_owned();
        assert_ne!(new, old);
        assert_eq!(body["cli_synced"], false, "未接管的 CLI 不该被改写");

        let (_, after) = send(
            &fixture.app,
            Method::GET,
            "/__byok-api__/api/claude/status",
            None,
        )
        .await;
        assert_eq!(after["token"], new);
    }

    #[tokio::test]
    async fn rotating_is_refused_while_claude_desktop_has_the_takeover() {
        let fixture = takeover_fixture(true).await;
        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/desktop",
            Some(json!({
                "action": "apply",
                "models": [{"model_hash": fixture.hashes[0]}],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "applied");
        let (_, before) = send(
            &fixture.app,
            Method::GET,
            "/__byok-api__/api/claude/status",
            None,
        )
        .await;
        let old = before["token"].as_str().unwrap().to_owned();

        let (status, body) = rotate(&fixture).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("undo the Claude Desktop takeover before rotating"),
            "{body}"
        );

        // 拒绝发生在轮换之前：库里的令牌与 profile 都还是旧的。
        let (_, after) = send(
            &fixture.app,
            Method::GET,
            "/__byok-api__/api/claude/status",
            None,
        )
        .await;
        assert_eq!(after["token"], old);
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture.desktop.meta).unwrap()).unwrap();
        let uuid = meta["appliedId"].as_str().unwrap();
        let profile: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture.desktop.profile(uuid)).unwrap())
                .unwrap();
        assert_eq!(profile["inferenceGatewayApiKey"], old.as_str());
    }

    #[tokio::test]
    async fn rotating_rewrites_an_applied_cli_takeover() {
        let fixture = takeover_fixture(true).await;
        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/cli",
            Some(json!({"action": "apply"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "applied");

        let (status, body) = rotate(&fixture).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let token = body["token"].as_str().unwrap().to_owned();
        assert_eq!(body["cli_synced"], true);

        let settings = cli_settings_document(&fixture);
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], token.as_str());
        let (_, takeover) = send(
            &fixture.app,
            Method::GET,
            "/__byok-api__/api/claude/takeover",
            None,
        )
        .await;
        assert_eq!(takeover["cli"]["state"], "applied");
    }

    #[tokio::test]
    async fn a_changed_gateway_setting_marks_the_cli_takeover_stale() {
        let fixture = takeover_fixture(true).await;
        let (status, body) = send(
            &fixture.app,
            Method::PUT,
            "/__byok-api__/api/claude/takeover/cli",
            Some(json!({"action": "apply"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "applied");

        // 接管之后换掉 Sonnet 模型：settings.json 仍与收据一致，但期望值已经变了。
        let mut settings = fixture.store.claude_settings().await.unwrap();
        settings.sonnet_model_hash = Some(fixture.hashes[2].clone());
        fixture.store.set_claude_settings(settings).await.unwrap();

        let (status, body) = send(
            &fixture.app,
            Method::GET,
            "/__byok-api__/api/claude/takeover",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["cli"]["state"], "stale");
        assert_eq!(body["cli"]["restorable"], true);
        let stale = body["cli"]["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|conflict| conflict["code"] == "takeover_stale")
            .expect("stale 状态必须带 takeover_stale");
        assert_eq!(stale["blocking"], false);
        assert_eq!(
            stale["message"],
            "the gateway settings changed after the takeover; apply again to update the client"
        );
    }
}
