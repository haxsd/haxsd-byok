//! Request-local effort adaptation. Successful values are never rewritten and no capabilities are cached.
use serde_json::Value;

const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EffortField {
    Chat,
    Responses,
    Anthropic,
}

impl EffortField {
    fn path(self) -> &'static [&'static str] {
        match self {
            Self::Chat => &["reasoning_effort"],
            Self::Responses => &["reasoning", "effort"],
            Self::Anthropic => &["output_config", "effort"],
        }
    }

    pub(crate) fn value(self, body: &Value) -> Option<&str> {
        self.path()
            .iter()
            .try_fold(body, |v, k| v.get(*k))?
            .as_str()
    }
}

/// One model call owns this state, including every value already rejected by that upstream.
pub(crate) struct EffortFallback {
    field: EffortField,
    candidates: Vec<&'static str>,
    next: usize,
    active: bool,
}

pub(crate) struct Adjustment {
    pub rejected: String,
    pub adjusted: Option<&'static str>,
}

impl EffortFallback {
    pub(crate) fn new(body: &Value, field: EffortField) -> Self {
        let original = field
            .value(body)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let active = !matches!(original.as_str(), "" | "none" | "off" | "disabled");
        let mut candidates = LEVELS.to_vec();
        if let Some(rank) = LEVELS.iter().position(|v| *v == original) {
            candidates.sort_by_key(|candidate| {
                let index = LEVELS.iter().position(|v| v == candidate).unwrap();
                (index.abs_diff(rank), std::cmp::Reverse(index))
            });
        } else {
            candidates = vec!["high", "xhigh", "max", "medium", "low"];
        }
        candidates.retain(|value| *value != original);
        Self {
            field,
            candidates,
            next: 0,
            active,
        }
    }

    pub(crate) fn adjust(
        &mut self,
        body: &mut Value,
        status: u16,
        raw: &str,
    ) -> Option<Adjustment> {
        if !self.active || !matches!(status, 400 | 422) {
            return None;
        }
        let rejected = self.field.value(body)?.to_owned();
        let unsupported_parameter = rejection(self.field, raw)?;
        let adjusted = if unsupported_parameter {
            None
        } else {
            let next = self.candidates.get(self.next).copied();
            self.next += 1;
            next
        };
        let (last, parents) = self.field.path().split_last()?;
        let mut parent = body;
        for key in parents {
            parent = parent.get_mut(*key)?;
        }
        if let Some(value) = adjusted {
            parent
                .as_object_mut()?
                .insert((*last).into(), Value::String(value.into()));
        } else {
            parent.as_object_mut()?.remove(*last);
            self.active = false;
        }
        Some(Adjustment { rejected, adjusted })
    }
}

/// A list of legal values is optional. Both the parameter identity and rejection must be clear.
/// The boolean distinguishes rejecting an entire parameter from rejecting its value.
fn rejection(field: EffortField, raw: &str) -> Option<bool> {
    let parsed = serde_json::from_str::<Value>(raw).ok();
    let error = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .or(parsed.as_ref());
    let code = error
        .and_then(|v| v.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let parameter = error
        .and_then(|v| v.get("param").or_else(|| v.get("parameter")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let message = error
        .and_then(|v| v.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(raw);
    let lower = message.to_ascii_lowercase();
    let names = [
        "reasoning_effort",
        "reasoning.effort",
        "output_config.effort",
        "effort",
    ];
    if !parameter.is_empty() && !names.contains(&parameter.as_str()) {
        return None;
    }
    let identified = !parameter.is_empty()
        || ["reasoning_effort", "reasoning.effort", "reasoning effort", "output_config.effort"].iter().any(|v| lower.contains(v))
        || (field == EffortField::Anthropic && lower.contains("effort"))
        || message.contains("思考强度") || message.contains("推理强度")
        // This provider's documented validation response names thinking rather than the wire field.
        || (code == "1210" && message.contains("思考") && message.contains("请使用"));
    if !identified {
        return None;
    }
    let parameter_rejected = [
        "unsupported_parameter",
        "unknown_parameter",
        "unrecognized_parameter",
        "extra_forbidden",
    ]
    .contains(&code.as_str())
        || [
            "unknown parameter",
            "unrecognized parameter",
            "unsupported parameter",
            "unexpected keyword",
            "extra inputs are not permitted",
        ]
        .iter()
        .any(|v| lower.contains(v));
    if parameter_rejected {
        return Some(true);
    }
    let value_rejected = ["unsupported_value", "invalid_value"].contains(&code.as_str())
        || [
            "not supported",
            "unsupported",
            "invalid",
            "must be",
            "allowed values",
            "supported values",
            "expected one of",
        ]
        .iter()
        .any(|v| lower.contains(v))
        || ["不支持", "无效", "非法", "请使用", "合法值"]
            .iter()
            .any(|v| message.contains(v));
    value_rejected.then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_error_without_an_allowed_list_still_tries_a_nearby_level() {
        let mut body =
            json!({"reasoning_effort":"xhigh", "messages":[{"role":"user","content":"hello"}]});
        let mut state = EffortFallback::new(&body, EffortField::Chat);
        assert_eq!(
            body["reasoning_effort"], "xhigh",
            "construction must not pre-map a supported value"
        );
        let change = state
            .adjust(&mut body, 400, "unsupported reasoning effort: xhigh")
            .unwrap();
        assert_eq!(change.rejected, "xhigh");
        assert_eq!(change.adjusted, Some("max"));
        assert_eq!(body["messages"], json!([{"role":"user","content":"hello"}]));
    }

    #[test]
    fn failures_visit_each_level_once_then_omit_only_the_effort() {
        let mut body = json!({"reasoning":{"effort":"medium","summary":"auto"}});
        let mut state = EffortFallback::new(&body, EffortField::Responses);
        let mut seen = Vec::new();
        while let Some(change) = state.adjust(&mut body, 422, r#"{"error":{"param":"reasoning.effort","code":"unsupported_value","message":"rejected"}}"#) {
            seen.push(change.adjusted);
        }
        assert_eq!(
            seen,
            vec![Some("high"), Some("low"), Some("xhigh"), Some("max"), None]
        );
        assert_eq!(body, json!({"reasoning":{"summary":"auto"}}));
    }

    #[test]
    fn an_unsupported_parameter_is_omitted_without_trying_other_values() {
        let mut body = json!({"output_config":{"effort":"high","format":"text"}});
        let mut state = EffortFallback::new(&body, EffortField::Anthropic);
        assert!(state
            .adjust(
                &mut body,
                400,
                r#"{"error":{"param":"output_config.effort","code":"unsupported_parameter"}}"#
            )
            .unwrap()
            .adjusted
            .is_none());
        assert_eq!(body, json!({"output_config":{"format":"text"}}));
        assert!(state
            .adjust(&mut body, 400, "unsupported reasoning effort")
            .is_none());
    }

    #[test]
    fn unrelated_errors_statuses_budgets_and_explicit_off_are_unchanged() {
        for (status, text) in [
            (401, "unsupported reasoning effort"),
            (429, "unsupported reasoning effort"),
            (500, "unsupported reasoning effort"),
            (400, "thinking budget is invalid"),
            (400, "invalid request"),
            (400, "reasoning_content is missing"),
            (400, "max_tokens is invalid"),
            (
                400,
                r#"{"error":{"param":"messages","message":"invalid reasoning effort in history"}}"#,
            ),
        ] {
            let mut body = json!({"reasoning_effort":"xhigh"});
            let mut state = EffortFallback::new(&body, EffortField::Chat);
            assert!(
                state.adjust(&mut body, status, text).is_none(),
                "{status}: {text}"
            );
            assert_eq!(body["reasoning_effort"], "xhigh");
        }
        for value in ["none", "off", "disabled", ""] {
            let mut body = json!({"reasoning_effort":value});
            let mut state = EffortFallback::new(&body, EffortField::Chat);
            assert!(state
                .adjust(&mut body, 400, "unsupported reasoning effort")
                .is_none());
        }
    }

    #[test]
    fn chinese_value_rejection_does_not_require_an_english_parameter_name() {
        let mut body = json!({"reasoning_effort":"medium"});
        let mut state = EffortFallback::new(&body, EffortField::Chat);
        assert_eq!(state.adjust(&mut body, 400, r#"{"error":{"code":"1210","message":"该模型始终思考，不支持关闭思考；请使用 low、high 或 max。"}}"#).unwrap().adjusted, Some("high"));
    }
}
