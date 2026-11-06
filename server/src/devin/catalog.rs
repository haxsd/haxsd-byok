//! Minimal Devin model catalog compatibility layer.
//!
//! The reference router rewrites the official model catalog in flight. This
//! implementation keeps the same useful wire behavior, but builds a small
//! local catalog from cursor-byok bindings so no upstream account or license
//! behavior is copied into this project.

use std::collections::HashSet;

use crate::{devin::wire, Error, Result};

use super::directory::LocalModelEntry;

#[cfg(test)]
use {
    super::{directory::local_entries, DevinSettings},
    crate::model::ModelConfig,
};

pub(super) const MODEL_FIELD: u32 = 1;
pub(super) const DEFAULT_OVERRIDE_FIELD: u32 = 3;
const SORT_FIELD: u32 = 2;
const SORT_NAME_FIELD: u32 = 1;
const SORT_GROUP_FIELD: u32 = 2;
const GROUP_LABEL_FIELD: u32 = 2;
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
pub(super) const MODEL_UID_FIELD: u32 = 22;
const MODEL_INFO_FIELD: u32 = 23;
const MODEL_DEFAULT_FIELD: u32 = 31;
const MODEL_METADATA_FIELD: u32 = 30;
const MODEL_INFO_CONTEXT_FIELD: u32 = 4;
const MODEL_INFO_UID_FIELD: u32 = 17;
const MODEL_INFO_SERVER_FIELD: u32 = 18;
const MODEL_INFO_FAMILY_FIELD: u32 = 23;
const IDENTITY_UID_FIELD: u32 = 3;
const METADATA_NAME_FIELD: u32 = 1;

#[cfg(test)]
fn model_configs_payload(
    settings: &DevinSettings,
    models: &[ModelConfig],
    gateway_url: &str,
) -> Result<Vec<u8>> {
    let entries = local_entries(settings, models)?;
    wire::serialize_fields(&encoded_models(&entries, gateway_url)?)
}

/// A complete model-catalog response. Every current catalog RPC uses field 1
/// for `client_model_configs`; Cascade and CLI additionally accept a default
/// override in field 3.
#[cfg(test)]
fn catalog_response_payload(
    settings: &DevinSettings,
    models: &[ModelConfig],
    gateway_url: &str,
    include_default_override: bool,
) -> Result<Vec<u8>> {
    let entries = local_entries(settings, models)?;
    let mut fields = encoded_models(&entries, gateway_url)?;
    if !include_default_override {
        return wire::serialize_fields(&fields);
    }
    let Some(entry) = entries.iter().find(|entry| entry.default) else {
        return wire::serialize_fields(&fields);
    };
    fields.push(default_override_field(&entry.uid)?);
    wire::serialize_fields(&fields)
}

fn encoded_models(entries: &[LocalModelEntry], gateway_url: &str) -> Result<Vec<wire::Field>> {
    entries
        .iter()
        .map(|entry| {
            build_model(entry, gateway_url).map(|model| wire::Field::bytes(MODEL_FIELD, model))
        })
        .collect()
}

pub(super) fn client_model_fields(
    entries: &[LocalModelEntry],
    gateway_url: &str,
) -> Result<Vec<wire::Field>> {
    encoded_models(entries, gateway_url)
}

pub(super) fn with_local_models(
    payload: &[u8],
    local_models: &[wire::Field],
    local_default_uid: Option<&str>,
    include_sorts: bool,
) -> Result<Vec<u8>> {
    if local_models.is_empty() && local_default_uid.is_none() {
        return Ok(payload.to_vec());
    }
    let mut fields = wire::parse_fields(payload)?;
    let original = fields.clone();
    let harnesses = super::harness::HarnessCatalog::from_fields(&fields)?;
    let mut existing = wire::all_fields(&fields, MODEL_FIELD)
        .filter_map(client_model_uid)
        .collect::<HashSet<_>>();
    let labels = wire::all_fields(&fields, MODEL_FIELD)
        .filter_map(client_model_label)
        .collect::<HashSet<_>>();
    let mut local_labels = HashSet::new();
    let mut additions = Vec::new();
    for local in local_models {
        let local = harnesses.attach(local)?;
        let uid = client_model_uid(&local).ok_or_else(|| {
            Error::Protocol("generated Devin local model has no model UID".into())
        })?;
        if existing.insert(uid) {
            let label = client_model_label(&local).ok_or_else(|| {
                Error::Protocol("generated Devin local model has no label".into())
            })?;
            if labels.contains(&label) {
                return Err(Error::Protocol(
                    "Devin local model label conflicts with an official model".into(),
                ));
            }
            if !local_labels.insert(label) {
                return Err(Error::Protocol(
                    "Devin local model labels must be unique".into(),
                ));
            }
            additions.push(local.clone());
        }
    }
    if include_sorts {
        append_local_model_labels(&mut fields, &additions)?;
    }
    fields.extend(additions);
    if let Some(uid) = local_default_uid
        .map(str::trim)
        .filter(|uid| !uid.is_empty())
    {
        fields.retain(|field| field.number != DEFAULT_OVERRIDE_FIELD);
        fields.push(default_override_field(uid)?);
    }
    if fields == original {
        return Ok(payload.to_vec());
    }
    wire::serialize_fields(&fields)
}

pub(super) fn client_model_uid(field: &wire::Field) -> Option<String> {
    if field.number != MODEL_FIELD {
        return None;
    }
    let wire::FieldValue::Bytes(model) = &field.value else {
        return None;
    };
    let fields = wire::parse_fields(model).ok()?;
    let wire::FieldValue::Bytes(uid) = &wire::first_field(&fields, MODEL_UID_FIELD)?.value else {
        return None;
    };
    String::from_utf8(uid.clone())
        .ok()
        .map(|uid| uid.trim().to_owned())
        .filter(|uid| !uid.is_empty())
}

pub(super) fn client_model_label(field: &wire::Field) -> Option<String> {
    if field.number != MODEL_FIELD {
        return None;
    }
    let wire::FieldValue::Bytes(model) = &field.value else {
        return None;
    };
    let fields = wire::parse_fields(model).ok()?;
    text_field(&fields, MODEL_NAME_FIELD).filter(|label| !label.trim().is_empty())
}

/// Devin renders `client_model_configs` through `client_model_sorts` groups.
/// A model without a label in a default group is searchable but absent from the
/// normal picker, so local models must join the first default-sort group.
pub(super) fn append_local_model_labels(
    catalog: &mut Vec<wire::Field>,
    additions: &[wire::Field],
) -> Result<bool> {
    let labels = additions
        .iter()
        .filter_map(client_model_label)
        .collect::<Vec<_>>();
    if labels.is_empty() {
        return Ok(false);
    }

    let mut unique_local = HashSet::with_capacity(labels.len());
    for label in &labels {
        if !unique_local.insert(label.clone()) {
            return Err(Error::Protocol(format!(
                "Devin local model labels must be unique: {label}"
            )));
        }
    }
    let existing_labels = wire::all_fields(catalog, MODEL_FIELD)
        .filter_map(client_model_label)
        .collect::<HashSet<_>>();
    for label in &labels {
        if existing_labels.contains(label) {
            return Err(Error::Protocol(format!(
                "Devin local model label conflicts with an official model: {label}"
            )));
        }
    }

    let Some(sort_index) = default_sort_index(catalog) else {
        let group = local_group(&labels)?;
        catalog.push(wire::Field::bytes(
            SORT_FIELD,
            wire::serialize_fields(&[wire::Field::bytes(SORT_NAME_FIELD, "All"), group])?,
        ));
        return Ok(true);
    };

    let wire::FieldValue::Bytes(sort_bytes) = &catalog[sort_index].value else {
        return Err(Error::Protocol("Devin model sort is not a message".into()));
    };
    let mut sort = wire::parse_fields(sort_bytes)?;
    let Some(group_index) = sort
        .iter()
        .position(|field| field.number == SORT_GROUP_FIELD)
    else {
        sort.push(local_group(&labels)?);
        catalog[sort_index] = wire::Field::bytes(SORT_FIELD, wire::serialize_fields(&sort)?);
        return Ok(true);
    };

    let wire::FieldValue::Bytes(group_bytes) = &sort[group_index].value else {
        return Err(Error::Protocol("Devin model group is not a message".into()));
    };
    let mut group = wire::parse_fields(group_bytes)?;
    let mut present = group
        .iter()
        .filter(|field| field.number == GROUP_LABEL_FIELD)
        .filter_map(|field| match &field.value {
            wire::FieldValue::Bytes(value) => String::from_utf8(value.clone()).ok(),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut changed = false;
    for label in labels {
        if present.insert(label.clone()) {
            group.push(wire::Field::bytes(GROUP_LABEL_FIELD, &label));
            changed = true;
        }
    }
    if changed {
        sort[group_index] = wire::Field::bytes(SORT_GROUP_FIELD, wire::serialize_fields(&group)?);
        catalog[sort_index] = wire::Field::bytes(SORT_FIELD, wire::serialize_fields(&sort)?);
    }
    Ok(changed)
}

fn default_sort_index(catalog: &[wire::Field]) -> Option<usize> {
    let mut first = None;
    for (index, field) in catalog.iter().enumerate() {
        if field.number != SORT_FIELD {
            continue;
        }
        let wire::FieldValue::Bytes(bytes) = &field.value else {
            continue;
        };
        let Ok(fields) = wire::parse_fields(bytes) else {
            continue;
        };
        first.get_or_insert(index);
        if matches!(
            text_field(&fields, SORT_NAME_FIELD).as_deref(),
            Some("Recommended" | "All")
        ) {
            return Some(index);
        }
    }
    first
}

fn local_group(labels: &[String]) -> Result<wire::Field> {
    let fields = labels
        .iter()
        .map(|label| wire::Field::bytes(GROUP_LABEL_FIELD, label))
        .collect::<Vec<_>>();
    Ok(wire::Field::bytes(
        SORT_GROUP_FIELD,
        wire::serialize_fields(&fields)?,
    ))
}

fn text_field(fields: &[wire::Field], number: u32) -> Option<String> {
    let wire::FieldValue::Bytes(value) = &wire::first_field(fields, number)?.value else {
        return None;
    };
    String::from_utf8(value.clone()).ok()
}

pub(super) fn default_override_field(uid: &str) -> Result<wire::Field> {
    Ok(wire::Field::bytes(
        DEFAULT_OVERRIDE_FIELD,
        wire::serialize_fields(&[wire::Field::bytes(DEFAULT_OVERRIDE_UID_FIELD, uid.trim())])?,
    ))
}

fn build_model(entry: &LocalModelEntry, gateway_url: &str) -> Result<Vec<u8>> {
    let uid = entry.uid.as_str();
    let display_name = entry
        .parameters
        .as_ref()
        .filter(|parameters| parameters.uid != parameters.family_uid)
        .map(|parameters| {
            format!(
                "{} · {} · {}",
                entry.display_name,
                parameters.effort_name(),
                parameters.context_name()
            )
        })
        .unwrap_or_else(|| entry.display_name.clone());
    let context = entry.context_window_tokens.unwrap_or_default();
    let mut model_info = vec![
        wire::Field::bytes(MODEL_INFO_UID_FIELD, uid),
        wire::Field::bytes(MODEL_INFO_SERVER_FIELD, gateway_url.trim_end_matches('/')),
    ];
    if context > 0 {
        model_info.push(wire::Field::varint(MODEL_INFO_CONTEXT_FIELD, context));
    }
    if let Some(parameters) = &entry.parameters {
        model_info.push(wire::Field::bytes(8, &parameters.model_id));
        model_info.push(wire::Field::bytes(
            MODEL_INFO_FAMILY_FIELD,
            &parameters.family_uid,
        ));
    }
    let family_default = entry
        .parameters
        .as_ref()
        .is_some_and(|parameters| parameters.uid == parameters.family_uid);
    let mut metadata = vec![wire::Field::bytes(METADATA_NAME_FIELD, &entry.display_name)];
    if let Some(parameters) = &entry.parameters {
        metadata.push(parameter_dimension(
            "Reasoning Effort",
            parameters.effort_order(),
            parameters.effort_name(),
            1,
        )?);
        metadata.push(parameter_dimension(
            "Context size",
            parameters.context_order(),
            &parameters.context_name(),
            3,
        )?);
        metadata.push(wire::Field::varint(3, u64::from(family_default)));
    }
    let mut fields = vec![
        wire::Field::bytes(MODEL_NAME_FIELD, &display_name),
        wire::Field::bytes(
            MODEL_IDENTITY_FIELD,
            wire::serialize_fields(&[wire::Field::bytes(IDENTITY_UID_FIELD, uid)])?,
        ),
        wire::Field::bytes(MODEL_UID_FIELD, uid),
        wire::Field::bytes(MODEL_INFO_FIELD, wire::serialize_fields(&model_info)?),
        wire::Field::bytes(MODEL_METADATA_FIELD, wire::serialize_fields(&metadata)?),
        // 与 Cursor 侧一致：模型库里的模型按「能看图」报给客户端。
        wire::Field::varint(MODEL_SUPPORTS_IMAGES_FIELD, 1),
    ];
    if context > 0 {
        fields.push(wire::Field::varint(MODEL_DISPLAY_CONTEXT_FIELD, context));
    }
    if entry.default || family_default {
        fields.push(wire::Field::varint(MODEL_DEFAULT_FIELD, 1));
    }
    wire::serialize_fields(&fields)
}

fn parameter_dimension(name: &str, order: u64, value: &str, control: u64) -> Result<wire::Field> {
    let value = wire::serialize_fields(&[
        wire::Field::varint(1, order),
        wire::Field::bytes(2, value),
        wire::Field::varint(3, control),
    ])?;
    Ok(wire::Field::bytes(
        2,
        wire::serialize_fields(&[wire::Field::bytes(1, name), wire::Field::bytes(2, value)])?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devin::DevinRoute;
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

    #[test]
    fn native_parameters_are_encoded_as_family_dimensions() {
        let settings = DevinSettings {
            bindings: vec![super::super::DevinModelBinding::new(
                "haxsd-byok:a",
                "hash-a",
            )],
            default_model_uid: Some("haxsd-byok:a".into()),
            ..DevinSettings::default()
        };
        let mut model = model_config("hash-a", "My model", 500_000);
        model.reasoning_effort = Some("max".into());
        let entries = super::super::directory::native_entries(&settings, &[model]).unwrap();
        let encoded = client_model_fields(&entries, "http://127.0.0.1:43112").unwrap();
        let model = encoded
            .iter()
            .find(|field| client_model_uid(field).as_deref() == Some("haxsd-byok:a~low~128000"))
            .unwrap();
        let wire::FieldValue::Bytes(bytes) = &model.value else {
            panic!("model message")
        };
        let fields = wire::parse_fields(bytes).unwrap();
        let wire::FieldValue::Bytes(info) =
            &wire::first_field(&fields, MODEL_INFO_FIELD).unwrap().value
        else {
            panic!("model info")
        };
        let info = wire::parse_fields(info).unwrap();
        assert_eq!(
            bytes_field(&info, MODEL_INFO_FAMILY_FIELD).as_deref(),
            Some("haxsd-byok:a")
        );
        assert_eq!(varint_field(&info, MODEL_INFO_CONTEXT_FIELD), Some(128_000));
        let wire::FieldValue::Bytes(metadata) = &wire::first_field(&fields, MODEL_METADATA_FIELD)
            .unwrap()
            .value
        else {
            panic!("family metadata")
        };
        let metadata = wire::parse_fields(metadata).unwrap();
        assert_eq!(bytes_field(&metadata, 1).as_deref(), Some("My model"));
        let dimensions = wire::all_fields(&metadata, 2)
            .map(|field| {
                let wire::FieldValue::Bytes(bytes) = &field.value else {
                    panic!("dimension")
                };
                let fields = wire::parse_fields(bytes).unwrap();
                let wire::FieldValue::Bytes(value) = &wire::first_field(&fields, 2).unwrap().value
                else {
                    panic!("dimension value")
                };
                let value = wire::parse_fields(value).unwrap();
                (
                    bytes_field(&fields, 1).unwrap(),
                    bytes_field(&value, 2).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            dimensions,
            vec![
                ("Reasoning Effort".into(), "Low".into()),
                ("Context size".into(), "128K".into())
            ]
        );
    }

    #[test]
    fn native_models_reuse_official_harnesses_without_matching_partial_aliases() {
        let official = wire::Field::bytes(
            1,
            wire::serialize_fields(&[
                wire::Field::bytes(1, "Official Sol"),
                wire::Field::bytes(22, "gpt-6-1-sol-medium"),
                wire::Field::bytes(
                    23,
                    wire::serialize_fields(&[
                        wire::Field::bytes(23, "gpt-6-1-sol"),
                        wire::Field::bytes(20, "gpt-5p6-codex"),
                        wire::Field::bytes(20, "gpt-5p6"),
                    ])
                    .unwrap(),
                ),
            ])
            .unwrap(),
        );
        let payload = wire::serialize_fields(&[official.clone()]).unwrap();
        for (model_id, expected) in [
            ("gpt-6.1-sol", vec!["gpt-5p6-codex", "gpt-5p6"]),
            ("gpt-6.1", Vec::new()),
            ("deepseek-flash", Vec::new()),
        ] {
            let local = wire::Field::bytes(
                1,
                wire::serialize_fields(&[
                    wire::Field::bytes(1, "My model"),
                    wire::Field::bytes(22, "haxsd-byok:a"),
                    wire::Field::bytes(
                        23,
                        wire::serialize_fields(&[
                            wire::Field::bytes(8, model_id),
                            wire::Field::bytes(17, "haxsd-byok:a"),
                            wire::Field::bytes(20, "cursor-byok:haxsd-byok:a"),
                        ])
                        .unwrap(),
                    ),
                ])
                .unwrap(),
            );
            let output = with_local_models(&payload, &[local], None, false).unwrap();
            let output = wire::parse_fields(&output).unwrap();
            assert_eq!(output[0], official);
            let wire::FieldValue::Bytes(local) = &output[1].value else {
                panic!("local model");
            };
            let local = wire::parse_fields(local).unwrap();
            let wire::FieldValue::Bytes(info) = &wire::first_field(&local, 23).unwrap().value
            else {
                panic!("model info");
            };
            let info = wire::parse_fields(info).unwrap();
            let harnesses = wire::all_fields(&info, 20)
                .map(|field| match &field.value {
                    wire::FieldValue::Bytes(value) => String::from_utf8(value.clone()).unwrap(),
                    _ => panic!("harness UID"),
                })
                .collect::<Vec<_>>();
            assert_eq!(harnesses, expected, "{model_id}");
        }
    }
}
