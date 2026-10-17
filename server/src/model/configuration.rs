//! Defines model and provider configuration.
use std::{fmt, str::FromStr};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const OPENAI_RESPONSES_ENDPOINT: &str = "/v1/responses";
pub const OPENAI_CHAT_ENDPOINT: &str = "/v1/chat/completions";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ProviderType {
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic")]
    Anthropic,
    /// 插件执行的调用;协议细节在插件内部,核心只按统一事件流记录。
    #[serde(rename = "plugin")]
    Plugin,
}

impl ProviderType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::Anthropic => "anthropic",
            Self::Plugin => "plugin",
        }
    }
}

impl fmt::Display for ProviderType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProviderType {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "openai-chat" => Ok(Self::OpenAiChat),
            "openai-responses" => Ok(Self::OpenAiResponses),
            "anthropic" => Ok(Self::Anthropic),
            "plugin" => Ok(Self::Plugin),
            _ => Err(Error::Config(format!("unsupported provider type: {value}"))),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelType {
    OpenAi,
    Anthropic,
}

impl ModelType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

impl FromStr for ModelType {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "openai" => Ok(Self::OpenAi),
            "anthropic" => Ok(Self::Anthropic),
            _ => Err(Error::Config(format!("unsupported model type: {value}"))),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ModelConfigInput {
    #[serde(default)]
    pub sort_order: i64,
    pub display_name: String,
    /// 供应商分组的自定义显示名;同一 base_url 主机下的模型共享。
    #[serde(default)]
    pub group_name: Option<String>,
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub base_url: String,
    #[serde(default)]
    pub use_full_url: bool,
    pub api_key: String,
    pub tooltip_data: String,
    pub model_id: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub openai_endpoint: String,
    #[serde(default)]
    pub openai_extra_params_enabled: bool,
    #[serde(default = "empty_object")]
    pub openai_extra_params: serde_json::Value,
    #[serde(default)]
    pub custom_headers_enabled: bool,
    #[serde(default = "empty_object")]
    pub custom_headers: serde_json::Value,
    #[serde(default)]
    pub anthropic_extra_params_enabled: bool,
    #[serde(default = "empty_object")]
    pub anthropic_extra_params: serde_json::Value,
    pub context_window_tokens: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    pub anthropic_max_tokens: Option<u64>,
    #[serde(default)]
    pub anthropic_thinking_effort: Option<String>,
    pub thinking_budget_tokens: Option<u64>,
}

/// 凭据在 `Debug` 里只以"配没配"出现。
///
/// 两个配置结构体都持有明文 API Key，而 `Debug` 会出现在日志、`tracing` 字段、
/// 断言失败与 `expect` 的文案里——派生 Debug 等于给密钥开了一条谁都没注意的出口。
/// 序列化那侧早已脱敏（`ModelConfig::api_key` 标了 `skip_serializing`），Debug 跟上。
const MASKED_API_KEY: &str = "***";

fn masked_api_key(api_key: &str) -> &'static str {
    if api_key.is_empty() {
        ""
    } else {
        MASKED_API_KEY
    }
}

impl std::fmt::Debug for ModelConfigInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelConfigInput")
            .field("display_name", &self.display_name)
            .field("model_type", &self.model_type)
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("api_key", &masked_api_key(&self.api_key))
            .field("api_key_configured", &!self.api_key.is_empty())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Serialize)]
pub struct ModelConfig {
    pub model_hash: String,
    pub sort_order: i64,
    pub display_name: String,
    pub group_name: Option<String>,
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub base_url: String,
    pub use_full_url: bool,
    /// 凭据只留在服务端：任何序列化（含管理接口）都不会带出明文，接口用
    /// `api_key_configured` 说明是否已配置。需要凭据本身的动作——连通性测试、
    /// 获取模型列表、复制模型——都由服务端读回这一列完成。
    #[serde(skip_serializing)]
    pub api_key: String,
    pub tooltip_data: String,
    pub model_id: String,
    pub reasoning_effort: Option<String>,
    pub openai_endpoint: String,
    pub openai_extra_params_enabled: bool,
    pub openai_extra_params: serde_json::Value,
    pub custom_headers_enabled: bool,
    pub custom_headers: serde_json::Value,
    pub anthropic_extra_params_enabled: bool,
    pub anthropic_extra_params: serde_json::Value,
    pub context_window_tokens: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    pub anthropic_max_tokens: Option<u64>,
    pub anthropic_thinking_effort: Option<String>,
    pub thinking_budget_tokens: Option<u64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl std::fmt::Debug for ModelConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelConfig")
            .field("model_hash", &self.model_hash)
            .field("display_name", &self.display_name)
            .field("model_type", &self.model_type)
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("api_key", &masked_api_key(&self.api_key))
            .field("api_key_configured", &!self.api_key.is_empty())
            .finish_non_exhaustive()
    }
}

impl ModelConfig {
    /// 以这份配置为模板生成一份可写入的输入：只换显示名与排序，其余字段（含凭据）原样沿用。
    ///
    /// 两个用途：复制模型（凭据不随接口回传前端，复制只能在服务端做，见
    /// `Store::duplicate_model`），以及还原一条已保存配置的内容指纹（去重只看
    /// 内容，见 `model_fingerprint`）。
    pub fn config_input(&self, display_name: String, sort_order: i64) -> ModelConfigInput {
        ModelConfigInput {
            sort_order,
            display_name,
            group_name: self.group_name.clone(),
            model_type: self.model_type,
            base_url: self.base_url.clone(),
            use_full_url: self.use_full_url,
            api_key: self.api_key.clone(),
            tooltip_data: self.tooltip_data.clone(),
            model_id: self.model_id.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            openai_endpoint: self.openai_endpoint.clone(),
            openai_extra_params_enabled: self.openai_extra_params_enabled,
            openai_extra_params: self.openai_extra_params.clone(),
            custom_headers_enabled: self.custom_headers_enabled,
            custom_headers: self.custom_headers.clone(),
            anthropic_extra_params_enabled: self.anthropic_extra_params_enabled,
            anthropic_extra_params: self.anthropic_extra_params.clone(),
            context_window_tokens: self.context_window_tokens,
            max_completion_tokens: self.max_completion_tokens,
            anthropic_max_tokens: self.anthropic_max_tokens,
            anthropic_thinking_effort: self.anthropic_thinking_effort.clone(),
            thinking_budget_tokens: self.thinking_budget_tokens,
        }
    }

    pub fn provider_type(&self) -> ProviderType {
        match self.model_type {
            ModelType::Anthropic => ProviderType::Anthropic,
            ModelType::OpenAi if self.openai_endpoint == OPENAI_RESPONSES_ENDPOINT => {
                ProviderType::OpenAiResponses
            }
            ModelType::OpenAi => ProviderType::OpenAiChat,
        }
    }

    pub fn request_url(&self) -> Result<String> {
        resolve_request_url(
            self.model_type,
            &self.base_url,
            &self.openai_endpoint,
            self.use_full_url,
        )
    }

    pub fn max_output_tokens(&self) -> Option<u64> {
        match self.model_type {
            ModelType::OpenAi => self.max_completion_tokens,
            ModelType::Anthropic => self
                .anthropic_max_tokens
                .or(self.max_completion_tokens)
                // 用户留空是常态，而 provider 的兜底取的是"所有在役模型都接受的最小值"
                // （4096），拿它当 64k 级模型的上限等于静默截断长回答。已知模型在这里补上
                // 它自己的真实上限；表里没有的（自建网关、别名）继续走兜底。
                .or_else(|| known_anthropic_max_output_tokens(&self.model_id)),
        }
    }

    pub fn extra_params(&self) -> &serde_json::Value {
        match self.model_type {
            ModelType::OpenAi if self.openai_extra_params_enabled => &self.openai_extra_params,
            ModelType::Anthropic if self.anthropic_extra_params_enabled => {
                &self.anthropic_extra_params
            }
            _ => empty_object_ref(),
        }
    }

    pub fn configure(&self, model: &mut super::ModelSpec) {
        model.display_name = Some(self.display_name.clone());
        // A request-selected context is authoritative.  Use the saved model
        // value only when Cursor did not send a context parameter.
        if model.context_window_tokens.is_none() {
            model.context_window_tokens = self.context_window_tokens;
        }
        // 只有调用方**显式关闭**思考（`Some(false)`）时才不补强度。`None` 是"没表态"：
        // 普通 Cursor→Anthropic 请求就是这个形状，思考强度由模型配置给（默认 xhigh），
        // 不能把"没表态"当成"不要思考"（那会一次关掉所有 Anthropic 模型的思考）。
        if model.reasoning.enabled != Some(false) && model.reasoning.effort.is_none() {
            model.reasoning.effort = match self.model_type {
                ModelType::OpenAi => self.reasoning_effort.clone(),
                ModelType::Anthropic => self.anthropic_thinking_effort.clone(),
            };
        }
        if model.reasoning.enabled.is_none() {
            model.reasoning.enabled = Some(model.reasoning.effort.is_some());
        }
        if model.reasoning.thinking_budget_tokens.is_none() {
            model.reasoning.thinking_budget_tokens = self.thinking_budget_tokens;
        }
    }
}

/// 在役 Anthropic 模型（按 id 前缀）公开的输出上限。
///
/// 模型 id 带日期或小版本后缀（`claude-sonnet-4-20250514`、`claude-opus-4-1-20250805`），
/// 所以按前缀匹配；这些前缀互不为前缀，匹配唯一。
///
/// 这里**只**列真实上限高于 provider 兜底值（4096）的模型：等于兜底的型号（claude-3-opus、
/// claude-3-sonnet/haiku、claude-2.x）不必列，不列也能得到正确结果。
///
/// 判定逻辑在 `max_output_tokens` 里，公开给 provider 层的用例，让它能钉住"已知模型不被
/// 兜底压低 / 未知模型仍出合法请求"这两条。
pub(crate) fn known_anthropic_max_output_tokens(model_id: &str) -> Option<u64> {
    const LIMITS: [(&str, u64); 5] = [
        ("claude-sonnet-4", 64_000),
        ("claude-opus-4", 32_000),
        ("claude-3-7-sonnet", 64_000),
        ("claude-3-5-sonnet", 8_192),
        ("claude-3-5-haiku", 8_192),
    ];
    let id = model_id.trim().to_ascii_lowercase();
    LIMITS
        .iter()
        .find(|(prefix, _)| id.starts_with(prefix))
        .map(|(_, limit)| *limit)
}

pub fn normalize_model_input(input: &ModelConfigInput) -> Result<ModelConfigInput> {
    let display_name = required(&input.display_name, "model display name")?;
    let group_name = input
        .group_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);
    let base_url = normalize_request_url(&input.base_url)?;
    let api_key = required(&input.api_key, "model API key")?;
    let tooltip_data = required(&input.tooltip_data, "model tooltip")?;
    let model_id = required(&input.model_id, "model id")?;
    let reasoning_effort = normalize_effort(input.reasoning_effort.as_deref(), true)?;
    let anthropic_thinking_effort = match input.model_type {
        ModelType::Anthropic => Some(
            normalize_effort(
                input.anthropic_thinking_effort.as_deref().or(Some("xhigh")),
                false,
            )?
            .expect("Anthropic effort has a default"),
        ),
        ModelType::OpenAi => None,
    };
    let openai_endpoint = match input.model_type {
        ModelType::OpenAi => normalize_openai_endpoint(&input.openai_endpoint)?,
        ModelType::Anthropic => String::new(),
    };
    validate_object(&input.openai_extra_params, "OpenAI extra params")?;
    validate_object(&input.anthropic_extra_params, "Anthropic extra params")?;
    validate_headers(&input.custom_headers)?;

    let normalized = ModelConfigInput {
        sort_order: input.sort_order.max(0),
        display_name,
        group_name,
        model_type: input.model_type,
        base_url,
        use_full_url: input.use_full_url,
        api_key,
        tooltip_data,
        model_id,
        reasoning_effort: (input.model_type == ModelType::OpenAi)
            .then_some(reasoning_effort)
            .flatten(),
        openai_endpoint,
        openai_extra_params_enabled: input.model_type == ModelType::OpenAi
            && input.openai_extra_params_enabled,
        openai_extra_params: if input.model_type == ModelType::OpenAi {
            input.openai_extra_params.clone()
        } else {
            empty_object()
        },
        custom_headers_enabled: input.custom_headers_enabled,
        custom_headers: input.custom_headers.clone(),
        anthropic_extra_params_enabled: input.model_type == ModelType::Anthropic
            && input.anthropic_extra_params_enabled,
        anthropic_extra_params: if input.model_type == ModelType::Anthropic {
            input.anthropic_extra_params.clone()
        } else {
            empty_object()
        },
        context_window_tokens: positive(input.context_window_tokens, "context window")?,
        max_completion_tokens: positive(input.max_completion_tokens, "max completion tokens")?,
        anthropic_max_tokens: positive(input.anthropic_max_tokens, "Anthropic max tokens")?,
        anthropic_thinking_effort,
        thinking_budget_tokens: positive(input.thinking_budget_tokens, "thinking budget")?,
    };
    resolve_request_url(
        normalized.model_type,
        &normalized.base_url,
        &normalized.openai_endpoint,
        normalized.use_full_url,
    )?;
    Ok(normalized)
}

/// 配置内容指纹：按配置内容算出，用于判断"同样的配置是否已经存在"（创建去重、旧配置导入跳过）。
///
/// 它**不是**模型身份。模型身份保存在 `model_configs.model_hash`，在创建时分配、
/// 之后不随配置变化，Devin 绑定、提交消息设置与历史调用都按身份引用模型。
/// 身份与指纹分开的原因：改了显示名或轮换凭据之后，已保存的引用必须继续有效，
/// 而同一条"指纹曾是某模型身份"的配置要能作为新模型再建一份。
pub fn model_fingerprint(input: &ModelConfigInput) -> Result<String> {
    let normalized = normalize_model_input(input)?;
    let request_url = resolve_request_url(
        normalized.model_type,
        &normalized.base_url,
        &normalized.openai_endpoint,
        normalized.use_full_url,
    )?;
    let mut parts = vec![
        request_url,
        normalized.model_id,
        normalized.api_key,
        normalized.display_name,
    ];
    if normalized.model_type == ModelType::OpenAi {
        parts.push(normalized.openai_endpoint);
    }
    let digest = Sha256::digest(parts.join("\n").as_bytes());
    Ok(hex::encode(&digest[..8]))
}

pub fn normalize_request_url(value: &str) -> Result<String> {
    let value = value.trim();
    let url = Url::parse(value)
        .map_err(|error| Error::Config(format!("invalid model request URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::Config(
            "model request URL must be an HTTP(S) URL with a host".into(),
        ));
    }
    if url.fragment().is_some() {
        return Err(Error::Config(
            "model request URL cannot contain a fragment".into(),
        ));
    }
    Ok(value.into())
}

pub fn resolve_request_url(
    model_type: ModelType,
    base_url: &str,
    openai_endpoint: &str,
    use_full_url: bool,
) -> Result<String> {
    let base_url = normalize_request_url(base_url)?;
    let endpoint = match model_type {
        ModelType::OpenAi => normalize_openai_endpoint(openai_endpoint)?,
        ModelType::Anthropic => "/v1/messages".into(),
    };
    if use_full_url {
        return Ok(base_url);
    }
    append_standard_endpoint(&base_url, &endpoint)
}

fn append_standard_endpoint(base_url: &str, endpoint: &str) -> Result<String> {
    let mut url = Url::parse(base_url)
        .map_err(|error| Error::Config(format!("invalid model server URL: {error}")))?;
    let base_path = url.path().trim_end_matches('/').to_string();
    let endpoint = if has_trailing_version(&base_path) {
        endpoint.strip_prefix("/v1").unwrap_or(endpoint)
    } else {
        endpoint
    };
    url.set_path(&format!("{base_path}{endpoint}"));
    normalize_request_url(url.as_str())
}

fn has_trailing_version(path: &str) -> bool {
    let Some(segment) = path.rsplit('/').next() else {
        return false;
    };
    segment.strip_prefix('v').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

pub fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "x-api-key" | "api-key" | "cookie" | "set-cookie"
    )
}

fn normalize_openai_endpoint(value: &str) -> Result<String> {
    match value.trim() {
        "" | OPENAI_RESPONSES_ENDPOINT => Ok(OPENAI_RESPONSES_ENDPOINT.into()),
        OPENAI_CHAT_ENDPOINT => Ok(OPENAI_CHAT_ENDPOINT.into()),
        value => Err(Error::Config(format!(
            "unsupported OpenAI endpoint: {value}"
        ))),
    }
}

fn normalize_effort(value: Option<&str>, allow_empty: bool) -> Result<Option<String>> {
    let value = value.unwrap_or_default().trim().to_ascii_lowercase();
    if value.is_empty() && allow_empty {
        return Ok(None);
    }
    if matches!(value.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
        Ok(Some(value))
    } else {
        Err(Error::Config(format!(
            "unsupported reasoning effort: {value}"
        )))
    }
}

fn positive(value: Option<u64>, label: &str) -> Result<Option<u64>> {
    match value {
        Some(0) => Err(Error::Config(format!("{label} must be greater than zero"))),
        value => Ok(value),
    }
}

fn required(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        Err(Error::Config(format!("{label} cannot be empty")))
    } else {
        Ok(value.into())
    }
}

fn validate_object(value: &serde_json::Value, label: &str) -> Result<()> {
    if value.is_object() {
        Ok(())
    } else {
        Err(Error::Config(format!("{label} must be a JSON object")))
    }
}

fn validate_headers(value: &serde_json::Value) -> Result<()> {
    validate_object(value, "custom headers")?;
    for (name, value) in value.as_object().expect("validated object") {
        if name.trim().is_empty() || !value.is_string() {
            return Err(Error::Config(
                "custom headers must have non-empty names and string values".into(),
            ));
        }
    }
    Ok(())
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}

fn empty_object_ref() -> &'static serde_json::Value {
    static EMPTY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    EMPTY.get_or_init(empty_object)
}

/// 一轮请求的思考（reasoning / extended thinking）配置。
///
/// `enabled` 是三态：`None` 表示调用方没表态，由 `ModelConfig::configure` 按模型配置兜底；
/// `Some(false)` 是**显式关闭**。摘要/压缩这类调用必须能表达"这一轮不要思考"——以前只有
/// 一个布尔，`false` 同时背着"没要求"和"不要"两种意思，`configure` 于是把摘要调用的关闭
/// 重新打开（Anthropic 默认 xhigh），8K 摘要预算被思考吃掉。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningSpec {
    pub enabled: Option<bool>,
    pub effort: Option<String>,
    /// 思考预算（界面上的"思考预算 Token"）。有值时 Anthropic 用
    /// `{"type":"enabled","budget_tokens":N}` 形态，留空走 adaptive。
    pub thinking_budget_tokens: Option<u64>,
}

impl ReasoningSpec {
    /// 是否要开思考。只认显式的 `Some(true)`：`None` 是"调用方没表态"，由
    /// `ModelConfig::configure` 决定，适配器不替它猜。
    pub fn is_enabled(&self) -> bool {
        self.enabled == Some(true)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelLatency {
    #[default]
    Standard,
    Fast,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelSpec {
    pub model_id: String,
    pub display_name: Option<String>,
    pub reasoning: ReasoningSpec,
    pub latency: ModelLatency,
    pub max_output_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
    #[serde(default)]
    pub supports_image_generation: bool,
    #[serde(default)]
    pub extra_params: serde_json::Value,
}

impl ModelSpec {
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            display_name: None,
            reasoning: ReasoningSpec::default(),
            latency: ModelLatency::Standard,
            max_output_tokens: None,
            context_window_tokens: None,
            supports_image_generation: false,
            extra_params: serde_json::json!({}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的 Anthropic 模型配置：只让思考相关字段有值。
    fn anthropic_config(effort: Option<&str>, budget: Option<u64>) -> ModelConfig {
        ModelConfig {
            model_hash: "hash".into(),
            sort_order: 0,
            display_name: "Claude".into(),
            group_name: None,
            model_type: ModelType::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            use_full_url: false,
            api_key: "key".into(),
            tooltip_data: String::new(),
            model_id: "claude-sonnet-4".into(),
            reasoning_effort: None,
            openai_endpoint: String::new(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: effort.map(String::from),
            thinking_budget_tokens: budget,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    /// 普通 Cursor→Anthropic 请求：调用方没表态（`None`/`None`），思考强度照旧由模型配置给。
    /// 这条是 P2-1 修复的护栏——不能在"让关闭可表达"时把没表态的请求一起关掉。
    #[test]
    fn a_request_that_did_not_ask_for_reasoning_still_gets_the_configured_effort() {
        let config = anthropic_config(Some("xhigh"), None);
        let mut model = ModelSpec::new("claude-sonnet-4");

        config.configure(&mut model);

        assert!(model.reasoning.is_enabled(), "没表态的请求照旧开思考");
        assert_eq!(model.reasoning.effort.as_deref(), Some("xhigh"));
    }

    /// 摘要调用显式关掉思考后，`configure` 不得再按模型配置把它打开。
    /// 以前 `enabled: false` 同时表示"没要求"，于是每次自动压缩都白付一份思考开销。
    #[test]
    fn an_explicitly_disabled_request_survives_configure() {
        let config = anthropic_config(Some("xhigh"), None);
        let mut model = ModelSpec::new("claude-sonnet-4");
        model.reasoning.enabled = Some(false);
        model.max_output_tokens = Some(8_192);

        config.configure(&mut model);

        assert!(!model.reasoning.is_enabled(), "显式关闭必须活过 configure");
        assert_eq!(model.reasoning.effort, None, "关闭时不该补出思考强度");
        assert_eq!(model.max_output_tokens, Some(8_192), "输出预算不受影响");
    }

    /// "要思考但没给强度"仍补模型配置的强度，否则会退化成上游默认强度。
    #[test]
    fn a_reasoning_request_without_an_effort_gets_the_configured_one() {
        let config = anthropic_config(Some("medium"), None);
        let mut model = ModelSpec::new("claude-sonnet-4");
        model.reasoning.enabled = Some(true);

        config.configure(&mut model);

        assert_eq!(model.reasoning.effort.as_deref(), Some("medium"));
    }

    /// OpenAI 模型没配"推理强度"时不开思考：以前 `enabled |= effort.is_some()` 就是这个效果。
    #[test]
    fn an_openai_model_without_a_configured_effort_does_not_reason() {
        let mut config = anthropic_config(None, None);
        config.model_type = ModelType::OpenAi;
        let mut model = ModelSpec::new("gpt-5");

        config.configure(&mut model);

        assert!(!model.reasoning.is_enabled());
        assert_eq!(model.reasoning.effort, None);
    }

    /// 界面上的"思考预算 Token"要经 `configure` 传进 `ModelSpec`，否则适配器读不到它
    /// （Cursor/Devin 路径上那个字段一直是"设了没反应"）。
    #[test]
    fn configure_carries_the_thinking_budget_into_the_spec() {
        let config = anthropic_config(Some("xhigh"), Some(4_096));
        let mut model = ModelSpec::new("claude-sonnet-4");

        config.configure(&mut model);

        assert_eq!(model.reasoning.thinking_budget_tokens, Some(4_096));
    }

    /// 桌面端把"最大输出 Token"留空是常态。那时必须用该模型自己的真实上限，而不是留给
    /// provider 的兜底：兜底取的是所有在役模型都接受的最小值（4096），对 64k 级模型就是
    /// 静默截断长回答（旧值 65000 只在输出上限更低的模型上 400，不会截断）。
    #[test]
    fn a_blank_limit_falls_back_to_the_models_real_output_cap() {
        for (model_id, expected) in [
            ("claude-sonnet-4", 64_000),
            ("claude-sonnet-4-20250514", 64_000),
            ("claude-opus-4-1-20250805", 32_000),
            ("claude-3-7-sonnet-20250219", 64_000),
            ("claude-3-5-sonnet-20241022", 8_192),
            ("claude-3-5-haiku-20241022", 8_192),
        ] {
            let mut config = anthropic_config(Some("xhigh"), None);
            config.model_id = model_id.into();

            assert_eq!(
                config.max_output_tokens(),
                Some(expected),
                "{model_id} 留空时要用它自己的上限"
            );
        }
    }

    /// 用户填了就一定用它（两种字段都算），模型表只补空缺。
    #[test]
    fn a_configured_limit_wins_over_the_model_table() {
        let mut config = anthropic_config(Some("xhigh"), None);
        config.anthropic_max_tokens = Some(2_000);
        assert_eq!(config.max_output_tokens(), Some(2_000));

        config.anthropic_max_tokens = None;
        config.max_completion_tokens = Some(7_000);
        assert_eq!(config.max_output_tokens(), Some(7_000));
    }

    /// 表里没有的模型不猜：仍然返回 `None`，由 provider 用安全兜底（4096）出请求——
    /// 猜大了会 400，猜小了会截断，两者都不如"按最小公共上限发一次能成功的请求"。
    #[test]
    fn an_unknown_anthropic_model_keeps_the_provider_fallback() {
        let mut config = anthropic_config(None, None);
        config.model_id = "my-gateway/claude-alias".into();
        assert_eq!(config.max_output_tokens(), None);

        // 大小写与空白不影响已知模型的判定。
        config.model_id = "  Claude-Sonnet-4-20250514  ".into();
        assert_eq!(config.max_output_tokens(), Some(64_000));
    }

    /// OpenAI 那条路不发 `max_tokens`（缺省由上游决定），所以不给它套 Anthropic 的模型表。
    #[test]
    fn an_openai_model_does_not_get_an_anthropic_limit() {
        let mut config = anthropic_config(None, None);
        config.model_type = ModelType::OpenAi;
        config.model_id = "claude-sonnet-4".into();

        assert_eq!(config.max_output_tokens(), None);
    }

    /// `Debug` 不能把明文 API Key 带出去：它会出现在日志、`tracing` 字段与断言文案里，
    /// 而这两个结构体都持有凭据（序列化那侧早就脱敏了）。
    #[test]
    fn debug_output_masks_the_api_key() {
        let input: ModelConfigInput = serde_json::from_value(serde_json::json!({
            "display_name": "Test",
            "type": "openai",
            "base_url": "https://example.com/v1",
            "api_key": "sk-live-super-secret",
            "tooltip_data": "Tooltip",
            "model_id": "test-model",
        }))
        .unwrap();
        let rendered = format!("{input:?}");
        assert!(
            !rendered.contains("sk-live-super-secret"),
            "ModelConfigInput 的 Debug 不能带出明文凭据: {rendered}"
        );
        assert!(
            rendered.contains(MASKED_API_KEY),
            "掩码要能看出凭据存在: {rendered}"
        );

        let mut config = anthropic_config(None, None);
        config.api_key = "sk-live-super-secret".into();
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("sk-live-super-secret"),
            "ModelConfig 的 Debug 不能带出明文凭据: {rendered}"
        );
        assert!(
            rendered.contains(MASKED_API_KEY),
            "掩码要能看出凭据存在: {rendered}"
        );

        // 没配凭据时说清"没配"，不要给它一个看起来像密钥的掩码。
        let mut empty = anthropic_config(None, None);
        empty.api_key = String::new();
        assert!(format!("{empty:?}").contains("api_key_configured: false"));
    }
}
