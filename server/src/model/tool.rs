//! Defines Tool calls, results, and Tool round identities.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ProviderReplayState;

pub fn normalize_tool_name(name: &str) -> String {
    let normalized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if normalized.is_empty() {
        "_".into()
    } else {
        normalized
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComputerUseToolConfig {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub display_width_px: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub display_height_px: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub display_number: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// 官方工具定义里不能塞进 name/description/parameters 的扩展字段。
///
/// Devin 的 `ChatToolDefinition` 在 schema 之外还带 strict、MCP server 名称、
/// 自定义工具 grammar、只读提示等。共享层保存它们，provider 只按自己的协议
/// 选择支持的字段转发；不支持的字段不能猜成别的含义。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attribution_field_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_use_config: Option<ComputerUseToolConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_custom_tool: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_tool_grammar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_tool_grammar_syntax: Option<String>,
    /// Anthropic 工具级缓存断点；只有 Anthropic 协议使用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strict: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ToolMetadata {
    pub fn is_empty(&self) -> bool {
        self.attribution_field_names.is_empty()
            && self.server_name.is_none()
            && self.read_only_hint.is_none()
            && self.computer_use_config.is_none()
            && self.is_custom_tool.is_none()
            && self.custom_tool_grammar.is_none()
            && self.custom_tool_grammar_syntax.is_none()
            && self.cache_control.is_none()
            && !self.strict
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "ToolMetadata::is_empty")]
    pub metadata: ToolMetadata,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub index: usize,
    pub call_id: String,
    pub model_call_id: String,
    pub name: String,
    pub arguments_text: String,
    pub arguments: Value,
    pub argument_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
    pub image: Option<ToolImageReference>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolImageReference {
    pub blob_id: String,
    pub mime_type: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolRoundAssistant {
    pub text: String,
    pub thinking: String,
    pub model_call_id: String,
    pub replay_state: Option<ProviderReplayState>,
}
