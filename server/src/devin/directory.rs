//! Semantic entries for the local Devin model directory.
//!
//! Wire encoding stays in `catalog.rs` and `user_status.rs`. This module owns
//! which enabled bindings exist, what they display, and which one is default.

use std::collections::HashSet;

use crate::{model::ModelConfig, Error, Result};

use super::variants::{model_variants, NativeModelVariant};
use super::DevinSettings;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalModelEntry {
    pub uid: String,
    pub display_name: String,
    pub model_hash: String,
    pub context_window_tokens: Option<u64>,
    pub default: bool,
    pub parameters: Option<NativeModelVariant>,
    /// 来自模型库的图片能力；绑定的模型已不在库里时按缺省值（支持）报。
    pub supports_images: bool,
}

pub const NATIVE_MODEL_UID_PREFIX: &str = "haxsd-byok:";

pub fn local_entries(
    settings: &DevinSettings,
    models: &[ModelConfig],
) -> Result<Vec<LocalModelEntry>> {
    let default_uid = settings
        .default_binding()
        .map(|binding| binding.model_uid.trim());
    let mut uids = HashSet::with_capacity(settings.bindings.len());
    let mut entries = Vec::with_capacity(settings.bindings.len());

    for binding in settings.bindings.iter().filter(|binding| binding.enabled) {
        let uid = binding.model_uid.trim();
        if uid.is_empty() {
            return Err(Error::Config("Devin model UID must not be empty".into()));
        }
        let model_hash = binding.effective_model_hash();
        if model_hash.is_empty() {
            return Err(Error::Config(format!(
                "Devin model binding for {uid} must include a cursor-byok model hash"
            )));
        }
        if !uids.insert(uid.to_owned()) {
            return Err(Error::Config(format!("duplicate Devin model UID: {uid}")));
        }

        let model = models.iter().find(|model| model.model_hash == model_hash);
        let display_name = model
            .map(|model| model.display_name.trim())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                (!binding.display_name.trim().is_empty()).then(|| binding.display_name.trim())
            })
            .unwrap_or(uid);
        entries.push(LocalModelEntry {
            uid: uid.to_owned(),
            display_name: display_name.to_owned(),
            model_hash: model_hash.to_owned(),
            context_window_tokens: model
                .and_then(|model| model.context_window_tokens)
                .or(binding.context_window_tokens)
                .filter(|tokens| *tokens > 0),
            default: default_uid.is_some_and(|default_uid| default_uid == uid),
            parameters: None,
            supports_images: model.is_none_or(|model| model.supports_images),
        });
    }

    Ok(entries)
}

pub fn is_native_model_uid(uid: &str) -> bool {
    uid.trim().starts_with(NATIVE_MODEL_UID_PREFIX)
}

pub fn native_entries(
    settings: &DevinSettings,
    models: &[ModelConfig],
) -> Result<Vec<LocalModelEntry>> {
    let standard_native_uids = settings
        .bindings
        .iter()
        .filter(|binding| binding.enabled && is_native_model_uid(&binding.model_uid))
        .map(|binding| binding.model_uid.trim())
        .collect::<HashSet<_>>();
    let mut entries = Vec::new();
    for entry in local_entries(settings, models)?
        .into_iter()
        .filter(|entry| standard_native_uids.contains(entry.uid.as_str()))
    {
        let Some(model) = models
            .iter()
            .find(|model| model.model_hash == entry.model_hash)
        else {
            entries.push(entry);
            continue;
        };
        for parameters in model_variants(&entry.uid, model, entry.context_window_tokens) {
            entries.push(LocalModelEntry {
                uid: parameters.uid.clone(),
                context_window_tokens: parameters.context_window_tokens,
                default: entry.default && parameters.uid == entry.uid,
                parameters: Some(parameters),
                ..entry.clone()
            });
        }
    }
    let unique = entries
        .iter()
        .map(|entry| &entry.uid)
        .collect::<HashSet<_>>();
    if unique.len() != entries.len() {
        return Err(Error::Config("duplicate Devin parameter identity".into()));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        devin::{DevinModelBinding, DevinRoute},
        model::{ModelConfig, ModelType, OPENAI_CHAT_ENDPOINT},
    };

    fn binding(uid: &str, hash: &str, display_name: &str) -> DevinModelBinding {
        DevinModelBinding {
            model_uid: uid.into(),
            model_hash: hash.into(),
            display_name: display_name.into(),
            ..DevinModelBinding::new("", "")
        }
    }

    fn route(route_id: &str, hash: &str) -> DevinRoute {
        DevinRoute {
            route_id: route_id.into(),
            model_hash: hash.into(),
            label: route_id.into(),
            enabled: true,
        }
    }

    fn model(hash: &str, display_name: &str, context: Option<u64>) -> ModelConfig {
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
            context_window_tokens: context,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
            supports_images: true,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn settings(bindings: Vec<DevinModelBinding>, default: Option<&str>) -> DevinSettings {
        DevinSettings {
            bindings,
            default_model_uid: default.map(str::to_owned),
            ..DevinSettings::default()
        }
    }

    #[test]
    fn enabled_bindings_keep_order_and_one_explicit_default() {
        let mut disabled = binding("devin-disabled", "hash-c", "Disabled");
        disabled.enabled = false;
        let settings = settings(
            vec![
                binding("devin-a", "hash-a", "Binding A"),
                binding("devin-b", "hash-b", "Binding B"),
                disabled,
            ],
            Some("devin-b"),
        );
        let models = vec![
            model("hash-a", "Library A", Some(128_000)),
            model("hash-b", "Library B", Some(256_000)),
        ];

        assert_eq!(
            local_entries(&settings, &models).unwrap(),
            vec![
                LocalModelEntry {
                    uid: "devin-a".into(),
                    display_name: "Library A".into(),
                    model_hash: "hash-a".into(),
                    context_window_tokens: Some(128_000),
                    default: false,
                    parameters: None,
                    supports_images: true,
                },
                LocalModelEntry {
                    uid: "devin-b".into(),
                    display_name: "Library B".into(),
                    model_hash: "hash-b".into(),
                    context_window_tokens: Some(256_000),
                    default: true,
                    parameters: None,
                    supports_images: true,
                },
            ]
        );
    }

    #[test]
    fn active_route_supplies_the_effective_hash_and_metadata() {
        let mut active = binding("devin-a", "legacy-hash", "Binding A");
        active.routes = vec![route("route-a", "route-hash")];
        active.active_route_id = Some("route-a".into());
        let models = vec![
            model("legacy-hash", "Legacy", Some(32_000)),
            model("route-hash", "Active route", Some(200_000)),
        ];

        assert_eq!(
            local_entries(&settings(vec![active], None), &models).unwrap(),
            vec![LocalModelEntry {
                uid: "devin-a".into(),
                display_name: "Active route".into(),
                model_hash: "route-hash".into(),
                context_window_tokens: Some(200_000),
                default: false,
                parameters: None,
                supports_images: true,
            }]
        );
    }

    #[test]
    fn duplicate_and_empty_uids_are_rejected() {
        let duplicate = settings(
            vec![
                binding("devin-a", "hash-a", "First"),
                binding("devin-a", "hash-b", "Second"),
            ],
            None,
        );
        assert!(local_entries(&duplicate, &[])
            .unwrap_err()
            .to_string()
            .contains("duplicate Devin model UID"));

        let empty = settings(vec![binding(" ", "hash-a", "A")], None);
        assert!(local_entries(&empty, &[])
            .unwrap_err()
            .to_string()
            .contains("must not be empty"));
    }

    #[test]
    fn native_entries_keep_only_enabled_namespaced_bindings() {
        let mut disabled = binding("haxsd-byok:model-c", "hash-c", "Disabled");
        disabled.enabled = false;
        let settings = settings(
            vec![
                binding("swe-1-6-slow", "hash-official", "Official mapping"),
                binding("haxsd-byok:model-a", "hash-a", "Local A"),
                binding("haxsd-byok:model-b", "hash-b", "Local B"),
                disabled,
            ],
            Some("haxsd-byok:model-a"),
        );
        let models = vec![
            model("hash-official", "Official model", Some(128_000)),
            model("hash-a", "Local model A", Some(128_000)),
            model("hash-b", "Local model B", Some(128_000)),
        ];

        assert_eq!(
            native_entries(&settings, &models)
                .unwrap()
                .iter()
                .filter(|entry| entry
                    .parameters
                    .as_ref()
                    .is_none_or(|parameters| parameters.uid == parameters.family_uid))
                .map(|entry| (entry.uid.as_str(), entry.default))
                .collect::<Vec<_>>(),
            vec![("haxsd-byok:model-a", true), ("haxsd-byok:model-b", false)]
        );
    }

    #[test]
    fn native_prefix_requires_the_separator() {
        assert!(is_native_model_uid(" haxsd-byok:model-a "));
        assert!(!is_native_model_uid("haxsd-byok"));
        assert!(!is_native_model_uid("other:haxsd-byok:model-a"));
    }

    #[test]
    fn native_parameters_offer_bounded_context_and_keep_the_default_identity() {
        let settings = settings(
            vec![binding("haxsd-byok:model-a", "hash-a", "Local A")],
            Some("haxsd-byok:model-a"),
        );
        let mut configured = model("hash-a", "Local A", Some(500_000));
        configured.reasoning_effort = Some("high".into());
        configured.max_completion_tokens = Some(64_000);
        let entries = native_entries(&settings, &[configured]).unwrap();
        assert!(entries.len() > 1);
        assert_eq!(entries.iter().filter(|entry| entry.default).count(), 1);
        assert!(entries
            .iter()
            .any(|entry| entry.default && entry.uid == "haxsd-byok:model-a"));
        assert!(entries
            .iter()
            .any(|entry| entry.uid == "haxsd-byok:model-a~low~128000"));
        assert!(entries.iter().all(|entry| entry
            .context_window_tokens
            .is_some_and(|tokens| tokens <= 500_000)));
        assert!(!entries.iter().any(|entry| entry.uid.ends_with("~64000")));
    }
}
