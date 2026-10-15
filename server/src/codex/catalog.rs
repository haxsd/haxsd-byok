//! 固定 Codex ModelInfo 契约；使用官方基础指令，模型能力来自实际配置。
use crate::{model::ModelConfig, Error, Result};
use serde_json::{json, Value};

pub(crate) fn routing_fingerprint(model: &ModelConfig) -> Result<String> {
    use sha2::{Digest, Sha256};
    let routing = json!({"model_hash":model.model_hash,"protocol":model.provider_type(),"url":model.request_url()?,
        "model":model.model_id,"credential":model.api_key,"headers":if model.custom_headers_enabled {model.custom_headers.clone()} else {json!({})},
        "params":model.extra_params(),"context":model.context_window_tokens,"max_output":model.max_output_tokens(),
        "reasoning":model.reasoning_effort,"anthropic_effort":model.anthropic_thinking_effort,"thinking_budget":model.thinking_budget_tokens});
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&routing)?)))
}

pub fn build_catalog(models: &[ModelConfig]) -> Result<Value> {
    let mut catalog = Vec::new();
    for (index, model) in models.iter().enumerate() {
        let context = model
            .context_window_tokens
            .filter(|n| *n >= 4096 && *n <= i64::MAX as u64)
            .ok_or_else(|| {
                Error::Config(format!(
                    "configure the actual context window for {}",
                    model.display_name
                ))
            })?;
        // 本地执行工具和补丁继续使用 Codex；不宣告服务商托管搜索或实时传输。
        catalog.push(json!({
            "haxsd_byok_routing_fingerprint":routing_fingerprint(model)?,
            "slug":model.model_hash,"display_name":model.display_name,"description":null,
            "default_reasoning_level":null,"supported_reasoning_levels":[],
            "shell_type":"shell_command","visibility":"list","supported_in_api":true,"priority":index,
            "availability_nux":null,"upgrade":null,
            "model_messages":{"instructions_template":include_str!("../../prompt/codex/base.md")},
            "include_skills_usage_instructions":false,"include_apps_usage_instructions":false,"include_plugin_usage_instructions":false,
            "supports_reasoning_summary_parameter":false,"default_reasoning_summary":"none",
            "support_verbosity":false,"default_verbosity":null,"apply_patch_tool_type":"freeform",
            "web_search_tool_type":"text","truncation_policy":{"mode":"tokens","limit":10000},
            "context_window":context,"max_context_window":context,"auto_compact_token_limit":context*4/5,
            "effective_context_window_percent":90,"experimental_supported_tools":[],"input_modalities":["text","image"],
            "supports_search_tool":false,"supports_experimental_context":false,"use_responses_lite":false,
            "prefer_websockets":false,"supports_parallel_tool_calls":true
        }));
    }
    Ok(json!({"models":catalog}))
}
