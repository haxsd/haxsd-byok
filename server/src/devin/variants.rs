use crate::model::{ModelConfig, ModelType};

use super::directory::is_native_model_uid;

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const CONTEXTS: [u64; 4] = [64_000, 128_000, 256_000, 512_000];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeModelVariant {
    pub uid: String,
    pub model_id: String,
    pub family_uid: String,
    pub reasoning_effort: Option<String>,
    pub context_window_tokens: Option<u64>,
}

impl NativeModelVariant {
    pub fn effort_name(&self) -> &str {
        match self.reasoning_effort.as_deref() {
            None => "Provider default",
            Some("low") => "Low",
            Some("medium") => "Medium",
            Some("high") => "High",
            Some("xhigh") => "Extra High",
            Some("max") => "Max",
            Some(value) => value,
        }
    }

    pub fn effort_order(&self) -> u64 {
        self.reasoning_effort
            .as_deref()
            .map(|effort| {
                EFFORTS
                    .iter()
                    .position(|value| *value == effort)
                    .map_or(6, |index| index + 1) as u64
            })
            .unwrap_or_default()
    }

    pub fn context_order(&self) -> u64 {
        let context = self.context_window_tokens.unwrap_or_default();
        CONTEXTS.iter().filter(|value| **value < context).count() as u64
    }

    pub fn context_name(&self) -> String {
        match self.context_window_tokens {
            Some(tokens) if tokens % 1_000 == 0 => format!("{}K", tokens / 1_000),
            Some(tokens) => tokens.to_string(),
            None => "Model library default".into(),
        }
    }
}

pub fn model_variants(
    uid: &str,
    model: &ModelConfig,
    binding_context: Option<u64>,
) -> Vec<NativeModelVariant> {
    let uid = uid.trim();
    if !is_native_model_uid(uid) {
        return Vec::new();
    }
    let configured_effort = match model.model_type {
        ModelType::OpenAi => model.reasoning_effort.clone(),
        ModelType::Anthropic => model.anthropic_thinking_effort.clone(),
    };
    let configured_context = model
        .context_window_tokens
        .or(binding_context)
        .filter(|value| *value > 0);
    let mut efforts = Vec::new();
    if model.model_type == ModelType::OpenAi || configured_effort.is_none() {
        efforts.push(None);
    }
    efforts.extend(EFFORTS.into_iter().map(|value| Some(value.to_owned())));
    if !efforts.contains(&configured_effort) {
        efforts.push(configured_effort.clone());
    }
    let mut contexts = Vec::new();
    if let Some(limit) = configured_context {
        let reserved = model
            .max_output_tokens()
            .unwrap_or(4_096)
            .saturating_add(4_096);
        contexts.extend(
            CONTEXTS
                .into_iter()
                .filter(|value| *value < limit && *value > reserved)
                .map(Some),
        );
    }
    contexts.push(configured_context);
    let mut variants = vec![NativeModelVariant {
        uid: uid.into(),
        model_id: model.model_id.clone(),
        family_uid: uid.into(),
        reasoning_effort: configured_effort.clone(),
        context_window_tokens: configured_context,
    }];
    for effort in &efforts {
        for context in &contexts {
            if *effort == configured_effort && *context == configured_context {
                continue;
            }
            let effort_tag = match effort.as_deref() {
                Some(value) if EFFORTS.contains(&value) => value,
                Some(_) => "configured",
                None => "default",
            };
            let context_tag = context.map_or_else(|| "default".into(), |value| value.to_string());
            variants.push(NativeModelVariant {
                uid: format!("{uid}~{effort_tag}~{context_tag}"),
                model_id: model.model_id.clone(),
                family_uid: uid.into(),
                reasoning_effort: effort.clone(),
                context_window_tokens: *context,
            });
        }
    }
    variants
}

pub fn base_model_uid(uid: &str) -> &str {
    if !is_native_model_uid(uid) {
        return uid;
    }
    let Some((prefix, context)) = uid.rsplit_once('~') else {
        return uid;
    };
    let Some((base, effort)) = prefix.rsplit_once('~') else {
        return uid;
    };
    if !is_native_model_uid(base)
        || !(EFFORTS.contains(&effort) || matches!(effort, "default" | "configured"))
        || !(context == "default"
            || context
                .parse::<u64>()
                .is_ok_and(|value| value > 0 && value.to_string() == context))
    {
        return uid;
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_identity_is_canonical_and_only_applies_to_local_models() {
        assert_eq!(base_model_uid("haxsd-byok:a~low~128000"), "haxsd-byok:a");
        for uid in [
            "official~low~128000",
            "haxsd-byok:a~low~0",
            "haxsd-byok:a~low~0128000",
            "haxsd-byok:a~unknown~128000",
            "haxsd-byok:a~low~18446744073709551616",
        ] {
            assert_eq!(base_model_uid(uid), uid);
        }
    }
}
