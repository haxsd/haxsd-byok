//! What the Devin model picker offers, and what the client asks for afterwards.
//!
//! Devin only ever requests identifiers it knows, so a mapping whose UID was
//! invented by this application can never be reached: the picker has nothing to
//! show and no request carries that name. Two sources answer "what can I map":
//!
//! - the models users actually pick from, recorded with the identifier each one
//!   turns into — a picker entry and its identifier are different strings, so
//!   `SWE-1.6 Slow` would otherwise have to be guessed as `swe-1-6-slow`;
//! - the compatibility identifiers the installed client recognises, read from
//!   its bundle, which is also where the host patch already looks.

use std::collections::BTreeSet;
use std::path::Path;

use regex::Regex;
use serde::Serialize;

use crate::{Error, Result};

/// The identifiers that exist specifically for routing a client's model traffic
/// to a third-party model. They carry this marker; the client's remaining model
/// enum entries (345 of them) are identifiers no account can select, and its
/// permission enum uses the same shape for names like `MODEL_ALLOW`.
const BYOK_MARKER: &str = "BYOK";

/// The models a user actually picks from, paired with the identifier the client
/// then asks for. Both halves come from the installed client rather than from
/// this project: the display name is what the picker shows, the identifier is
/// what the client requests afterwards.
pub struct KnownModel {
    pub name: &'static str,
    pub uid: &'static str,
}

pub const KNOWN_MODELS: [KnownModel; 18] = [
    KnownModel {
        name: "SWE-1.6 Slow",
        uid: "swe-1-6-slow",
    },
    KnownModel {
        name: "SWE-1.6 Fast",
        uid: "swe-1-6-fast",
    },
    KnownModel {
        name: "SWE-1.7 Medium",
        uid: "swe-1-7-medium",
    },
    KnownModel {
        name: "SWE-1.7 Lightning Medium",
        uid: "swe-1-7-lightning-medium",
    },
    KnownModel {
        name: "SWE-2 High",
        uid: "swe-2-high",
    },
    KnownModel {
        name: "Claude Fable 5.1 Medium",
        uid: "claude-fable-5-1-medium",
    },
    KnownModel {
        name: "Claude Opus 5.5 Medium",
        uid: "claude-opus-5-5-medium",
    },
    KnownModel {
        name: "Claude Sonnet 5 Medium",
        uid: "claude-sonnet-5-medium",
    },
    KnownModel {
        name: "GPT-6 Astra Medium Thinking",
        uid: "gpt-6-astra-medium-thinking",
    },
    KnownModel {
        name: "GPT-6 Luna Medium Thinking",
        uid: "gpt-6-luna-medium-thinking",
    },
    KnownModel {
        name: "GPT-6 Sol Medium Thinking",
        uid: "gpt-6-sol-medium-thinking",
    },
    KnownModel {
        name: "Gemini 3.8 Flash Medium",
        uid: "gemini-3-8-flash-medium",
    },
    KnownModel {
        name: "Kimi K3 High",
        uid: "kimi-k3-high",
    },
    KnownModel {
        name: "GLM-5.2 High",
        uid: "glm-5-2-high",
    },
    KnownModel {
        name: "GLM-5.3 High",
        uid: "glm-5-3-high",
    },
    KnownModel {
        name: "GLM-5.3 Max",
        uid: "glm-5-3-max",
    },
    KnownModel {
        name: "DeepSeek V4.1 Flash",
        uid: "deepseek-v4-1-flash",
    },
    KnownModel {
        name: "DeepSeek V4.1 Flash High",
        uid: "deepseek-v4-1-flash-high",
    },
];

/// One entry of the picker: the name to show and the identifier to map.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelChoice {
    pub name: String,
    pub uid: String,
}

/// The picker choices: the models a user recognises, then the compatibility
/// identifiers, so the list stays short enough to read.
pub fn choices(byok: &[String]) -> Vec<ModelChoice> {
    let mut choices: Vec<ModelChoice> = KNOWN_MODELS
        .iter()
        .map(|model| ModelChoice {
            name: model.name.to_owned(),
            uid: model.uid.to_owned(),
        })
        .collect();
    choices.extend(byok.iter().map(|uid| ModelChoice {
        name: uid.clone(),
        uid: uid.clone(),
    }));
    choices
}

/// The enum entries look like `A[A.MODEL_CLAUDE_4_SONNET_BYOK=279]="MODEL_CLAUDE_4_SONNET_BYOK"`.
fn uid_regex() -> Regex {
    Regex::new(r#"A\[A\.(MODEL_[A-Z0-9_]+)=\d+\]"#).expect("valid Devin model UID anchor")
}

pub fn read(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path).map_err(|error| {
        Error::Config(format!(
            "cannot read the Devin client at {}: {error}",
            path.display()
        ))
    })?;
    Ok(parse(&content))
}

pub fn parse(content: &str) -> Vec<String> {
    let regex = uid_regex();
    let mut uids = BTreeSet::new();
    for captures in regex.captures_iter(content) {
        let Some(uid) = captures.get(1) else {
            continue;
        };
        let uid = uid.as_str();
        if uid.ends_with(BYOK_MARKER) {
            uids.insert(uid.to_owned());
        }
    }
    uids.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        r#"A[A.MODEL_CLAUDE_4_SONNET_BYOK=279]="MODEL_CLAUDE_4_SONNET_BYOK","#,
        r#"A[A.MODEL_CLAUDE_4_OPUS_BYOK=277]="MODEL_CLAUDE_4_OPUS_BYOK","#,
        r#"A[A.MODEL_CHAT_GPT_5=412]="MODEL_CHAT_GPT_5","#,
        r#"A[A.USER_ALLOW=1]="USER_ALLOW",A[A.MODEL_ALLOW=5]="MODEL_ALLOW""#,
    );

    /// 客户端只请求自己表里的 UID，所以界面必须读到这份表。权限枚举与模型枚举的写法
    /// 完全一样（`MODEL_ALLOW`），只有兼容标识那一类能被映射，就只认那一类。
    #[test]
    fn reads_only_the_compatibility_identifiers() {
        let uids = parse(FIXTURE);
        assert_eq!(
            uids,
            vec![
                "MODEL_CLAUDE_4_OPUS_BYOK".to_owned(),
                "MODEL_CLAUDE_4_SONNET_BYOK".to_owned()
            ]
        );
    }

    /// 选择器里显示的名字与客户端随后请求的标识不是同一个字符串，所以界面拿到的
    /// 必须是成对的；只给标识会让用户对着几百个枚举值自己猜。
    #[test]
    fn choices_pair_the_picker_name_with_the_requested_identifier() {
        let choices = choices(&parse(FIXTURE));
        let swe = choices
            .iter()
            .find(|choice| choice.uid == "swe-1-6-slow")
            .expect("the picker entry must be offered");
        assert_eq!(swe.name, "SWE-1.6 Slow");

        let byok = choices
            .iter()
            .find(|choice| choice.uid == "MODEL_CLAUDE_4_SONNET_BYOK")
            .expect("compatibility identifiers must stay selectable");
        assert_eq!(byok.name, "MODEL_CLAUDE_4_SONNET_BYOK");

        assert!(
            choices.iter().all(|choice| !choice.uid.is_empty()),
            "every choice must carry an identifier"
        );
    }
}
