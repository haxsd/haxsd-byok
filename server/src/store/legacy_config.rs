//! Imports model definitions from the v0.0.49 YAML configuration.
use std::{collections::HashSet, path::Path};

use serde::Deserialize;

use crate::{
    model::{
        model_fingerprint, normalize_model_input, normalize_request_url, ModelConfigInput,
        ModelType, OPENAI_CHAT_ENDPOINT, OPENAI_RESPONSES_ENDPOINT,
    },
    Error, Result,
};

use super::Store;

pub struct LegacyModelImportPlan {
    pub models: Vec<LegacyModelImportEntry>,
}

pub struct LegacyModelImportEntry {
    /// 配置内容指纹：判断该条配置在库里是否已经存在。不是模型身份
    /// （身份见 `model_configs.model_hash`），沿用预览接口既有的字段名。
    pub model_hash: String,
    pub input: ModelConfigInput,
    pub existing: bool,
}

pub struct LegacyModelImportOutcome {
    pub imported: usize,
    pub skipped: usize,
    pub total: usize,
}

#[derive(Default, Deserialize)]
struct LegacyConfig {
    #[serde(rename = "modelAdapters", default)]
    model_adapters: Vec<LegacyModel>,
}

#[derive(Default, Deserialize)]
struct LegacyModel {
    #[serde(default)]
    sort: i64,
    #[serde(rename = "displayName", default)]
    display_name: String,
    #[serde(rename = "type", default)]
    model_type: String,
    #[serde(rename = "baseURL", default)]
    base_url: String,
    #[serde(rename = "apiKey", default)]
    api_key: String,
    #[serde(rename = "tooltipData", default)]
    tooltip_data: String,
    #[serde(rename = "modelID", default)]
    model_id: String,
    #[serde(rename = "reasoningEffort", default)]
    reasoning_effort: String,
    #[serde(rename = "openAIEndpoint", default)]
    openai_endpoint: String,
    #[serde(rename = "openAIExtraParamsEnabled", default)]
    openai_extra_params_enabled: bool,
    #[serde(rename = "openAIExtraParamsJSON", default)]
    openai_extra_params_json: String,
    #[serde(rename = "customHeadersEnabled", default)]
    custom_headers_enabled: bool,
    #[serde(rename = "customHeadersJSON", default)]
    custom_headers_json: String,
    #[serde(rename = "anthropicExtraParamsEnabled", default)]
    anthropic_extra_params_enabled: bool,
    #[serde(rename = "anthropicExtraParamsJSON", default)]
    anthropic_extra_params_json: String,
    #[serde(rename = "contextWindowTokens", default)]
    context_window_tokens: u64,
    #[serde(rename = "maxCompletionTokens", default)]
    max_completion_tokens: u64,
    #[serde(rename = "anthropicMaxTokens", default)]
    anthropic_max_tokens: u64,
    #[serde(rename = "anthropicThinkingEffort", default)]
    anthropic_thinking_effort: String,
    #[serde(rename = "thinkingBudgetTokens", default)]
    thinking_budget_tokens: u64,
}

impl Store {
    pub async fn preview_v0049_model_config(&self, path: &Path) -> Result<LegacyModelImportPlan> {
        let inputs = load_v0049_model_config(path)?;
        // "已存在"按配置内容判断：改名或改过凭据的那条不再等于文件里的原配置。
        let existing = self.stored_fingerprints(None).await?;
        let mut seen = HashSet::with_capacity(inputs.len());
        let mut models = Vec::with_capacity(inputs.len());
        for input in inputs {
            let input = normalize_model_input(&input)?;
            let fingerprint = model_fingerprint(&input)?;
            if seen.insert(fingerprint.clone()) {
                models.push(LegacyModelImportEntry {
                    existing: existing.contains(&fingerprint),
                    model_hash: fingerprint,
                    input,
                });
            }
        }
        Ok(LegacyModelImportPlan { models })
    }

    pub async fn import_v0049_model_config(&self, path: &Path) -> Result<LegacyModelImportOutcome> {
        let plan = self.preview_v0049_model_config(path).await?;
        let total = plan.models.len();
        let missing = plan
            .models
            .into_iter()
            .filter(|model| !model.existing)
            .map(|model| model.input)
            .collect::<Vec<_>>();
        let imported = self.create_models_if_missing(&missing).await?;
        Ok(LegacyModelImportOutcome {
            imported,
            skipped: total - imported,
            total,
        })
    }
}

fn load_v0049_model_config(path: &Path) -> Result<Vec<ModelConfigInput>> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Config(format!(
                "v0.0.49 config not found at {}",
                path.display()
            )))
        }
        Err(error) => return Err(error.into()),
    };
    let legacy: LegacyConfig = serde_yaml::from_slice(&raw)
        .map_err(|error| Error::Config(format!("invalid v0.0.49 config: {error}")))?;
    if legacy.model_adapters.is_empty() {
        return Err(Error::Config(
            "v0.0.49 config contains no model adapters".into(),
        ));
    }
    legacy.model_adapters.into_iter().map(model_input).collect()
}

fn model_input(model: LegacyModel) -> Result<ModelConfigInput> {
    let model_type = match model.model_type.trim().to_ascii_lowercase().as_str() {
        "openai" => ModelType::OpenAi,
        "anthropic" => ModelType::Anthropic,
        value => {
            return Err(Error::Config(format!(
                "unsupported v0.0.49 model type: {value}"
            )))
        }
    };
    let (base_url, openai_endpoint, use_full_url) =
        legacy_request_configuration(model_type, &model.base_url, &model.openai_endpoint)?;
    Ok(ModelConfigInput {
        sort_order: model.sort,
        display_name: model.display_name.clone(),
        group_name: None,
        model_type,
        base_url,
        use_full_url,
        api_key: model.api_key,
        tooltip_data: if model.tooltip_data.trim().is_empty() {
            model.display_name
        } else {
            model.tooltip_data
        },
        model_id: model.model_id,
        reasoning_effort: optional_string(model.reasoning_effort),
        openai_endpoint,
        openai_extra_params_enabled: model.openai_extra_params_enabled,
        openai_extra_params: enabled_json_object(
            model_type == ModelType::OpenAi && model.openai_extra_params_enabled,
            &model.openai_extra_params_json,
        )?,
        custom_headers_enabled: model.custom_headers_enabled,
        custom_headers: enabled_json_object(
            model.custom_headers_enabled,
            &model.custom_headers_json,
        )?,
        anthropic_extra_params_enabled: model.anthropic_extra_params_enabled,
        anthropic_extra_params: enabled_json_object(
            model_type == ModelType::Anthropic && model.anthropic_extra_params_enabled,
            &model.anthropic_extra_params_json,
        )?,
        context_window_tokens: positive(model.context_window_tokens),
        max_completion_tokens: positive(model.max_completion_tokens),
        anthropic_max_tokens: positive(model.anthropic_max_tokens),
        anthropic_thinking_effort: optional_string(model.anthropic_thinking_effort),
        thinking_budget_tokens: positive(model.thinking_budget_tokens),
    })
}

fn legacy_request_configuration(
    model_type: ModelType,
    base_url: &str,
    openai_endpoint: &str,
) -> Result<(String, String, bool)> {
    let base_url = normalize_request_url(base_url)?;
    match model_type {
        ModelType::Anthropic => {
            let use_full_url = url_path_ends_with(&base_url, "/messages");
            Ok((base_url, String::new(), use_full_url))
        }
        ModelType::OpenAi => {
            let detected = openai_protocol_from_url(&base_url);
            let configured = match openai_endpoint.trim() {
                "" | OPENAI_RESPONSES_ENDPOINT => OPENAI_RESPONSES_ENDPOINT,
                OPENAI_CHAT_ENDPOINT => OPENAI_CHAT_ENDPOINT,
                "/custom" => OPENAI_CHAT_ENDPOINT,
                value => {
                    return Err(Error::Config(format!(
                        "unsupported v0.0.49 OpenAI endpoint: {value}"
                    )))
                }
            };
            let protocol = detected.unwrap_or(configured);
            let use_full_url = detected.is_some() || openai_endpoint.trim() == "/custom";
            Ok((base_url, protocol.into(), use_full_url))
        }
    }
}

fn openai_protocol_from_url(value: &str) -> Option<&'static str> {
    let url = reqwest::Url::parse(value).ok()?;
    let path = url.path().trim_end_matches('/');
    if path.to_ascii_lowercase().ends_with("/responses") {
        Some(OPENAI_RESPONSES_ENDPOINT)
    } else if path.to_ascii_lowercase().ends_with("/chat/completions") {
        Some(OPENAI_CHAT_ENDPOINT)
    } else {
        None
    }
}

fn url_path_ends_with(value: &str, suffix: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        url.path()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .ends_with(suffix)
    })
}

fn enabled_json_object(enabled: bool, value: &str) -> Result<serde_json::Value> {
    if enabled {
        json_object(value)
    } else {
        Ok(serde_json::json!({}))
    }
}

fn json_object(value: &str) -> Result<serde_json::Value> {
    if value.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    let value: serde_json::Value = serde_json::from_str(value)?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(Error::Config(
            "v0.0.49 model JSON fields must be objects".into(),
        ))
    }
}

fn positive(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

fn optional_string(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两份不同的 v0.0.49 模型,字段覆盖 OpenAI 与 Anthropic 两条路径。
    const LEGACY_CONFIG: &str = r#"
modelAdapters:
  - displayName: Legacy One
    type: openai
    baseURL: https://legacy.example.com/v1
    apiKey: legacy-key-1
    tooltipData: Legacy One
    modelID: legacy-model-1
  - displayName: Legacy Two
    type: anthropic
    baseURL: https://legacy.example.com
    apiKey: legacy-key-2
    tooltipData: Legacy Two
    modelID: legacy-model-2
"#;

    async fn legacy_store(config: &str) -> (tempfile::TempDir, Store, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let path = directory.path().join("config.yaml");
        std::fs::write(&path, config).unwrap();
        (directory, store, path)
    }

    /// 重复导入同一份配置:第一次新增,第二次必须全部按"已存在"跳过,
    /// 已有模型的身份与内容都不被改写。
    #[tokio::test]
    async fn importing_the_same_legacy_configuration_twice_imports_nothing_the_second_time() {
        let (_directory, store, path) = legacy_store(LEGACY_CONFIG).await;

        let first = store.import_v0049_model_config(&path).await.unwrap();
        assert_eq!((first.imported, first.skipped, first.total), (2, 0, 2));
        let imported = store.models().await.unwrap();
        assert_eq!(imported.len(), 2);

        // 预览按内容判断"已存在":第二次导入前 UI 看到 2 个已存在、0 个新增。
        let preview = store.preview_v0049_model_config(&path).await.unwrap();
        assert_eq!(preview.models.len(), 2);
        assert!(preview.models.iter().all(|model| model.existing));

        let second = store.import_v0049_model_config(&path).await.unwrap();
        assert_eq!((second.imported, second.skipped, second.total), (0, 2, 2));
        let after = store.models().await.unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(
            after
                .iter()
                .map(|model| model.model_hash.clone())
                .collect::<Vec<_>>(),
            imported
                .iter()
                .map(|model| model.model_hash.clone())
                .collect::<Vec<_>>()
        );
        let one = after
            .iter()
            .find(|model| model.display_name == "Legacy One")
            .expect("imported model must survive a repeated import");
        assert_eq!(one.api_key, "legacy-key-1");
        assert_eq!(one.display_name, "Legacy One");
    }

    /// 用户改过某条模型后再导入同一份配置:被编辑的那条身份与内容保持原样,
    /// 导入只为"配置里那份原始内容"补一行,不能把编辑结果覆盖回去。
    #[tokio::test]
    async fn a_legacy_import_does_not_overwrite_an_edited_model() {
        let (_directory, store, path) = legacy_store(LEGACY_CONFIG).await;
        store.import_v0049_model_config(&path).await.unwrap();
        let edited = store
            .models()
            .await
            .unwrap()
            .into_iter()
            .find(|model| model.display_name == "Legacy One")
            .unwrap();
        // 直接改行,模拟用户改过显示名之后的内容。
        sqlx::query("UPDATE model_configs SET display_name = 'Renamed One' WHERE model_hash = ?")
            .bind(&edited.model_hash)
            .execute(store.pool())
            .await
            .unwrap();

        let second = store.import_v0049_model_config(&path).await.unwrap();

        assert_eq!((second.imported, second.skipped, second.total), (1, 1, 2));
        let models = store.models().await.unwrap();
        assert_eq!(models.len(), 3);
        let renamed = models
            .iter()
            .find(|model| model.model_hash == edited.model_hash)
            .expect("the edited model must keep its identity");
        assert_eq!(renamed.display_name, "Renamed One");
        let restored = models
            .iter()
            .find(|model| {
                model.model_hash != edited.model_hash && model.display_name == "Legacy One"
            })
            .expect("the original configuration must be imported as a new model");
        assert_eq!(restored.api_key, "legacy-key-1");
    }
}
