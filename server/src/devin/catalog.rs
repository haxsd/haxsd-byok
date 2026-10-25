//! Minimal Devin model catalog compatibility layer.
//!
//! The reference router rewrites the official model catalog in flight. This
//! implementation keeps the same useful wire behavior, but builds a small
//! local catalog from cursor-byok bindings so no upstream account or license
//! behavior is copied into this project.

use crate::{devin::wire, model::ModelConfig, Result};

use super::DevinSettings;

const MODEL_FIELD: u32 = 1;
const DEFAULT_OVERRIDE_FIELD: u32 = 3;
const DEFAULT_OVERRIDE_UID_FIELD: u32 = 3;
const MODEL_NAME_FIELD: u32 = 1;
const MODEL_IDENTITY_FIELD: u32 = 2;
/// `exa.codeium_common_pb.ClientModelConfig.no:5 supports_images`（bool）。
///
/// Devin 客户端据此决定能不能发图：这个字段缺席时它按 false 处理，于是用户一贴图就被
/// 拦下（"This model does not support images."）。Cursor 那侧的模型目录一直把
/// `supports_images` 写死为 true（`cursor/services/model_catalog.rs` 的 `available_model`），
/// 所以这里也报 true：两个客户端对同一份模型库的说法必须一致。
const MODEL_SUPPORTS_IMAGES_FIELD: u32 = 5;
const MODEL_DISPLAY_CONTEXT_FIELD: u32 = 18;
const MODEL_UID_FIELD: u32 = 22;
const MODEL_INFO_FIELD: u32 = 23;
const MODEL_DEFAULT_FIELD: u32 = 31;
const MODEL_METADATA_FIELD: u32 = 30;
const MODEL_INFO_CONTEXT_FIELD: u32 = 4;
const MODEL_INFO_UID_FIELD: u32 = 17;
const MODEL_INFO_SERVER_FIELD: u32 = 18;
const MODEL_INFO_HARNESS_FIELD: u32 = 20;
const IDENTITY_UID_FIELD: u32 = 3;
const METADATA_NAME_FIELD: u32 = 1;

pub fn model_configs_payload(
    settings: &DevinSettings,
    models: &[ModelConfig],
    gateway_url: &str,
) -> Result<Vec<u8>> {
    let mut fields = Vec::new();
    let default_uid = settings
        .default_binding()
        .map(|binding| binding.model_uid.trim());
    for binding in settings.bindings.iter().filter(|binding| binding.enabled) {
        let model = models
            .iter()
            .find(|model| model.model_hash == binding.effective_model_hash());
        let is_default = default_uid.is_some_and(|uid| uid == binding.model_uid.trim());
        fields.push(wire::Field::bytes(
            MODEL_FIELD,
            build_model(binding, model, gateway_url, is_default)?,
        ));
    }
    wire::serialize_fields(&fields)
}

/// A complete model-catalog response. Every current catalog RPC uses field 1
/// for `client_model_configs`; Cascade and CLI additionally accept a default
/// override in field 3.
pub fn catalog_response_payload(
    settings: &DevinSettings,
    models: &[ModelConfig],
    gateway_url: &str,
    include_default_override: bool,
) -> Result<Vec<u8>> {
    let catalog = model_configs_payload(settings, models, gateway_url)?;
    if !include_default_override {
        return Ok(catalog);
    }
    let Some(binding) = settings.default_binding() else {
        return Ok(catalog);
    };
    let mut fields = wire::parse_fields(&catalog)?;
    fields.push(wire::Field::bytes(
        DEFAULT_OVERRIDE_FIELD,
        wire::serialize_fields(&[wire::Field::bytes(
            DEFAULT_OVERRIDE_UID_FIELD,
            binding.model_uid.trim(),
        )])?,
    ));
    wire::serialize_fields(&fields)
}

fn build_model(
    binding: &super::DevinModelBinding,
    model: Option<&ModelConfig>,
    gateway_url: &str,
    is_default: bool,
) -> Result<Vec<u8>> {
    let uid = binding.model_uid.trim();
    let display_name = model
        .map(|model| model.display_name.trim())
        .filter(|value| !value.is_empty())
        .or_else(|| (!binding.display_name.trim().is_empty()).then(|| binding.display_name.trim()))
        .unwrap_or(uid);
    let context = model
        .and_then(|model| model.context_window_tokens)
        .or(binding.context_window_tokens)
        .unwrap_or_default();
    let harness_uid = format!("cursor-byok:{uid}");
    let mut model_info = vec![
        wire::Field::bytes(MODEL_INFO_UID_FIELD, uid),
        wire::Field::bytes(MODEL_INFO_SERVER_FIELD, gateway_url.trim_end_matches('/')),
        wire::Field::bytes(MODEL_INFO_HARNESS_FIELD, &harness_uid),
    ];
    if context > 0 {
        model_info.push(wire::Field::varint(MODEL_INFO_CONTEXT_FIELD, context));
    }
    let mut fields = vec![
        wire::Field::bytes(MODEL_NAME_FIELD, display_name),
        wire::Field::bytes(
            MODEL_IDENTITY_FIELD,
            wire::serialize_fields(&[wire::Field::bytes(IDENTITY_UID_FIELD, uid)])?,
        ),
        wire::Field::bytes(MODEL_UID_FIELD, uid),
        wire::Field::bytes(MODEL_INFO_FIELD, wire::serialize_fields(&model_info)?),
        wire::Field::bytes(
            MODEL_METADATA_FIELD,
            wire::serialize_fields(&[wire::Field::bytes(METADATA_NAME_FIELD, display_name)])?,
        ),
        // 与 Cursor 侧一致：模型库里的模型按「能看图」报给客户端。
        wire::Field::varint(MODEL_SUPPORTS_IMAGES_FIELD, 1),
    ];
    if context > 0 {
        fields.push(wire::Field::varint(MODEL_DISPLAY_CONTEXT_FIELD, context));
    }
    if is_default {
        fields.push(wire::Field::varint(MODEL_DEFAULT_FIELD, 1));
    }
    wire::serialize_fields(&fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devin::{DevinBindingKind, DevinRoute};
    use crate::model::{ModelType, OPENAI_CHAT_ENDPOINT};

    fn varint_field(fields: &[wire::Field], number: u32) -> Option<u64> {
        fields
            .iter()
            .find_map(|field| match (&field.number, &field.value) {
                (field_number, wire::FieldValue::Varint(value)) if *field_number == number => {
                    Some(*value)
                }
                _ => None,
            })
    }

    fn bytes_field(fields: &[wire::Field], number: u32) -> Option<String> {
        fields
            .iter()
            .find_map(|field| match (&field.number, &field.value) {
                (field_number, wire::FieldValue::Bytes(value)) if *field_number == number => {
                    String::from_utf8(value.clone()).ok()
                }
                _ => None,
            })
    }

    /// 模型记录里的 UID：生产侧的 rewrite 路径已经删掉，这里保留测试自己的读法。
    fn model_uid(fields: &[wire::Field]) -> Option<String> {
        fields
            .iter()
            .find_map(|field| match (&field.number, &field.value) {
                (number, wire::FieldValue::Bytes(value)) if *number == MODEL_UID_FIELD => {
                    String::from_utf8(value.clone())
                        .ok()
                        .filter(|value| !value.is_empty())
                }
                _ => None,
            })
    }

    /// 一整个 payload 里第一条模型记录的字段。
    fn first_model_fields(payload: &[u8]) -> Vec<wire::Field> {
        let outer = wire::parse_fields(payload).unwrap();
        match &outer[0].value {
            wire::FieldValue::Bytes(value) => wire::parse_fields(value).unwrap(),
            _ => panic!("model is not a bytes field"),
        }
    }

    fn settings() -> DevinSettings {
        DevinSettings {
            bindings: vec![
                super::super::DevinModelBinding {
                    model_uid: "model-a".into(),
                    model_hash: "hash-a".into(),
                    display_name: "Model A".into(),
                    context_window_tokens: Some(128_000),
                    enabled: true,
                    ..super::super::DevinModelBinding::new("", "")
                },
                super::super::DevinModelBinding {
                    model_uid: "model-disabled".into(),
                    model_hash: "hash-disabled".into(),
                    display_name: "Disabled".into(),
                    context_window_tokens: None,
                    enabled: false,
                    ..super::super::DevinModelBinding::new("", "")
                },
            ],
            default_model_uid: Some("model-a".into()),
            ..Default::default()
        }
    }

    fn all_model_fields(payload: &[u8]) -> Vec<Vec<wire::Field>> {
        wire::parse_fields(payload)
            .unwrap()
            .into_iter()
            .filter(|field| field.number == MODEL_FIELD)
            .filter_map(|field| match field.value {
                wire::FieldValue::Bytes(value) => wire::parse_fields(&value).ok(),
                _ => None,
            })
            .collect()
    }

    fn default_override_uid(payload: &[u8]) -> Option<String> {
        let fields = wire::parse_fields(payload).ok()?;
        let field = fields
            .iter()
            .find(|field| field.number == DEFAULT_OVERRIDE_FIELD)?;
        let wire::FieldValue::Bytes(value) = &field.value else {
            return None;
        };
        let inner = wire::parse_fields(value).ok()?;
        bytes_field(&inner, DEFAULT_OVERRIDE_UID_FIELD)
    }

    fn model_config(hash: &str, display_name: &str, context: u64) -> ModelConfig {
        ModelConfig {
            model_hash: hash.into(),
            sort_order: 0,
            display_name: display_name.into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "http://127.0.0.1:9000".into(),
            use_full_url: false,
            api_key: String::new(),
            tooltip_data: String::new(),
            model_id: hash.into(),
            reasoning_effort: None,
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: Some(context),
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn generated_catalog_contains_enabled_bindings_only() {
        let payload = model_configs_payload(&settings(), &[], "http://127.0.0.1:43112").unwrap();
        let fields = wire::parse_fields(&payload).unwrap();
        assert_eq!(fields.len(), 1);
        let model = match &fields[0].value {
            wire::FieldValue::Bytes(value) => value,
            _ => panic!("model is not a bytes field"),
        };
        assert_eq!(
            model_uid(&wire::parse_fields(model).unwrap()).as_deref(),
            Some("model-a")
        );
    }

    #[test]
    fn default_override_names_the_explicit_default_binding() {
        let payload =
            catalog_response_payload(&settings(), &[], "http://127.0.0.1:43112", true).unwrap();
        assert_eq!(default_override_uid(&payload).as_deref(), Some("model-a"));
    }

    #[test]
    fn default_flag_and_override_follow_the_selected_binding() {
        let mut settings = settings();
        settings
            .bindings
            .push(super::super::DevinModelBinding::new("model-b", "hash-b"));
        settings.default_model_uid = Some("model-b".into());

        let payload = model_configs_payload(&settings, &[], "http://127.0.0.1:43112").unwrap();
        let models = all_model_fields(&payload);
        assert_eq!(models.len(), 2);
        assert_eq!(varint_field(&models[0], MODEL_DEFAULT_FIELD), None);
        assert_eq!(varint_field(&models[1], MODEL_DEFAULT_FIELD), Some(1));

        let catalog =
            catalog_response_payload(&settings, &[], "http://127.0.0.1:43112", true).unwrap();
        assert_eq!(default_override_uid(&catalog).as_deref(), Some("model-b"));
    }

    #[test]
    fn no_explicit_default_leaves_selection_to_the_client() {
        let mut settings = settings();
        settings.default_model_uid = None;

        let payload = model_configs_payload(&settings, &[], "http://127.0.0.1:43112").unwrap();
        for model in all_model_fields(&payload) {
            assert_eq!(varint_field(&model, MODEL_DEFAULT_FIELD), None);
        }

        let catalog =
            catalog_response_payload(&settings, &[], "http://127.0.0.1:43112", true).unwrap();
        assert_eq!(default_override_uid(&catalog), None);
    }

    #[test]
    fn generated_catalog_advertises_image_support() {
        // Devin 客户端只在模型带 supports_images 时才允许贴图；缺席即按 false 处理，
        // 用户就会被 "This model does not support images." 拦下。
        let payload = model_configs_payload(&settings(), &[], "http://127.0.0.1:43112").unwrap();
        assert_eq!(
            varint_field(&first_model_fields(&payload), MODEL_SUPPORTS_IMAGES_FIELD),
            Some(1)
        );
    }

    #[test]
    fn catalog_generation_does_not_mutate_binding_route_state() {
        let mut settings = settings();
        settings.bindings[0].kind = DevinBindingKind::ContextCompression;
        settings.bindings[0].routes = vec![
            DevinRoute {
                route_id: "secondary".into(),
                model_hash: "hash-secondary".into(),
                label: "Secondary".into(),
                enabled: false,
            },
            DevinRoute {
                route_id: "primary".into(),
                model_hash: "hash-primary".into(),
                label: "Primary".into(),
                enabled: true,
            },
        ];
        settings.bindings[0].active_route_id = Some("primary".into());

        let before = serde_json::to_value(&settings).unwrap();
        // 生成目录只读设置：它不该改动绑定里的路由状态。
        model_configs_payload(&settings, &[], "http://127.0.0.1:43112").unwrap();
        let after = serde_json::to_value(&settings).unwrap();

        assert_eq!(before, after);
        let binding = &settings.bindings[0];
        assert_eq!(binding.kind, DevinBindingKind::ContextCompression);
        assert_eq!(binding.active_route_id.as_deref(), Some("primary"));
        assert_eq!(
            binding
                .routes
                .iter()
                .map(|route| (route.route_id.as_str(), route.enabled))
                .collect::<Vec<_>>(),
            vec![("secondary", false), ("primary", true)]
        );
        assert_eq!(binding.routes[0].label, "Secondary");
        assert!(!settings.bindings[1].enabled);
    }

    #[test]
    fn catalog_prefers_current_model_library_name_and_context() {
        let payload = model_configs_payload(
            &settings(),
            &[model_config("hash-a", "Renamed model", 256_000)],
            "http://127.0.0.1:43112",
        )
        .unwrap();
        let fields = first_model_fields(&payload);

        assert_eq!(
            bytes_field(&fields, MODEL_NAME_FIELD).as_deref(),
            Some("Renamed model")
        );
        assert_eq!(
            varint_field(&fields, MODEL_DISPLAY_CONTEXT_FIELD),
            Some(256_000)
        );
    }
}
