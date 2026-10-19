//! Estimates provider-visible context size and formats configured token counts.

use super::{ContentPart, ProjectedContent, ProjectedMessage, PromptSpec};

const TOKENS_PER_MESSAGE_OVERHEAD: u64 = 8;
const TOKENS_PER_TOOL_CALL_OVERHEAD: u64 = 6;
const TOKENS_PER_IMAGE: u64 = 1_024;

/// ASCII text density for English prose, code, and JSON: about four characters
/// per token on the tokenizers in use.
const ASCII_CHARS_PER_TOKEN: u64 = 4;

/// CJK text density. Provider tokenizers disagree here (roughly 0.4 tokens per
/// character for DeepSeek's vocabulary and up to 1 token per character for
/// Anthropic), so the base estimate takes a middle value and the compaction
/// path corrects the remainder from provider-reported usage
/// (`run::compaction::estimate_calibration`).
const CJK_CHARS_PER_TOKEN: u64 = 2;

/// Non-CJK, non-ASCII text (accented Latin, Cyrillic, emoji, ...): half the
/// character density of ASCII so mixed-script history is never silently cheap.
const OTHER_CHARS_PER_TOKEN: u64 = 2;

pub(crate) fn estimate_context_tokens(prompt: &PromptSpec, messages: &[ProjectedMessage]) -> u64 {
    let instructions = estimate_text_tokens(&prompt.instructions);
    let tools = prompt.tools.iter().fold(0_u64, |total, tool| {
        total.saturating_add(estimate_json_tokens(tool))
    });
    instructions
        .saturating_add(tools)
        .saturating_add(estimate_projected_messages_tokens(messages))
}

pub(crate) fn estimate_projected_messages_tokens(messages: &[ProjectedMessage]) -> u64 {
    messages.iter().fold(0_u64, |total, message| {
        total.saturating_add(estimate_message_tokens(message))
    })
}

fn estimate_message_tokens(message: &ProjectedMessage) -> u64 {
    let content = match &message.content {
        ProjectedContent::Parts(parts) => estimate_parts_tokens(parts),
        ProjectedContent::Assistant {
            text,
            thinking,
            replay_state: _,
            calls,
        } => {
            let calls = calls.iter().fold(0_u64, |total, call| {
                total
                    .saturating_add(TOKENS_PER_TOOL_CALL_OVERHEAD)
                    .saturating_add(estimate_text_tokens(&call.call_id))
                    .saturating_add(estimate_text_tokens(&call.name))
                    .saturating_add(estimate_json_tokens(&call.arguments))
            });
            estimate_text_tokens(text)
                .saturating_add(estimate_text_tokens(thinking))
                .saturating_add(calls)
        }
        ProjectedContent::ToolResult(result) => {
            let content = if result.provider_parts.is_empty() {
                estimate_text_tokens(&result.content).saturating_add(
                    result
                        .image
                        .as_ref()
                        .map(|image| {
                            TOKENS_PER_IMAGE.saturating_add(estimate_text_tokens(&image.mime_type))
                        })
                        .unwrap_or_default(),
                )
            } else {
                estimate_parts_tokens(&result.provider_parts)
            };
            estimate_text_tokens(&result.call_id)
                .saturating_add(estimate_text_tokens(&result.name))
                .saturating_add(content)
        }
    };
    TOKENS_PER_MESSAGE_OVERHEAD.saturating_add(content)
}

fn estimate_parts_tokens(parts: &[ContentPart]) -> u64 {
    parts.iter().fold(0_u64, |total, part| {
        let tokens = match part {
            ContentPart::Text { text } => estimate_text_tokens(text),
            ContentPart::Image { mime_type, data } => estimate_image_tokens(mime_type, data)
                .saturating_add(estimate_text_tokens(mime_type)),
        };
        total.saturating_add(tokens)
    })
}

/// Conservative image price for a provider-agnostic estimator.
///
/// Anthropic charges roughly one token per 750 pixels; the OpenAI high-detail
/// rule is 85 tokens plus 170 per 512-pixel tile. Taking the larger of the two
/// keeps mixed-provider history on the safe side of the window; a flat 1024
/// undercounted full-size screenshots by 2-3x. The fallback stays 1024 for
/// images whose bytes are unavailable (blob references) or whose header the
/// parser does not recognize.
fn estimate_image_tokens(mime_type: &str, data: &[u8]) -> u64 {
    let Some((width, height)) = image_dimensions(mime_type, data) else {
        return TOKENS_PER_IMAGE;
    };
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    let anthropic = pixels.div_ceil(750).max(200);
    let tiles = u64::from(width)
        .div_ceil(512)
        .saturating_mul(u64::from(height).div_ceil(512));
    let openai = 85_u64.saturating_add(170_u64.saturating_mul(tiles));
    anthropic.max(openai).min(20_000)
}

fn image_dimensions(mime_type: &str, data: &[u8]) -> Option<(u32, u32)> {
    let mime = mime_type.to_ascii_lowercase();
    if mime.contains("png") || data.starts_with(&[0x89, b'P', b'N', b'G']) {
        return png_dimensions(data);
    }
    if mime.contains("gif") || data.starts_with(b"GIF") {
        return gif_dimensions(data);
    }
    if mime.contains("jpeg") || mime.contains("jpg") || data.starts_with(&[0xFF, 0xD8]) {
        return jpeg_dimensions(data);
    }
    None
}

fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // 8-byte signature, 4-byte chunk length, "IHDR", then width/height.
    if data.len() < 24 || &data[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

fn gif_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 10 {
        return None;
    }
    let width = u32::from(u16::from_le_bytes(data[6..8].try_into().ok()?));
    let height = u32::from(u16::from_le_bytes(data[8..10].try_into().ok()?));
    (width > 0 && height > 0).then_some((width, height))
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2_usize;
    while index + 9 <= data.len() {
        if data[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = data[index + 1];
        // Start-of-frame markers carry the dimensions; C4/C8/CC are not SOF.
        if matches!(
            marker,
            0xC0 | 0xC1
                | 0xC2
                | 0xC3
                | 0xC5
                | 0xC6
                | 0xC7
                | 0xC9
                | 0xCA
                | 0xCB
                | 0xCD
                | 0xCE
                | 0xCF
        ) {
            let height = u32::from(u16::from_be_bytes(
                data[index + 5..index + 7].try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                data[index + 7..index + 9].try_into().ok()?,
            ));
            return (width > 0 && height > 0).then_some((width, height));
        }
        // Standalone markers have no length payload.
        if matches!(marker, 0xD8 | 0xD9 | 0x01 | 0xD0..=0xD7) {
            index += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes(
            data[index + 2..index + 4].try_into().ok()?,
        ));
        if length < 2 {
            return None;
        }
        index += 2 + length;
    }
    None
}

fn estimate_json_tokens(value: &impl serde::Serialize) -> u64 {
    serde_json::to_string(value)
        .map(|value| estimate_text_tokens(&value))
        .unwrap_or_default()
}

/// Prices text by script instead of by character count alone.
///
/// The old `chars / 4 + newlines` estimate was accurate for English/code but
/// underpriced CJK by 2-3x (a Chinese-heavy 628,726-token conversation was
/// estimated at roughly 410,000). Compaction triggers, summary-input trimming,
/// and retention budgets all floor on this function, so a script-aware split
/// keeps every downstream decision on the same side of the real window.
fn estimate_text_tokens(text: &str) -> u64 {
    let text = text.trim();
    if text.is_empty() {
        return 0;
    }
    let mut ascii = 0_u64;
    let mut cjk = 0_u64;
    let mut other = 0_u64;
    let mut newlines = 0_u64;
    for character in text.chars() {
        if character == '\n' {
            newlines += 1;
        }
        if character.is_ascii() {
            ascii += 1;
        } else if is_cjk(character) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    ascii
        .div_ceil(ASCII_CHARS_PER_TOKEN)
        .saturating_add(cjk.div_ceil(CJK_CHARS_PER_TOKEN))
        .saturating_add(other.div_ceil(OTHER_CHARS_PER_TOKEN))
        .saturating_add(newlines)
        .max(1)
}

/// CJK ideographs, kana, hangul, and full-width forms.
fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x2E80..=0x2FFF       // CJK radicals, Kangxi radicals
        | 0x3000..=0x303F     // CJK punctuation
        | 0x3040..=0x30FF     // Hiragana, Katakana
        | 0x3100..=0x312F     // Bopomofo
        | 0x3400..=0x4DBF     // CJK Extension A
        | 0x4E00..=0x9FFF     // CJK Unified Ideographs
        | 0xA960..=0xA97F     // Hangul Jamo Extended-A
        | 0xAC00..=0xD7AF     // Hangul syllables
        | 0xF900..=0xFAFF     // CJK Compatibility Ideographs
        | 0xFE10..=0xFE1F     // Vertical forms
        | 0xFE30..=0xFE4F     // CJK Compatibility Forms
        | 0xFF00..=0xFFEF     // Halfwidth and Fullwidth Forms
        | 0x20000..=0x2FA1F   // CJK Extensions B-F and compatibility supplement
    )
}

pub(crate) fn parse_token_count(value: &str) -> Option<u64> {
    let value = value.trim().to_ascii_lowercase();
    let (number, multiplier) = match value.chars().last()? {
        'k' => (&value[..value.len() - 1], 1_000),
        'm' => (&value[..value.len() - 1], 1_000_000),
        _ => (value.as_str(), 1),
    };
    number.parse::<u64>().ok()?.checked_mul(multiplier)
}

pub(crate) fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 && tokens.is_multiple_of(1_000) {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        ProviderReplayState, Role, ToolCallContent, ToolDefinition, ToolResultContent,
    };

    fn prompt() -> PromptSpec {
        PromptSpec {
            instructions: "system instructions".into(),
            tools: vec![ToolDefinition {
                name: "Read".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
        }
    }

    #[test]
    fn context_estimate_grows_with_provider_visible_text_and_tools() {
        let short = vec![ProjectedMessage {
            message_id: "short".into(),
            role: Role::User,
            content: ProjectedContent::Parts(vec![ContentPart::Text {
                text: "hello".into(),
            }]),
        }];
        let long = vec![ProjectedMessage {
            message_id: "long".into(),
            role: Role::User,
            content: ProjectedContent::Parts(vec![ContentPart::Text {
                text: "x".repeat(40_000),
            }]),
        }];
        let without_tools = PromptSpec {
            instructions: prompt().instructions,
            tools: Vec::new(),
        };

        assert!(
            estimate_context_tokens(&prompt(), &short)
                > estimate_context_tokens(&without_tools, &short)
        );
        assert!(
            estimate_context_tokens(&prompt(), &long) > estimate_context_tokens(&prompt(), &short)
        );
    }

    #[test]
    fn context_estimate_counts_tool_calls_results_and_images() {
        let assistant = ProjectedMessage {
            message_id: "assistant".into(),
            role: Role::Assistant,
            content: ProjectedContent::Assistant {
                text: String::new(),
                thinking: "reasoning".into(),
                replay_state: None,
                calls: vec![ToolCallContent {
                    index: 0,
                    call_id: "call-1".into(),
                    name: "Read".into(),
                    arguments: serde_json::json!({"path": "/tmp/file"}),
                }],
            },
        };
        let text_result = ProjectedMessage {
            message_id: "result-text".into(),
            role: Role::Tool,
            content: ProjectedContent::ToolResult(ToolResultContent {
                call_id: "call-1".into(),
                name: "Read".into(),
                content: "file contents".into(),
                is_error: false,
                image: None,
                provider_parts: Vec::new(),
            }),
        };
        let image_result = ProjectedMessage {
            message_id: "result-image".into(),
            role: Role::Tool,
            content: ProjectedContent::ToolResult(ToolResultContent {
                call_id: "call-1".into(),
                name: "Read".into(),
                content: "file contents".into(),
                is_error: false,
                image: None,
                provider_parts: vec![ContentPart::Image {
                    mime_type: "image/png".into(),
                    data: vec![0; 32],
                }],
            }),
        };

        let base = estimate_context_tokens(&prompt(), &[]);
        let with_call = estimate_context_tokens(&prompt(), std::slice::from_ref(&assistant));
        let with_text = estimate_context_tokens(&prompt(), &[assistant.clone(), text_result]);
        let with_image = estimate_context_tokens(&prompt(), &[assistant, image_result]);
        assert!(with_call > base);
        assert!(with_text > with_call);
        assert!(with_image > with_text);
    }

    #[test]
    fn assistant_replay_state_does_not_duplicate_thinking_or_count_signature() {
        let assistant = |replay_state| ProjectedMessage {
            message_id: "assistant".into(),
            role: Role::Assistant,
            content: ProjectedContent::Assistant {
                text: "answer".into(),
                thinking: "reasoning".repeat(1_000),
                replay_state,
                calls: Vec::new(),
            },
        };
        let without_replay = assistant(None);
        let with_replay = assistant(Some(ProviderReplayState {
            provider_kind: "anthropic".into(),
            value: serde_json::json!({
                "blocks": [{
                    "type": "thinking",
                    "thinking": "reasoning".repeat(1_000),
                    "signature": "s".repeat(282_100)
                }]
            }),
        }));

        assert_eq!(
            estimate_projected_messages_tokens(&[without_replay]),
            estimate_projected_messages_tokens(&[with_replay])
        );
    }

    #[test]
    fn cjk_text_prices_at_least_twice_as_high_as_ascii_of_the_same_length() {
        let ascii = estimate_text_tokens(&"a".repeat(8_000));
        let cjk = estimate_text_tokens(&"中".repeat(8_000));
        let japanese = estimate_text_tokens(&"あ".repeat(8_000));

        // 4 ASCII characters/token vs 2 CJK characters/token.
        assert_eq!(ascii, 2_000);
        assert_eq!(cjk, 4_000);
        assert_eq!(japanese, 4_000);
    }

    #[test]
    fn mixed_script_text_does_not_collapse_to_the_ascii_density() {
        // Half English code, half Chinese: the old chars/4 estimate priced the
        // whole string as ASCII, which is exactly the case that under-triggered
        // compaction on CJK-heavy conversations.
        let mixed = format!(
            "{}{}",
            "fn main() {}\n".repeat(1_000),
            "中文内容".repeat(2_000)
        );
        let ascii_only = estimate_text_tokens(&"x".repeat(mixed.chars().count()));
        assert!(
            estimate_text_tokens(&mixed) > ascii_only,
            "mixed-script text must price above pure-ASCII text of the same length"
        );
    }

    #[test]
    fn one_chinese_character_is_cheaper_than_one_token_but_not_four_to_one() {
        // The base weight is deliberately a compromise across tokenizers; the
        // compaction calibration corrects it with provider usage.
        let tokens = estimate_text_tokens("中文内容");
        assert_eq!(tokens, 2, "four CJK characters estimate as two tokens");
    }

    #[test]
    fn image_tokens_follow_the_pixels_instead_of_a_flat_fee() {
        let png = |width: u32, height: u32| {
            let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
            bytes.extend_from_slice(&13_u32.to_be_bytes());
            bytes.extend_from_slice(b"IHDR");
            bytes.extend_from_slice(&width.to_be_bytes());
            bytes.extend_from_slice(&height.to_be_bytes());
            bytes
        };
        let small = estimate_image_tokens("image/png", &png(64, 64));
        let screenshot = estimate_image_tokens("image/png", &png(1920, 1080));
        assert!(small >= 200);
        assert!(
            screenshot > 2_000,
            "a full-size screenshot must not be priced like a thumbnail"
        );
        assert!(screenshot > small);
        assert_eq!(
            estimate_image_tokens("image/png", b"not a png"),
            TOKENS_PER_IMAGE
        );
    }
}
