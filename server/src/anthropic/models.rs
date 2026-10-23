//! Claude Code 的模型别名解析与 Anthropic 格式的模型目录。
use std::collections::HashSet;

use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::{json, Value};

use super::{ApiError, ClaudeSettings, ModelFamily};
use crate::{model::ModelConfig, store::Store};

/// `GET /v1/models` 的 ID 前缀。Claude Code 与 Desktop 的模型发现只保留 `id` 里含
/// `claude`（或 `anthropic`）的条目，裸哈希会被整批过滤掉，所以发现 ID 必须带前缀。
const DISCOVERY_PREFIX: &str = "claude-";

/// 模型在发现列表里的 ID。
pub fn discovery_id(model_hash: &str) -> String {
    let mut id = String::from(DISCOVERY_PREFIX);
    id.push_str(&model_hash.to_ascii_lowercase());
    id
}

/// 把 Claude Code 发来的模型 ID 解析成模型库里的配置。
///
/// 顺序：精确的 `model_hash` → 发现 ID（`claude-<model_hash>`）→ 家族名
/// （`…-sonnet-…` / `…-opus-…` / `…-haiku-…`，含裸别名）→ 默认模型。未知 ID 走默认
/// 模型而不是报错：Claude Code 会发送它自带的版本化 ID，网关不该要求用户把它们逐一
/// 登记。
pub async fn resolve(
    store: &Store,
    settings: &ClaudeSettings,
    requested: &str,
) -> Result<ModelConfig, ApiError> {
    let requested = requested.trim();
    // Claude Code 会在模型 ID 后面追加 `[1m]` 表示 1M 上下文变体；模型库里的哈希没有
    // 这个后缀，先剥掉再解析，否则别名会全部退化成默认模型。
    let requested = requested.strip_suffix("[1m]").unwrap_or(requested).trim();
    let default = settings
        .configured_default()
        .ok_or_else(|| ApiError::invalid("no default Claude model is configured"))?;
    let default = load(store, default).await?.ok_or_else(|| {
        ApiError::invalid("the default Claude model no longer exists; re-select it in settings")
    })?;

    if !requested.is_empty() {
        if let Some(model) = load(store, requested).await? {
            return Ok(model);
        }
        // 发现 ID 剥掉前缀后按哈希查；前缀后面不是已知哈希时继续走家族名回退，
        // 这样 `claude-sonnet-4-5-…` 这类客户端自带 ID 仍然可用。
        if let Some(hash) = discovery_hash(requested) {
            if let Some(model) = load(store, &hash).await? {
                return Ok(model);
            }
        }
        if let Some(family) = family_of(requested) {
            if let Some(id) = settings.family(family) {
                if let Some(model) = load(store, id).await? {
                    return Ok(model);
                }
            }
        }
    }
    Ok(default)
}

/// `GET /v1/models` 返回的模型集合：默认模型在前，随后是三个别名，按哈希去重。
pub async fn catalog(
    store: &Store,
    settings: &ClaudeSettings,
) -> Result<Vec<ModelConfig>, ApiError> {
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for (_, id) in settings.model_references() {
        if !seen.insert(id.to_owned()) {
            continue;
        }
        if let Some(model) = load(store, id).await? {
            models.push(model);
        }
    }
    Ok(models)
}

/// Anthropic 的列表形状。`id` 是带 `claude-` 前缀的发现 ID（见 [`discovery_id`]），
/// 解析器把它剥回哈希后精确命中同一个模型；可读名称在 `display_name` 里。
pub fn catalog_json(models: &[ModelConfig]) -> Value {
    let data = models
        .iter()
        .map(|model| {
            json!({
                "type": "model",
                "id": discovery_id(&model.model_hash),
                "display_name": model.display_name,
                "created_at": created_at(model.created_at_ms),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "data": data,
        "has_more": false,
        "first_id": models.first().map(|model| discovery_id(&model.model_hash)),
        "last_id": models.last().map(|model| discovery_id(&model.model_hash)),
    })
}

/// 从发现 ID 里剥出哈希；不是 `claude-` 开头（或后面为空）时返回 `None`。
///
/// 前缀与内容都按大小写不敏感处理：模型哈希本身是小写十六进制，客户端把 ID 大写
/// 回来时仍应命中同一个模型。
fn discovery_hash(id: &str) -> Option<String> {
    id.get(..DISCOVERY_PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(DISCOVERY_PREFIX))
        .and_then(|_| id.get(DISCOVERY_PREFIX.len()..))
        .filter(|hash| !hash.is_empty())
        .map(str::to_ascii_lowercase)
}

fn created_at(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_millis_opt(0).unwrap())
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn family_of(requested: &str) -> Option<ModelFamily> {
    let id = requested.to_ascii_lowercase();
    if id.contains("opus") {
        Some(ModelFamily::Opus)
    } else if id.contains("sonnet") {
        Some(ModelFamily::Sonnet)
    } else if id.contains("haiku") {
        Some(ModelFamily::Haiku)
    } else {
        None
    }
}

async fn load(store: &Store, id: &str) -> Result<Option<ModelConfig>, ApiError> {
    store
        .model(id)
        .await
        .map_err(|error| internal(error.to_string()))
}

pub(crate) fn internal(message: String) -> ApiError {
    ApiError {
        status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        kind: "api_error",
        message,
        should_retry: true,
        retry_after: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        anthropic::ClaudeSettings,
        model::{ModelConfigInput, ModelType},
    };

    async fn fixture() -> (tempfile::TempDir, Store, ClaudeSettings) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("claude-models.db").display()
        ))
        .await
        .unwrap();
        let mut hashes = Vec::new();
        for name in ["Default", "Sonnet", "Opus", "Haiku"] {
            let model = create(&store, name).await;
            hashes.push(model);
        }
        let settings = ClaudeSettings {
            enabled: true,
            default_model_hash: Some(hashes[0].clone()),
            sonnet_model_hash: Some(hashes[1].clone()),
            opus_model_hash: Some(hashes[2].clone()),
            haiku_model_hash: Some(hashes[3].clone()),
            ..ClaudeSettings::default()
        };
        store.set_claude_settings(settings.clone()).await.unwrap();
        (directory, store, settings)
    }

    async fn create(store: &Store, name: &str) -> String {
        store
            .create_model(&ModelConfigInput {
                sort_order: 0,
                display_name: name.into(),
                group_name: None,
                model_type: ModelType::Anthropic,
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

    #[tokio::test]
    async fn claude_model_ids_route_by_family_and_unknown_ids_fall_back() {
        let (_directory, store, settings) = fixture().await;

        for (requested, expected) in [
            ("claude-sonnet-4-5-20250929", "Sonnet"),
            ("claude-3-5-haiku-20241022", "Haiku"),
            ("claude-opus-4-1-20250805", "Opus"),
            ("some-vendor-model", "Default"),
            ("", "Default"),
        ] {
            let model = resolve(&store, &settings, requested).await.unwrap();
            assert_eq!(model.display_name, expected, "{requested}");
        }

        let exact = settings.opus_model_hash.clone().unwrap();
        assert_eq!(
            resolve(&store, &settings, &exact).await.unwrap().model_hash,
            exact
        );
    }

    #[tokio::test]
    async fn an_unmapped_family_falls_back_to_the_default_model() {
        let (_directory, store, mut settings) = fixture().await;
        settings.sonnet_model_hash = None;
        store.set_claude_settings(settings.clone()).await.unwrap();

        let model = resolve(&store, &settings, "claude-sonnet-4-5")
            .await
            .unwrap();
        assert_eq!(Some(model.model_hash), settings.default_model_hash.clone());
    }

    #[tokio::test]
    async fn the_catalog_is_anthropic_shaped_and_deduplicated() {
        let (_directory, store, mut settings) = fixture().await;
        settings.haiku_model_hash = settings.sonnet_model_hash.clone();
        let models = catalog(&store, &settings).await.unwrap();
        assert_eq!(models.len(), 3, "同一个哈希只出现一次");

        let default_hash = settings.default_model_hash.clone().unwrap();
        let value = catalog_json(&models);
        assert_eq!(value["data"][0]["type"], "model");
        assert_eq!(value["data"][0]["id"], discovery_id(&default_hash));
        assert_eq!(value["data"][0]["display_name"], "Default");
        assert_eq!(value["has_more"], false);
        assert_eq!(value["first_id"], discovery_id(&default_hash));
        assert_eq!(
            value["last_id"],
            discovery_id(&models.last().unwrap().model_hash)
        );
        assert!(value["data"][0]["created_at"].is_string());

        // Claude Code / Desktop 的发现过滤器只保留含 `claude` 的 ID。
        for entry in value["data"].as_array().unwrap() {
            let id = entry["id"].as_str().unwrap();
            assert!(id.contains("claude"), "{id}");
            assert_eq!(id, id.to_ascii_lowercase());
        }
    }

    #[tokio::test]
    async fn discovery_ids_and_bare_hashes_both_resolve_to_the_same_model() {
        let (_directory, store, settings) = fixture().await;
        let hash = settings.opus_model_hash.clone().unwrap();

        let by_discovery_id = resolve(&store, &settings, &discovery_id(&hash))
            .await
            .unwrap();
        assert_eq!(by_discovery_id.model_hash, hash);
        assert_eq!(by_discovery_id.display_name, "Opus");

        // 裸哈希继续可用：Cursor / Deck 的既有配置里存的是它。
        let by_hash = resolve(&store, &settings, &hash).await.unwrap();
        assert_eq!(by_hash.model_hash, hash);

        // 大小写不敏感的前缀也要认得。
        let upper = discovery_id(&hash).to_ascii_uppercase();
        assert_eq!(
            resolve(&store, &settings, &upper).await.unwrap().model_hash,
            hash
        );
    }

    #[tokio::test]
    async fn an_unknown_prefixed_id_falls_back_by_family_then_default() {
        let (_directory, store, settings) = fixture().await;

        // 前缀后面不是已登记的哈希：仍按家族名解析。
        let haiku = resolve(&store, &settings, "claude-zzy-haiku")
            .await
            .unwrap();
        assert_eq!(haiku.display_name, "Haiku");

        let default = resolve(&store, &settings, "claude-unknown").await.unwrap();
        assert_eq!(default.display_name, "Default");

        // 只有前缀、没有内容也不算命中。
        let default = resolve(&store, &settings, "claude-").await.unwrap();
        assert_eq!(default.display_name, "Default");

        // Claude Code 的 1M 变体后缀：裸哈希与发现 ID 加 `[1m]` 都要命中同一个模型。
        let default_hash = settings.default_model_hash.clone().unwrap();
        let one_million = resolve(&store, &settings, &format!("{default_hash}[1m]"))
            .await
            .unwrap();
        assert_eq!(one_million.display_name, "Default");
        let one_million = resolve(&store, &settings, &format!("claude-{default_hash}[1m]"))
            .await
            .unwrap();
        assert_eq!(one_million.display_name, "Default");
    }
}
