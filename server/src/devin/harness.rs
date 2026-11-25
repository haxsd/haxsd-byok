use std::cmp::Ordering;
use std::collections::HashMap;

use crate::Result;

use super::{
    catalog::client_model_uid,
    directory::is_native_model_uid,
    wire::{self, Field, FieldValue},
};

/// 官方目录里一个模型条目携带的 agent 配置。
///
/// 只保存会随 harness 走的字段：提示词模板、工具格式、harness UID 与压缩阈值。
/// 模型 UID、推理地址、上下文长度和图片能力仍由本地条目自己决定。
#[derive(Clone, Default)]
struct ModelProfile {
    harnesses: Vec<String>,
    prompt_templater: Option<Field>,
    tool_formatter: Option<Field>,
    compaction: Option<Field>,
    default: bool,
    model_uid: String,
    /// `swe-2.1` 这类模型版本；只用于选择最新的自研 SWE 系列配置。
    swe_version: Option<Vec<u64>>,
}

#[derive(Default)]
pub(super) struct HarnessCatalog {
    models: HashMap<String, Vec<ModelProfile>>,
    /// 没有同名官方模型的本地模型使用最新 SWE 系列配置。
    fallback: Option<ModelProfile>,
}

impl HarnessCatalog {
    pub fn from_fields(fields: &[Field]) -> Result<Self> {
        let mut catalog = Self::default();
        for field in wire::all_fields(fields, 1) {
            let Some(uid) = client_model_uid(field) else {
                continue;
            };
            if is_native_model_uid(&uid) {
                continue;
            }
            let model = nested(field)?;
            let info = message(&model, 23)?;
            let harnesses = strings(&info, 20);
            let prompt_templater = wire::first_field(&info, 14).cloned();
            let tool_formatter = wire::first_field(&info, 15).cloned();
            let compaction = wire::first_field(&info, 26).cloned();
            if harnesses.is_empty()
                && prompt_templater.is_none()
                && tool_formatter.is_none()
                && compaction.is_none()
            {
                continue;
            }
            let metadata = message(&model, 30)?;
            let default = boolean(&model, 31) || boolean(&metadata, 3);
            let model_name = strings(&info, 8).into_iter().next().unwrap_or_default();
            let profile = ModelProfile {
                harnesses,
                prompt_templater,
                tool_formatter,
                compaction,
                default,
                model_uid: uid.clone(),
                swe_version: swe_version(&uid, &model_name),
            };
            let mut identities = vec![uid];
            identities.extend(strings(&info, 23));
            identities.extend(strings(&metadata, 1));
            identities.sort();
            identities.dedup();
            for identity in identities {
                catalog
                    .models
                    .entry(normalize(&identity))
                    .or_default()
                    .push(profile.clone());
            }
            if profile.swe_version.is_some()
                && catalog
                    .fallback
                    .as_ref()
                    .is_none_or(|current| better_swe_profile(&profile, current))
            {
                catalog.fallback = Some(profile);
            }
        }
        Ok(catalog)
    }

    pub fn attach(&self, field: &Field) -> Result<Field> {
        if !client_model_uid(field).is_some_and(|uid| is_native_model_uid(&uid)) {
            return Ok(field.clone());
        }
        let mut model = nested(field)?;
        let Some(info_field) = model.iter_mut().find(|field| field.number == 23) else {
            return Ok(field.clone());
        };
        let mut info = nested(info_field)?;
        let model_id = strings(&info, 8).into_iter().next().unwrap_or_default();
        let key = normalize(&model_id);
        let profile = if self.models.contains_key(&key) {
            self.resolve(&key)
        } else {
            // 这是产品选择：没有同名官方模型的本地模型，默认复用 Devin 最新 SWE
            // 系列（当前 swe-2.x）的 harness 工程，而不是通用 CLI 默认配置。
            self.fallback.as_ref()
        };
        // 本地目录以前可能带过自造的 harness UID；无论这次有没有官方 profile，
        // 都要先清掉这些字段，避免旧值继续冒充官方配置。
        info.retain(|field| !matches!(field.number, 14 | 15 | 20 | 26));
        if let Some(profile) = profile {
            apply_profile(&mut info, profile);
        }
        info_field.value = FieldValue::Bytes(wire::serialize_fields(&info)?);
        Ok(Field::bytes(1, wire::serialize_fields(&model)?))
    }

    fn resolve(&self, key: &str) -> Option<&ModelProfile> {
        let profiles = self.models.get(key)?;
        let defaults = profiles
            .iter()
            .filter(|profile| profile.default)
            .collect::<Vec<_>>();
        let candidates = if defaults.is_empty() {
            profiles.iter().collect::<Vec<_>>()
        } else {
            defaults
        };
        let first = candidates.first()?;
        if candidates
            .iter()
            .any(|profile| profile.harnesses != first.harnesses)
        {
            return None;
        }
        Some(first)
    }
}

/// 把选中的官方 harness 字段搬到本地模型。保留本地模型自己的 UID、推理地址、
/// 上下文和模型名称；只替换会改变 agent 行为的配置。
fn apply_profile(info: &mut Vec<Field>, profile: &ModelProfile) {
    if let Some(field) = &profile.prompt_templater {
        info.push(field.clone());
    }
    if let Some(field) = &profile.tool_formatter {
        info.push(field.clone());
    }
    for harness in &profile.harnesses {
        info.push(Field::bytes(20, harness));
    }
    if let Some(field) = &profile.compaction {
        info.push(field.clone());
    }
}

fn better_swe_profile(candidate: &ModelProfile, current: &ModelProfile) -> bool {
    match candidate.swe_version.cmp(&current.swe_version) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => {
            if candidate.default != current.default {
                return candidate.default;
            }
            if candidate.harnesses.len() != current.harnesses.len() {
                return candidate.harnesses.len() > current.harnesses.len();
            }
            candidate.model_uid.len() < current.model_uid.len()
                || (candidate.model_uid.len() == current.model_uid.len()
                    && candidate.model_uid < current.model_uid)
        }
    }
}

/// `swe-2-high` → `[2]`；`swe-2.1` → `[2, 1]`；`swe-1-6-slow` → `[1, 6]`。
/// 只从 SWE 前缀后提取数字，避免把模型变体名里的数字当成版本。
fn swe_version(uid: &str, model_name: &str) -> Option<Vec<u64>> {
    [uid, model_name]
        .into_iter()
        .filter_map(version_after_swe)
        .max()
}

fn version_after_swe(value: &str) -> Option<Vec<u64>> {
    let lower = value.to_ascii_lowercase();
    let start = lower.find("swe")?;
    let mut numbers = Vec::new();
    let mut digits = String::new();
    for character in lower[start + 3..].chars() {
        if character.is_ascii_digit() {
            digits.push(character);
        } else if !digits.is_empty() {
            numbers.push(digits.parse().ok()?);
            digits.clear();
        }
    }
    if !digits.is_empty() {
        numbers.push(digits.parse().ok()?);
    }
    (!numbers.is_empty()).then_some(numbers)
}

pub(super) fn local_assignments(
    payload: &[u8],
    user_status: bool,
) -> Result<HashMap<String, Vec<String>>> {
    let mut fields = wire::parse_fields(payload)?;
    if user_status {
        fields = message(&message(&fields, 1)?, 33)?;
    }
    let mut assignments = HashMap::new();
    for field in wire::all_fields(&fields, 1) {
        if let Some(uid) = client_model_uid(field).filter(|uid| is_native_model_uid(uid)) {
            assignments.insert(uid, strings(&message(&nested(field)?, 23)?, 20));
        }
    }
    Ok(assignments)
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn nested(field: &Field) -> Result<Vec<Field>> {
    match &field.value {
        FieldValue::Bytes(bytes) => wire::parse_fields(bytes),
        _ => Err(crate::Error::Protocol(
            "Devin model metadata must be a message".into(),
        )),
    }
}

fn message(fields: &[Field], number: u32) -> Result<Vec<Field>> {
    wire::first_field(fields, number)
        .map(nested)
        .transpose()
        .map(Option::unwrap_or_default)
}

fn strings(fields: &[Field], number: u32) -> Vec<String> {
    wire::all_fields(fields, number)
        .filter_map(|field| match &field.value {
            FieldValue::Bytes(bytes) => String::from_utf8(bytes.clone()).ok(),
            _ => None,
        })
        .collect()
}

fn boolean(fields: &[Field], number: u32) -> bool {
    wire::first_field(fields, number)
        .is_some_and(|field| matches!(field.value, FieldValue::Varint(value) if value != 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devin::wire::{first_field, serialize_fields};

    fn official_model(uid: &str, model_name: &str, harness: &str, prompt: u64, tool: u64) -> Field {
        Field::bytes(
            1,
            serialize_fields(&[
                Field::bytes(1, model_name),
                Field::bytes(22, uid),
                Field::bytes(
                    23,
                    serialize_fields(&[
                        Field::bytes(8, uid),
                        Field::varint(14, prompt),
                        Field::varint(15, tool),
                        Field::bytes(20, harness),
                        Field::bytes(26, serialize_fields(&[Field::varint(1, 100_000)]).unwrap()),
                    ])
                    .unwrap(),
                ),
            ])
            .unwrap(),
        )
    }

    fn local_model(model_name: &str) -> Field {
        Field::bytes(
            1,
            serialize_fields(&[
                Field::bytes(1, "Local"),
                Field::bytes(22, "haxsd-byok:local"),
                Field::bytes(
                    23,
                    serialize_fields(&[Field::bytes(8, model_name)]).unwrap(),
                ),
            ])
            .unwrap(),
        )
    }

    fn info(field: &Field) -> Vec<Field> {
        let model = nested(field).unwrap();
        nested(first_field(&model, 23).unwrap()).unwrap()
    }

    fn version(info: &[Field], number: u32) -> Option<u64> {
        first_field(info, number).and_then(|field| match field.value {
            FieldValue::Varint(value) => Some(value),
            _ => None,
        })
    }

    #[test]
    fn an_unmatched_local_model_uses_the_latest_swe_harness_config() {
        let old = official_model("swe-1-6-slow", "SWE-1.6", "old-harness", 1, 2);
        let latest = official_model("swe-2-high", "SWE-2.1", "new-harness", 14, 15);
        let payload = serialize_fields(&[old, latest]).unwrap();
        let fields = wire::parse_fields(&payload).unwrap();
        let catalog = HarnessCatalog::from_fields(&fields).unwrap();

        let attached = catalog.attach(&local_model("deepseek-flash")).unwrap();
        let info = info(&attached);
        assert_eq!(version(&info, 14), Some(14));
        assert_eq!(version(&info, 15), Some(15));
        assert_eq!(
            strings(&info, 20),
            vec!["new-harness".to_string()],
            "应该选择最新 SWE 系列的 harness"
        );
        assert!(first_field(&info, 26).is_some(), "压缩阈值也要跟随 harness");
    }

    #[test]
    fn an_exact_official_model_match_wins_over_the_swe_fallback() {
        let exact = official_model("deepseek-flash", "DeepSeek Flash", "exact-harness", 7, 8);
        let swe = official_model("swe-2-high", "SWE-2.1", "swe-harness", 14, 15);
        let payload = serialize_fields(&[exact, swe]).unwrap();
        let fields = wire::parse_fields(&payload).unwrap();
        let catalog = HarnessCatalog::from_fields(&fields).unwrap();

        let attached = catalog.attach(&local_model("deepseek-flash")).unwrap();
        let info = info(&attached);
        assert_eq!(strings(&info, 20), vec!["exact-harness".to_string()]);
        assert_eq!(version(&info, 14), Some(7));
    }
}
