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
pub const DEFAULT_DEVIN_UPSTREAM_API_URL: &str = "https://server.self-serve.windsurf.com";

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
            .map(|route| route.model_hash.as_str())
            .unwrap_or(&self.model_hash)
    }
}

impl DevinSettings {
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
        Ok(())
    }

    pub fn binding(&self, model_uid: &str) -> Option<&DevinModelBinding> {
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

pub mod assignment;
pub mod catalog;
pub mod gateway;
pub mod host_detect;
pub mod host_patch;
pub mod host_status;
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

        assert!(error.contains("must include a cursor-byok model hash"));
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
}
