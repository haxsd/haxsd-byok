//! Optional Devin protocol integration.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const DEFAULT_DEVIN_API_PORT: u16 = 43_110;
pub const DEFAULT_DEVIN_INFERENCE_PORT: u16 = 43_111;
pub const DEFAULT_DEVIN_LOCAL_API_PORT: u16 = 43_112;

/// Everything the gateway does not serve locally is forwarded here, the same way
/// the reference router forwards it. Sign-in, account state and telemetry keep
/// reaching Devin's own service; only model traffic is answered locally.
pub const DEFAULT_DEVIN_UPSTREAM_API_URL: &str = "https://server.codeium.com";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DevinSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auth_token: String,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    #[serde(default = "default_inference_port")]
    pub inference_port: u16,
    #[serde(default = "default_local_api_port")]
    pub local_api_port: u16,
    #[serde(default = "default_upstream_api_url")]
    pub upstream_api_url: String,
    #[serde(default)]
    pub bindings: Vec<DevinModelBinding>,
    /// The binding Devin selects when it opens. Validation accepts a missing
    /// value so an older row can still be read and repaired; saving an enabled
    /// gateway requires it through [`DevinSettings::validate_for_use`].
    #[serde(default)]
    pub default_model_uid: Option<String>,
}

impl Default for DevinSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auth_token: String::new(),
            api_port: DEFAULT_DEVIN_API_PORT,
            inference_port: DEFAULT_DEVIN_INFERENCE_PORT,
            local_api_port: DEFAULT_DEVIN_LOCAL_API_PORT,
            upstream_api_url: default_upstream_api_url(),
            bindings: Vec::new(),
            default_model_uid: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DevinBindingKind {
    #[default]
    Standard,
    ContextCompression,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DevinRoute {
    #[serde(default)]
    pub route_id: String,
    #[serde(default)]
    pub model_hash: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for DevinRoute {
    fn default() -> Self {
        Self {
            route_id: String::new(),
            model_hash: String::new(),
            label: String::new(),
            enabled: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DevinModelBinding {
    pub model_uid: String,
    pub model_hash: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub context_window_tokens: Option<u64>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub kind: DevinBindingKind,
    #[serde(default)]
    pub routes: Vec<DevinRoute>,
    #[serde(default)]
    pub active_route_id: Option<String>,
}

impl DevinModelBinding {
    pub fn new(model_uid: impl Into<String>, model_hash: impl Into<String>) -> Self {
        Self {
            model_uid: model_uid.into(),
            model_hash: model_hash.into(),
            display_name: String::new(),
            context_window_tokens: None,
            enabled: true,
            kind: DevinBindingKind::default(),
            routes: Vec::new(),
            active_route_id: None,
        }
    }

    pub fn effective_model_hash(&self) -> &str {
        self.active_route_id
            .as_deref()
            .and_then(|active_route_id| {
                self.routes.iter().find(|route| {
                    route.enabled
                        && !route.model_hash.trim().is_empty()
                        && route.route_id == active_route_id
                })
            })
            .map(|route| route.model_hash.trim())
            .unwrap_or_else(|| self.model_hash.trim())
    }
}

impl DevinSettings {
    /// Trim every identifier once at the persistence boundary. Validation already
    /// trims for its emptiness checks; storing the raw value made a pasted UID
    /// pass validation and then never match an incoming Devin request.
    pub fn normalized(mut self) -> Self {
        self.auth_token = self.auth_token.trim().to_owned();
        self.upstream_api_url = self.upstream_api_url.trim().to_owned();
        self.default_model_uid = self
            .default_model_uid
            .take()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        for binding in &mut self.bindings {
            binding.model_uid = binding.model_uid.trim().to_owned();
            binding.model_hash = binding.model_hash.trim().to_owned();
            binding.display_name = binding.display_name.trim().to_owned();
            binding.active_route_id = binding
                .active_route_id
                .take()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty());
            for route in &mut binding.routes {
                route.route_id = route.route_id.trim().to_owned();
                route.model_hash = route.model_hash.trim().to_owned();
                route.label = route.label.trim().to_owned();
            }
        }
        self
    }

    pub fn validate(&self) -> Result<()> {
        for (name, port) in [
            ("api", self.api_port),
            ("inference", self.inference_port),
            ("local API", self.local_api_port),
        ] {
            if port == 0 {
                return Err(Error::Config(format!("Devin {name} port must be non-zero")));
            }
        }
        if self.api_port == self.inference_port
            || self.api_port == self.local_api_port
            || self.inference_port == self.local_api_port
        {
            return Err(Error::Config("Devin ports must be distinct".into()));
        }

        let upstream = self.upstream_api_url.trim();
        if upstream.is_empty() {
            return Err(Error::Config(
                "Devin upstream API URL must not be empty; it receives every request the gateway does not serve locally".into(),
            ));
        }
        match url::Url::parse(upstream) {
            Ok(parsed)
                if matches!(parsed.scheme(), "http" | "https") && parsed.host().is_some() => {}
            _ => {
                return Err(Error::Config(format!(
                    "Devin upstream API URL must be an absolute http(s) URL: {upstream}"
                )))
            }
        }

        let mut uids = HashSet::with_capacity(self.bindings.len());
        for binding in &self.bindings {
            let uid = binding.model_uid.trim();
            if uid.is_empty() {
                return Err(Error::Config("Devin model UID must not be empty".into()));
            }
            if binding.model_hash.trim().is_empty() {
                // "must include a cursor-byok model hash" 是界面的匹配片段：改文案要连着
                // `apps/desktop` 的 `harnessErrorText.ts` 与它的测试一起改。
                return Err(Error::Config(format!(
                    "Devin model binding for {uid} must include a cursor-byok model hash"
                )));
            }
            if !uids.insert(uid.to_owned()) {
                return Err(Error::Config(format!("duplicate Devin model UID: {uid}")));
            }

            if binding.kind == DevinBindingKind::ContextCompression && !binding.routes.is_empty() {
                return Err(Error::Config(format!(
                    "Devin context compression binding {uid} must not define candidate routes"
                )));
            }

            let mut route_ids = HashSet::with_capacity(binding.routes.len());
            for route in &binding.routes {
                let route_id = route.route_id.as_str();
                if route_id.trim().is_empty() {
                    return Err(Error::Config("Devin route ID must not be empty".into()));
                }
                if route.model_hash.trim().is_empty() {
                    return Err(Error::Config(format!(
                        "Devin route {route_id} must include a cursor-byok model hash"
                    )));
                }
                if !route_ids.insert(route_id.to_owned()) {
                    return Err(Error::Config(format!(
                        "duplicate Devin route ID: {route_id}"
                    )));
                }
            }
            if let Some(active_route_id) = binding.active_route_id.as_deref() {
                match binding
                    .routes
                    .iter()
                    .find(|route| route.route_id == active_route_id)
                {
                    None => {
                        return Err(Error::Config(format!(
                            "Devin active route not found: {active_route_id}"
                        )))
                    }
                    Some(route) if !route.enabled => {
                        return Err(Error::Config(format!(
                            "Devin active route must be enabled: {active_route_id}"
                        )))
                    }
                    Some(_) => {}
                }
            }
        }

        let default_uid = self
            .default_model_uid
            .as_deref()
            .map(str::trim)
            .filter(|uid| !uid.is_empty());
        if let Some(default_uid) = default_uid {
            let binding = self
                .bindings
                .iter()
                .find(|binding| binding.model_uid.trim() == default_uid)
                .ok_or_else(|| {
                    Error::Config(format!("Devin default model UID not found: {default_uid}"))
                })?;
            if !binding.enabled {
                return Err(Error::Config(format!(
                    "Devin default model must be enabled: {default_uid}"
                )));
            }
            if binding.kind != DevinBindingKind::Standard {
                return Err(Error::Config(format!(
                    "Devin default model must be a standard binding: {default_uid}"
                )));
            }
        }
        Ok(())
    }

    /// Read-time structural validation accepts a missing default so an existing
    /// configuration can still be opened and repaired. Saving an enabled gateway
    /// is stricter.
    pub fn validate_for_use(&self) -> Result<()> {
        self.validate()?;
        if self.enabled && self.default_binding().is_none() {
            return Err(Error::Config(
                "Devin default model must be selected before enabling the gateway".into(),
            ));
        }
        Ok(())
    }

    pub fn default_binding(&self) -> Option<&DevinModelBinding> {
        let uid = self.default_model_uid.as_deref()?.trim();
        if uid.is_empty() {
            return None;
        }
        self.bindings.iter().find(|binding| {
            binding.enabled
                && binding.kind == DevinBindingKind::Standard
                && binding.model_uid.trim() == uid
        })
    }

    pub fn binding(&self, model_uid: &str) -> Option<&DevinModelBinding> {
        let model_uid = model_uid.trim();
        self.bindings
            .iter()
            .find(|binding| binding.enabled && binding.model_uid == model_uid)
    }
}

impl Default for DevinModelBinding {
    fn default() -> Self {
        Self::new("", "")
    }
}

fn default_true() -> bool {
    true
}

fn default_api_port() -> u16 {
    DEFAULT_DEVIN_API_PORT
}

fn default_inference_port() -> u16 {
    DEFAULT_DEVIN_INFERENCE_PORT
}

fn default_local_api_port() -> u16 {
    DEFAULT_DEVIN_LOCAL_API_PORT
}

fn default_upstream_api_url() -> String {
    DEFAULT_DEVIN_UPSTREAM_API_URL.to_owned()
}

/// Both prefixes Devin calls carry in the observation store. Producing and
/// recognising them must go through these constants: the two sides once drifted
/// apart (produced `devin-call:`, matched `devin:`), which left the Devin page
/// reporting that no conversation had ever reached the gateway.
pub const DEVIN_CALL_ID_PREFIX: &str = "devin-call:";
pub const DEVIN_EXECUTION_ID_PREFIX: &str = "devin:";

pub mod assignment;
pub mod catalog;
pub mod gateway;
pub mod host_config;
pub mod host_detect;
pub mod host_patch;
pub mod model_uids;
pub mod request;
pub mod response;
pub mod wire;

#[cfg(test)]
mod tests {
    use super::*;

    fn route(route_id: &str, model_hash: &str, enabled: bool) -> DevinRoute {
        DevinRoute {
            route_id: route_id.into(),
            model_hash: model_hash.into(),
            label: route_id.into(),
            enabled,
        }
    }

    fn settings_with_binding(binding: DevinModelBinding) -> DevinSettings {
        DevinSettings {
            bindings: vec![binding],
            ..DevinSettings::default()
        }
    }

    #[test]
    fn default_model_must_name_an_enabled_standard_binding() {
        let standard = DevinModelBinding::new("model-a", "hash-a");
        let mut compression = DevinModelBinding::new("compact", "hash-c");
        compression.kind = DevinBindingKind::ContextCompression;
        let base = DevinSettings {
            bindings: vec![standard, compression],
            ..DevinSettings::default()
        };

        let missing = DevinSettings {
            default_model_uid: Some("missing".into()),
            ..base.clone()
        };
        assert!(missing
            .validate()
            .unwrap_err()
            .to_string()
            .contains("Devin default model UID not found"));

        let compression = DevinSettings {
            default_model_uid: Some("compact".into()),
            ..base.clone()
        };
        assert!(compression
            .validate()
            .unwrap_err()
            .to_string()
            .contains("Devin default model must be a standard binding"));

        let mut disabled_binding = base.clone();
        disabled_binding.bindings[0].enabled = false;
        disabled_binding.default_model_uid = Some("model-a".into());
        assert!(disabled_binding
            .validate()
            .unwrap_err()
            .to_string()
            .contains("Devin default model must be enabled"));

        let valid = DevinSettings {
            default_model_uid: Some("model-a".into()),
            ..base
        };
        assert!(valid.validate().is_ok());
        assert_eq!(
            valid
                .default_binding()
                .map(|binding| binding.model_uid.as_str()),
            Some("model-a")
        );
    }

    #[test]
    fn enabled_settings_need_an_explicit_default_when_saved() {
        let base = DevinSettings {
            enabled: true,
            bindings: vec![DevinModelBinding::new("model-a", "hash-a")],
            ..DevinSettings::default()
        };
        assert!(base
            .validate_for_use()
            .unwrap_err()
            .to_string()
            .contains("Devin default model must be selected"));
        assert!(
            base.validate().is_ok(),
            "read-time validation stays repairable"
        );

        let disabled = DevinSettings {
            enabled: false,
            ..base.clone()
        };
        assert!(disabled.validate_for_use().is_ok());

        let selected = DevinSettings {
            default_model_uid: Some("model-a".into()),
            ..base
        };
        assert!(selected.validate_for_use().is_ok());
    }

    #[test]
    fn legacy_binding_json_keeps_the_legacy_hash_effective() {
        let binding: DevinModelBinding = serde_json::from_str(
            r#"{
                "model_uid": "devin-legacy",
                "model_hash": "legacy-hash",
                "display_name": "Legacy binding",
                "context_window_tokens": 200000,
                "enabled": true
            }"#,
        )
        .unwrap();

        assert_eq!(binding.kind, DevinBindingKind::Standard);
        assert!(binding.routes.is_empty());
        assert_eq!(binding.active_route_id, None);
        assert_eq!(binding.effective_model_hash(), "legacy-hash");
    }

    #[test]
    fn enabled_active_route_hash_is_effective() {
        let mut binding = DevinModelBinding::new("devin-model", "legacy-hash");
        binding.routes = vec![
            route("disabled-route", "disabled-hash", false),
            route("active-route", "active-hash", true),
        ];
        binding.active_route_id = Some("active-route".into());

        assert_eq!(binding.effective_model_hash(), "active-hash");

        binding.active_route_id = Some("disabled-route".into());
        assert_eq!(binding.effective_model_hash(), "legacy-hash");

        binding.active_route_id = Some("missing-route".into());
        assert_eq!(binding.effective_model_hash(), "legacy-hash");
    }

    #[test]
    fn route_state_round_trips_with_snake_case_json() {
        let mut binding = DevinModelBinding::new("devin-model", "legacy-hash");
        binding.kind = DevinBindingKind::ContextCompression;
        binding.routes = vec![route("active-route", "active-hash", true)];
        binding.active_route_id = Some("active-route".into());

        let value = serde_json::to_value(&binding).unwrap();

        assert_eq!(value["kind"], "context_compression");
        assert_eq!(value["routes"][0]["route_id"], "active-route");
        assert_eq!(value["routes"][0]["model_hash"], "active-hash");
        assert_eq!(value["active_route_id"], "active-route");
        assert_eq!(
            serde_json::from_value::<DevinModelBinding>(value).unwrap(),
            binding
        );
    }

    #[test]
    fn settings_reject_duplicate_route_ids() {
        let mut binding = DevinModelBinding::new("devin-model", "legacy-hash");
        binding.routes = vec![
            route("same-route", "hash-a", true),
            route("same-route", "hash-b", true),
        ];

        let error = settings_with_binding(binding)
            .validate()
            .unwrap_err()
            .to_string();

        assert!(error.contains("duplicate Devin route ID"));
    }

    #[test]
    fn settings_reject_empty_route_ids() {
        let mut binding = DevinModelBinding::new("devin-model", "legacy-hash");
        binding.routes = vec![route(" ", "hash-a", true)];

        let error = settings_with_binding(binding)
            .validate()
            .unwrap_err()
            .to_string();

        assert!(error.contains("Devin route ID must not be empty"));
    }

    #[test]
    fn settings_reject_empty_route_hashes() {
        let mut binding = DevinModelBinding::new("devin-model", "legacy-hash");
        binding.routes = vec![route("route-a", " ", true)];

        let error = settings_with_binding(binding)
            .validate()
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("must include a cursor-byok model hash"),
            "{error}"
        );
    }

    #[test]
    fn settings_reject_missing_active_routes() {
        let mut binding = DevinModelBinding::new("devin-model", "legacy-hash");
        binding.routes = vec![route("route-a", "hash-a", true)];
        binding.active_route_id = Some("missing-route".into());

        let error = settings_with_binding(binding)
            .validate()
            .unwrap_err()
            .to_string();

        assert!(error.contains("Devin active route not found"));
    }

    #[test]
    fn settings_reject_disabled_active_routes() {
        let mut binding = DevinModelBinding::new("devin-model", "legacy-hash");
        binding.routes = vec![route("route-a", "hash-a", false)];
        binding.active_route_id = Some("route-a".into());

        let error = settings_with_binding(binding)
            .validate()
            .unwrap_err()
            .to_string();

        assert!(error.contains("Devin active route must be enabled"));
    }

    #[test]
    fn context_compression_bindings_stay_single_line() {
        let mut binding = DevinModelBinding::new("devin-compaction", "hash-a");
        binding.kind = DevinBindingKind::ContextCompression;
        assert!(settings_with_binding(binding.clone()).validate().is_ok());

        binding.routes = vec![route("route-a", "hash-b", true)];
        binding.active_route_id = Some("route-a".into());

        let error = settings_with_binding(binding)
            .validate()
            .unwrap_err()
            .to_string();

        assert!(error.contains("must not define candidate routes"));
    }

    #[test]
    fn settings_keep_primary_hash_and_uid_validation() {
        let empty_hash = settings_with_binding(DevinModelBinding::new("devin-model", " "));
        assert!(empty_hash
            .validate()
            .unwrap_err()
            .to_string()
            .contains("must include a cursor-byok model hash"));

        let duplicate_uid = DevinSettings {
            bindings: vec![
                DevinModelBinding::new("devin-model", "hash-a"),
                DevinModelBinding::new("devin-model", "hash-b"),
            ],
            ..DevinSettings::default()
        };
        assert!(duplicate_uid
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate Devin model UID"));
    }

    #[test]
    fn normalization_trims_identifiers_hashes_and_route_state() {
        let binding = DevinModelBinding {
            model_uid: "  devin-model  ".into(),
            model_hash: "  primary-hash  ".into(),
            display_name: "  Display  ".into(),
            active_route_id: Some("  route-a  ".into()),
            routes: vec![DevinRoute {
                route_id: "  route-a  ".into(),
                model_hash: "  route-hash  ".into(),
                label: "  Route A  ".into(),
                enabled: true,
            }],
            ..DevinModelBinding::new("", "")
        };
        let settings = DevinSettings {
            auth_token: "  token  ".into(),
            upstream_api_url: "  https://server.codeium.com  ".into(),
            default_model_uid: Some("  devin-model  ".into()),
            bindings: vec![binding],
            ..Default::default()
        }
        .normalized();

        assert_eq!(settings.auth_token, "token");
        assert_eq!(settings.upstream_api_url, "https://server.codeium.com");
        assert_eq!(settings.default_model_uid.as_deref(), Some("devin-model"));
        let binding = settings.binding("devin-model").unwrap();
        assert_eq!(binding.model_uid, "devin-model");
        assert_eq!(binding.model_hash, "primary-hash");
        assert_eq!(binding.display_name, "Display");
        assert_eq!(binding.active_route_id.as_deref(), Some("route-a"));
        assert_eq!(binding.routes[0].route_id, "route-a");
        assert_eq!(binding.routes[0].model_hash, "route-hash");
        assert_eq!(binding.routes[0].label, "Route A");
        assert_eq!(binding.effective_model_hash(), "route-hash");
        assert!(settings.validate().is_ok());
    }
}
