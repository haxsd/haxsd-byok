//! 固定 Codex ModelInfo 契约；使用官方基础指令，模型能力来自实际配置。
use crate::{
    model::{ModelConfig, ProviderType},
    Error, Result,
};
use serde_json::{json, Value};

/// 客户端思考强度选择器里的档位：与上游转发口径一致，只有这些字面量才会被
/// `upstream::normalize_reasoning` 原样转给上游。
const REASONING_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// 上下文窗口的合理上限：比当前最大的商用模型高一个数量级，同时保证目录里的
/// `context * 4 / 5` 不会溢出。
pub(crate) const MAX_CONTEXT_WINDOW: u64 = 10_000_000;

/// 官方基础指令之外的那些 `model_messages` 段落（持久模式、协作模式、多代理、审批、
/// 自动复核、权限、Token 预算、确认策略）。
///
/// 客户端的这些功能是"目录里给了说明才出现"：以前我们只宣告 `instructions_template`，
/// 于是持久模式、多代理、计划协作这些都看不见了。文本抄自同一版本的官方模型目录
/// （Apache-2.0，署名与许可见 `server/prompt/codex/LICENSE`），只删掉 `persistent_instructions`
/// 末尾那句 OpenAI 内部的任务截止时间。
const MODEL_MESSAGES: &str = include_str!("../../prompt/codex/model_messages.json");

/// 解析一次就够：目录每次发布都要组装全部模型。
fn model_messages() -> Value {
    use std::sync::OnceLock;
    static PARSED: OnceLock<Value> = OnceLock::new();
    let sections = PARSED.get_or_init(|| {
        serde_json::from_str(MODEL_MESSAGES)
            .expect("the vendored model_messages asset is valid JSON")
    });
    let mut messages = sections.clone();
    if let Some(object) = messages.as_object_mut() {
        object.insert(
            "instructions_template".into(),
            Value::String(include_str!("../../prompt/codex/base.md").into()),
        );
    }
    messages
}

fn digest(value: &Value) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}

/// 整条路由配置的指纹：地址、凭据、参数、上下文/输出上限、思考强度都算在内。
///
/// 它只用来判断已发布的目录是否过期（`gateway::publish_current_catalog`），不是
/// 模型身份——改一次 API Key 也会让它变化，这是有意的。
pub(crate) fn routing_fingerprint(model: &ModelConfig) -> Result<String> {
    let routing = json!({"model_hash":model.model_hash,"protocol":model.provider_type(),"url":model.request_url()?,
        "model":model.model_id,"credential":model.api_key,"headers":if model.custom_headers_enabled {model.custom_headers.clone()} else {json!({})},
        "params":model.extra_params(),"context":model.context_window_tokens,"max_output":model.max_output_tokens(),
        "reasoning":model.reasoning_effort,"anthropic_effort":model.anthropic_thinking_effort,"thinking_budget":model.thinking_budget_tokens});
    digest(&routing)
}

/// 跨轮推理状态的归属身份：**上游 origin + 上游 model_id + 协议**。
///
/// 回放密文里记的是"这段推理状态是谁签发的"。判据与 `provider/openai_responses.rs` 的
/// `replay_owner` 同一语义：origin 取 scheme + 主机 + 端口——路径微调不该让历史推理失效，
/// 换主机（另一家同协议服务）则必须失效，因为密文是那台上游签发的，回放过去只会被它以
/// 签名/密文不合法拒掉整轮请求；上游的 model_id 也一样，换模型就是另一段推理状态。
///
/// 只认 `model_hash`（本应用给模型起的 ID）+ 协议是不够的：model_hash 不是上游身份，
/// 同一个 hash 指向不同上游、或两个上游共用同一个 hash 时，外来密文会被误判成本应用签发。
/// 至于凭据、上限、参数、自定义头、思考强度这些"只影响这次怎么发"的配置，仍然不进归属。
pub(crate) fn replay_owner(model: &ModelConfig) -> Result<String> {
    let request_url = model.request_url()?;
    let upstream = reqwest::Url::parse(&request_url)
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or(request_url);
    digest(&json!({
        "upstream": upstream,
        "model": model.model_id,
        "protocol": model.provider_type(),
    }))
}

/// 模型配置的思考强度：Anthropic 类型取 `anthropic_thinking_effort`（与
/// `upstream.rs` 的取值口径一致），其余取 `reasoning_effort`。只有落在宣告档位里
/// 的值才拿来宣告；没配（或配了别的值）就维持空档位，不替用户猜一组上游可能拒绝
/// 的值。
fn configured_reasoning_level(model: &ModelConfig) -> Option<&str> {
    let effort = if model.provider_type() == ProviderType::Anthropic {
        model.anthropic_thinking_effort.as_deref()
    } else {
        model.reasoning_effort.as_deref()
    }?;
    let effort = effort.trim();
    REASONING_LEVELS.contains(&effort).then_some(effort)
}

/// 引擎把这份说明显示在思考强度选择器里；只说这一档在做什么，不承诺效果。
fn reasoning_level_description(effort: &str) -> &'static str {
    match effort {
        "low" => "Fastest responses with the least reasoning",
        "medium" => "Balanced reasoning for everyday tasks",
        "high" => "Deeper reasoning for harder tasks",
        "xhigh" => "Extended reasoning for complex tasks",
        _ => "Maximum reasoning for the hardest tasks",
    }
}

pub fn build_catalog(models: &[ModelConfig]) -> Result<Value> {
    let mut catalog = Vec::new();
    for (index, model) in models.iter().enumerate() {
        let context = model
            .context_window_tokens
            .filter(|n| (4096..=MAX_CONTEXT_WINDOW).contains(n))
            .ok_or_else(|| {
                Error::Config(format!(
                    "configure the actual context window (4096..={MAX_CONTEXT_WINDOW} tokens) for {}",
                    model.display_name
                ))
            })?;
        let reasoning = configured_reasoning_level(model);
        let levels = if reasoning.is_some() {
            REASONING_LEVELS
                .iter()
                .map(|effort| {
                    json!({"effort": effort, "description": reasoning_level_description(effort)})
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        // 能力字段照官方模型的形状宣告：客户端"能用哪些功能"全看它们，少一个界面就少一块。
        //
        // `effective_context_window_percent` 是客户端**实际用来算输入预算**的比例：95 表示
        // 留 5% 余量（800k 的模型在界面里显示 760k）。它不再由 config.toml 的全局窗口
        // 覆盖——那份覆盖只按默认模型写一次，客户端换模型时不会更新。
        //
        // 仍然不宣告的、以及为什么不宣告：
        // - `supports_search_tool` / `web_search_tool_type`：托管搜索由模型服务商执行，我们
        //   的上游（Chat Completions / Anthropic / 第三方 Responses）没有这项能力。
        // - `use_responses_lite`：它改的是客户端发往**服务商**的请求形状，只对官方后端验证过。
        // - `tool_mode`: 官方用 `code_mode_only`，那会换一套工具调用形状；我们的上游是在标准
        //   工具循环上验证的，不跟着换。
        // - `prefer_websockets`：没有实时传输通道。
        // - `max_context_window`：等于 `context_window`，服务商的真实硬上限我们不知道。
        catalog.push(json!({
            "haxsd_byok_routing_fingerprint":routing_fingerprint(model)?,
            "slug":model.model_hash,"display_name":model.display_name,"description":null,
            "default_reasoning_level":reasoning,"supported_reasoning_levels":levels,
            "shell_type":"unified_exec","visibility":"list","supported_in_api":true,"priority":index,
            "availability_nux":null,"upgrade":null,"additional_speed_tiers":[],"service_tiers":[],
            "model_messages":model_messages(),
            "include_skills_usage_instructions":false,"include_apps_usage_instructions":false,"include_plugin_usage_instructions":false,
            "supports_reasoning_summary_parameter":false,"default_reasoning_summary":"none",
            "support_verbosity":true,"default_verbosity":"low","apply_patch_tool_type":"freeform",
            "web_search_tool_type":"text","truncation_policy":{"mode":"tokens","limit":10000},
            "context_window":context,"max_context_window":context,"auto_compact_token_limit":context*4/5,
            "effective_context_window_percent":95,
            "experimental_supported_tools":["send_user_message_async","clock"],
            "input_modalities":["text","image"],"supports_image_detail_original":true,
            "supports_search_tool":false,"supports_experimental_context":false,"use_responses_lite":false,
            "prefer_websockets":false,"supports_parallel_tool_calls":true,
            "supports_reasoning_effort_updates":true,"multi_agent_version":"v2"
        }));
    }
    Ok(json!({"models":catalog}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelType;

    fn model(kind: ModelType, reasoning: Option<&str>, anthropic: Option<&str>) -> ModelConfig {
        ModelConfig {
            model_hash: "hash".into(),
            sort_order: 0,
            display_name: "test model".into(),
            group_name: None,
            model_type: kind,
            base_url: "http://127.0.0.1:1".into(),
            use_full_url: true,
            api_key: "secret".into(),
            tooltip_data: String::new(),
            model_id: "vendor-model".into(),
            reasoning_effort: reasoning.map(String::from),
            openai_endpoint: "/v1/responses".into(),
            openai_extra_params_enabled: false,
            openai_extra_params: json!({}),
            custom_headers_enabled: false,
            custom_headers: json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: json!({}),
            context_window_tokens: Some(32768),
            max_completion_tokens: Some(4096),
            anthropic_max_tokens: Some(4096),
            anthropic_thinking_effort: anthropic.map(String::from),
            thinking_budget_tokens: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    /// 回放归属 = 上游 origin + 上游 model_id + 协议（复核 13-codex/P1-1）：
    /// 只影响"这次怎么发"的配置（凭据、上限、参数、自定义头、思考强度）不能让进行中
    /// 会话的旧推理状态失效；换上游主机、换上游模型 ID、换协议才是另一段推理状态。
    #[test]
    fn the_replay_owner_ignores_route_configuration_but_tracks_the_upstream_identity() {
        let base = model(ModelType::OpenAi, Some("high"), None);
        let owner = replay_owner(&base).unwrap();

        let mut edited = base.clone();
        edited.api_key = "rotated-secret".into();
        edited.max_completion_tokens = Some(8192);
        edited.context_window_tokens = Some(65_536);
        edited.openai_extra_params_enabled = true;
        edited.openai_extra_params = json!({"temperature": 0.2});
        edited.custom_headers_enabled = true;
        edited.custom_headers = json!({"x-tenant": "b"});
        edited.reasoning_effort = Some("xhigh".into());
        assert_eq!(
            replay_owner(&edited).unwrap(),
            owner,
            "换凭据/上限/参数/自定义头/思考强度不能让进行中会话的旧密文失效"
        );

        // origin 只认 scheme + 主机 + 端口：同一台服务换个路径不算换上游。
        let mut repathed = base.clone();
        repathed.base_url = "http://127.0.0.1:1/v1/another-endpoint".into();
        assert_eq!(
            replay_owner(&repathed).unwrap(),
            owner,
            "路径微调不是换上游"
        );

        // 反例（复核要求的判据）：不同 origin、同一个 model_hash——外来密文不能被当成
        // 本应用签发的，否则回放给另一台上游会被以签名不合法拒掉整轮。
        let mut other_upstream = base.clone();
        other_upstream.base_url = "http://127.0.0.1:9999".into();
        assert_ne!(
            replay_owner(&other_upstream).unwrap(),
            owner,
            "换上游主机必须让旧密文失效"
        );

        let mut other_vendor_model = base.clone();
        other_vendor_model.model_id = "another-vendor-id".into();
        assert_ne!(
            replay_owner(&other_vendor_model).unwrap(),
            owner,
            "换上游模型 ID 就是另一段推理状态"
        );

        let anthropic = model(ModelType::Anthropic, Some("high"), None);
        assert_ne!(
            replay_owner(&anthropic).unwrap(),
            owner,
            "Chat 文本与 Anthropic 签名块不能互相回放"
        );
    }

    /// 配置了强度才宣告档位：选择器里出现的正是这几个上游认识的字面量。
    #[test]
    fn a_configured_effort_declares_the_selectable_levels() {
        let catalog = build_catalog(&[model(ModelType::OpenAi, Some("high"), None)]).unwrap();
        assert_eq!(catalog["models"][0]["default_reasoning_level"], "high");
        let levels = catalog["models"][0]["supported_reasoning_levels"]
            .as_array()
            .unwrap();
        assert_eq!(levels.len(), REASONING_LEVELS.len());
        assert_eq!(levels[0]["effort"], "low");
        assert!(levels
            .iter()
            .all(|level| level["description"].as_str().is_some_and(|d| !d.is_empty())));
    }

    /// 没配强度（或配了档位外的值）就维持空档位：不替用户猜一组上游可能拒绝的值。
    #[test]
    fn an_unconfigured_effort_declares_nothing() {
        for (reasoning, anthropic) in [(None, None), (Some("none"), None), (Some("custom"), None)] {
            let catalog = build_catalog(&[model(ModelType::OpenAi, reasoning, anthropic)]).unwrap();
            assert_eq!(
                catalog["models"][0]["default_reasoning_level"],
                Value::Null,
                "{reasoning:?}"
            );
            assert_eq!(
                catalog["models"][0]["supported_reasoning_levels"],
                json!([]),
                "{reasoning:?}"
            );
        }
    }

    /// Anthropic 类型取 anthropic_thinking_effort，与上游的取值口径一致。
    #[test]
    fn anthropic_models_declare_their_thinking_effort() {
        let catalog =
            build_catalog(&[model(ModelType::Anthropic, Some("low"), Some("xhigh"))]).unwrap();
        assert_eq!(catalog["models"][0]["default_reasoning_level"], "xhigh");
        let catalog = build_catalog(&[model(ModelType::Anthropic, Some("low"), None)]).unwrap();
        assert_eq!(catalog["models"][0]["default_reasoning_level"], Value::Null);
    }

    /// 每个模型按自己的配置宣告窗口：800k 的模型不能被另一个模型的 286k 夹住。
    #[test]
    fn each_model_publishes_its_own_configured_window() {
        let mut large = model(ModelType::OpenAi, None, None);
        large.context_window_tokens = Some(800_000);
        let mut small = model(ModelType::OpenAi, None, None);
        small.model_hash = "small".into();
        small.context_window_tokens = Some(286_000);

        let catalog = build_catalog(&[large, small]).unwrap();
        assert_eq!(catalog["models"][0]["context_window"], 800_000);
        assert_eq!(catalog["models"][0]["max_context_window"], 800_000);
        assert_eq!(catalog["models"][0]["auto_compact_token_limit"], 640_000);
        assert_eq!(catalog["models"][1]["context_window"], 286_000);
        assert_eq!(catalog["models"][1]["max_context_window"], 286_000);
        assert_eq!(catalog["models"][1]["auto_compact_token_limit"], 228_800);
    }

    /// 离谱的窗口不猜也不截断：直接拒绝，避免目录里的 `* 4 / 5` 溢出。
    #[test]
    fn an_absurd_context_window_is_refused() {
        let mut model = model(ModelType::OpenAi, None, None);
        model.context_window_tokens = Some(MAX_CONTEXT_WINDOW + 1);
        assert!(build_catalog(&[model]).is_err());
    }

    /// 客户端的功能开关全在这些字段上：少一个，界面上就少一块（以前"用起来丢功能"的根因）。
    #[test]
    fn capability_fields_match_the_official_shape() {
        let catalog = build_catalog(&[model(ModelType::OpenAi, Some("high"), None)]).unwrap();
        let published = &catalog["models"][0];
        assert_eq!(published["shell_type"], "unified_exec");
        assert_eq!(published["support_verbosity"], true);
        assert_eq!(published["default_verbosity"], "low");
        assert_eq!(published["supports_image_detail_original"], true);
        assert_eq!(published["supports_reasoning_effort_updates"], true);
        assert_eq!(published["multi_agent_version"], "v2");
        assert_eq!(published["effective_context_window_percent"], 95);
        assert_eq!(
            published["experimental_supported_tools"],
            json!(["send_user_message_async", "clock"])
        );
        // 服务商托管的能力与改请求形状的开关仍然不宣告。
        assert_eq!(published["supports_search_tool"], false);
        assert_eq!(published["use_responses_lite"], false);
        assert!(published.get("tool_mode").is_none());
    }

    /// 持久模式、多代理这些功能靠 `model_messages` 里的段落才会出现。
    #[test]
    fn model_messages_carry_the_official_sections() {
        let catalog = build_catalog(&[model(ModelType::OpenAi, None, None)]).unwrap();
        let messages = &catalog["models"][0]["model_messages"];
        assert!(
            messages["instructions_template"]
                .as_str()
                .is_some_and(|text| text.contains("You are a coding agent")),
            "基础指令仍然是我们随包 vendored 的那份"
        );
        for section in [
            "persistent_instructions",
            "collaboration_modes",
            "multi_agent",
            "approvals",
            "auto_review",
            "confirmation_policies",
        ] {
            assert!(
                !messages[section].is_null(),
                "{section} 缺失时对应功能不会出现在界面里"
            );
        }
        // 官方那句 OpenAI 内部的任务截止时间不该带进来。
        assert!(!messages["persistent_instructions"]
            .as_str()
            .unwrap()
            .contains("task deadline"));
    }
}
