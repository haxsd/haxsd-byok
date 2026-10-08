//! Implements model configuration endpoints.
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    model::{ModelConfig, ModelConfigInput},
    Result,
};

use super::{
    ControlService, DiscoveredModels, LegacyModelImportPreview, LegacyModelImportResult,
    ModelConnectivityResult, ModelDiscoveryInput,
};

/// 模型配置的对外表示：明细与内部表示一致，凭据只以"是否已配置"出现。
///
/// `ModelConfig` 的 `api_key` 不参与序列化，所以这里不会带出明文；需要凭据本身的动作
/// （连通性测试、获取模型列表、复制模型）都由服务端读回数据库完成。
#[derive(Serialize)]
pub struct ModelView {
    #[serde(flatten)]
    model: ModelConfig,
    api_key_configured: bool,
}

impl From<ModelConfig> for ModelView {
    fn from(model: ModelConfig) -> Self {
        Self {
            api_key_configured: !model.api_key.trim().is_empty(),
            model,
        }
    }
}

fn views(models: Vec<ModelConfig>) -> Vec<ModelView> {
    models.into_iter().map(ModelView::from).collect()
}

#[derive(Deserialize)]
pub struct SaveModels {
    pub models: Vec<ModelConfigInput>,
}

#[derive(Deserialize)]
pub struct ModelOrder {
    pub model_hashes: Vec<String>,
}

#[derive(Deserialize)]
pub struct DuplicateModel {
    pub display_name: String,
}

pub async fn list(State(service): State<ControlService>) -> Result<Json<Vec<ModelView>>> {
    Ok(Json(views(service.models().await?)))
}

pub async fn create(
    State(service): State<ControlService>,
    Json(input): Json<SaveModels>,
) -> Result<(StatusCode, Json<Vec<ModelView>>)> {
    Ok((
        StatusCode::CREATED,
        Json(views(service.create_models(&input.models).await?)),
    ))
}

pub async fn duplicate(
    State(service): State<ControlService>,
    Path(model_hash): Path<String>,
    Json(input): Json<DuplicateModel>,
) -> Result<(StatusCode, Json<ModelView>)> {
    Ok((
        StatusCode::CREATED,
        Json(ModelView::from(
            service
                .duplicate_model(&model_hash, &input.display_name)
                .await?,
        )),
    ))
}

pub async fn reorder(
    State(service): State<ControlService>,
    Json(input): Json<ModelOrder>,
) -> Result<Json<Vec<ModelView>>> {
    Ok(Json(views(service.reorder_models(&input.model_hashes).await?)))
}

pub async fn remove(
    State(service): State<ControlService>,
    Path(model_hash): Path<String>,
) -> Result<StatusCode> {
    service.delete_model(&model_hash).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn update(
    State(service): State<ControlService>,
    Path(model_hash): Path<String>,
    Json(input): Json<ModelConfigInput>,
) -> Result<Json<ModelView>> {
    Ok(Json(ModelView::from(
        service.update_model(&model_hash, &input).await?,
    )))
}

pub async fn test(
    State(service): State<ControlService>,
    Path((model_hash, test_id)): Path<(String, String)>,
) -> Result<Json<ModelConnectivityResult>> {
    Ok(Json(service.test_model(&model_hash, &test_id).await?))
}

pub async fn cancel(
    State(service): State<ControlService>,
    Path((_model_hash, test_id)): Path<(String, String)>,
) -> Result<StatusCode> {
    service.cancel_model_test(&test_id);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn discover(
    State(service): State<ControlService>,
    Json(input): Json<ModelDiscoveryInput>,
) -> Result<Json<DiscoveredModels>> {
    Ok(Json(service.discover_models(&input).await?))
}

pub async fn import_v0049(
    State(service): State<ControlService>,
) -> Result<Json<LegacyModelImportResult>> {
    Ok(Json(service.import_v0049_models().await?))
}

pub async fn preview_v0049(
    State(service): State<ControlService>,
) -> Result<Json<LegacyModelImportPreview>> {
    Ok(Json(service.preview_v0049_models().await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelType, OPENAI_CHAT_ENDPOINT};

    fn model() -> ModelConfig {
        ModelConfig {
            model_hash: "01234567".into(),
            sort_order: 1,
            display_name: "Test Model".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "sk-secret".into(),
            tooltip_data: "Test Model".into(),
            model_id: "test-model".into(),
            reasoning_effort: None,
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    /// 列表/详情只回"是否已配置"：明文凭据不能出现在响应里。
    #[test]
    fn model_view_reports_only_whether_a_credential_is_configured() {
        let view = serde_json::to_value(ModelView::from(model())).unwrap();
        assert_eq!(view["api_key_configured"], serde_json::json!(true));
        assert!(view.get("api_key").is_none());
        assert!(!view.to_string().contains("sk-secret"));
        // 其余字段原样带出，前端表单不需要第二份形状。
        assert_eq!(view["model_id"], serde_json::json!("test-model"));
        assert_eq!(view["type"], serde_json::json!("openai"));
    }

    /// 空凭据只可能来自"没配过"的历史行：照样只回布尔值，不能回空串。
    #[test]
    fn model_view_reports_a_missing_credential_as_false() {
        let view = serde_json::to_value(ModelView::from(ModelConfig {
            api_key: "  ".into(),
            ..model()
        }))
        .unwrap();
        assert_eq!(view["api_key_configured"], serde_json::json!(false));
    }
}
