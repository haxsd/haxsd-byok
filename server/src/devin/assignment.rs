//! Bounded model assignment discovery for Devin's local protocol.
//!
//! Devin may send a model UID directly, or send an opaque assignment token
//! returned by AssignModel. This module keeps the token state local to the
//! gateway and deliberately does not implement the reference application's
//! commercial JWT or license behavior.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use axum::http::HeaderMap;
use uuid::Uuid;

use crate::{devin::wire, Error, Result};

pub const ASSIGNMENT_TTL: Duration = Duration::from_secs(12 * 60 * 60);
pub const MAX_SCAN_DEPTH: usize = 5;
pub const MAX_NESTED_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub uid: String,
    pub token: String,
    pub expires_at: SystemTime,
}

#[derive(Clone)]
pub struct AssignmentSessions {
    inner: Arc<Mutex<SessionState>>,
    ttl: Duration,
    clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
}

struct SessionState {
    tokens: HashMap<String, Assignment>,
}

impl Default for AssignmentSessions {
    fn default() -> Self {
        Self::new()
    }
}

impl AssignmentSessions {
    pub fn new() -> Self {
        Self::with_clock(ASSIGNMENT_TTL, SystemTime::now)
    }

    pub fn with_clock(
        ttl: Duration,
        clock: impl Fn() -> SystemTime + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SessionState {
                tokens: HashMap::new(),
            })),
            ttl,
            clock: Arc::new(clock),
        }
    }

    pub fn issue(&self, uid: &str) -> Assignment {
        let now = (self.clock)();
        let assignment = Assignment {
            uid: uid.to_owned(),
            token: Uuid::new_v4().to_string(),
            expires_at: now + self.ttl,
        };
        let mut state = self
            .inner
            .lock()
            .expect("assignment session mutex poisoned");
        prune(&mut state, now);
        state
            .tokens
            .insert(assignment.token.clone(), assignment.clone());
        assignment
    }

    pub fn uid_for_token(&self, token: &str) -> Option<String> {
        let now = (self.clock)();
        let mut state = self
            .inner
            .lock()
            .expect("assignment session mutex poisoned");
        prune(&mut state, now);
        state
            .tokens
            .get(token)
            .map(|assignment| assignment.uid.clone())
    }

    pub fn clear(&self) {
        self.inner
            .lock()
            .expect("assignment session mutex poisoned")
            .tokens
            .clear();
    }
}

fn prune(state: &mut SessionState, now: SystemTime) {
    state
        .tokens
        .retain(|_, assignment| assignment.expires_at > now);
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelReference {
    pub uid: String,
    pub source: String,
    pub path: Vec<u32>,
    pub candidates: Vec<String>,
    pub ambiguous: bool,
}

#[derive(Clone, Debug)]
struct Candidate {
    uid: String,
    source: &'static str,
    path: Vec<u32>,
    priority: usize,
}

pub fn find_model_reference(
    payload: &[u8],
    headers: &HeaderMap,
    candidates: &HashSet<String>,
    sessions: &AssignmentSessions,
) -> Result<ModelReference> {
    for value in headers.values() {
        let value = value
            .to_str()
            .map_err(|error| Error::Protocol(format!("invalid Devin header: {error}")))?;
        if let Some(uid) = sessions.uid_for_token(value) {
            return Ok(ModelReference {
                uid,
                source: "assignment-header".into(),
                ..ModelReference::default()
            });
        }
        if let Some(token) = value.strip_prefix("Bearer ") {
            if let Some(uid) = sessions.uid_for_token(token) {
                return Ok(ModelReference {
                    uid,
                    source: "assignment-bearer".into(),
                    ..ModelReference::default()
                });
            }
        }
    }

    let fields = wire::parse_fields(payload)?;
    let mut matches = Vec::new();
    scan_fields(
        &fields,
        candidates,
        sessions,
        0,
        &mut Vec::new(),
        &mut matches,
    );
    matches.sort_by_key(|candidate| (candidate.priority, candidate.path.len()));
    let Some(best) = matches.first() else {
        return Ok(ModelReference::default());
    };

    let mut peer_uids = matches
        .iter()
        .take_while(|candidate| candidate.priority == best.priority)
        .map(|candidate| candidate.uid.clone())
        .collect::<Vec<_>>();
    peer_uids.sort();
    peer_uids.dedup();
    if peer_uids.len() > 1 {
        return Ok(ModelReference {
            candidates: peer_uids,
            ambiguous: true,
            ..ModelReference::default()
        });
    }

    Ok(ModelReference {
        uid: best.uid.clone(),
        source: best.source.into(),
        path: best.path.clone(),
        candidates: peer_uids,
        ambiguous: false,
    })
}

fn scan_fields(
    fields: &[wire::Field],
    candidates: &HashSet<String>,
    sessions: &AssignmentSessions,
    depth: usize,
    path: &mut Vec<u32>,
    output: &mut Vec<Candidate>,
) {
    for field in fields {
        let wire::FieldValue::Bytes(value) = &field.value else {
            continue;
        };
        let text = String::from_utf8_lossy(value).into_owned();
        let match_info = if candidates.contains(&text) {
            Some((text, "uid"))
        } else {
            sessions.uid_for_token(&text).map(|uid| (uid, "assignment"))
        };
        if let Some((uid, source)) = match_info {
            output.push(Candidate {
                uid,
                source,
                path: {
                    let mut current = path.clone();
                    current.push(field.number);
                    current
                },
                priority: if depth == 0 && field.number == 21 {
                    0
                } else if depth == 0 {
                    10
                } else {
                    depth * 100 + 10
                },
            });
        }
        if depth < MAX_SCAN_DEPTH && !value.is_empty() && value.len() <= MAX_NESTED_BYTES {
            if let Ok(nested) = wire::parse_fields(value) {
                path.push(field.number);
                scan_fields(&nested, candidates, sessions, depth + 1, path, output);
                path.pop();
            }
        }
    }
}

pub fn assignment_token_response(assignment: &Assignment, harness_uid: &str) -> Result<Vec<u8>> {
    wire::serialize_fields(&[
        wire::Field::bytes(1, &assignment.token),
        wire::Field::bytes(2, harness_uid),
        wire::Field::bytes(3, &assignment.uid),
    ])
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
        time::{Duration, SystemTime},
    };

    use axum::http::HeaderMap;

    use super::*;

    #[test]
    fn assignment_tokens_expire_after_the_configured_ttl() {
        let now = Arc::new(Mutex::new(SystemTime::UNIX_EPOCH));
        let clock = Arc::clone(&now);
        let sessions =
            AssignmentSessions::with_clock(Duration::from_secs(60), move || *clock.lock().unwrap());
        let assignment = sessions.issue("model-a");
        assert_eq!(
            sessions.uid_for_token(&assignment.token).as_deref(),
            Some("model-a")
        );

        *now.lock().unwrap() += Duration::from_secs(61);
        assert_eq!(sessions.uid_for_token(&assignment.token), None);
    }

    #[test]
    fn finds_direct_model_uid_at_the_protocol_model_field() {
        let payload = wire::serialize_fields(&[wire::Field::bytes(21, "model-a")]).unwrap();
        let candidates = HashSet::from([String::from("model-a")]);
        let reference = find_model_reference(
            &payload,
            &HeaderMap::new(),
            &candidates,
            &AssignmentSessions::new(),
        )
        .unwrap();
        assert_eq!(reference.uid, "model-a");
        assert_eq!(reference.source, "uid");
        assert_eq!(reference.path, vec![21]);
    }

    #[test]
    fn rejects_ambiguous_nested_model_uids() {
        let nested = wire::serialize_fields(&[
            wire::Field::bytes(1, "model-a"),
            wire::Field::bytes(2, "model-b"),
        ])
        .unwrap();
        let payload = wire::serialize_fields(&[wire::Field::bytes(3, nested)]).unwrap();
        let candidates = HashSet::from([String::from("model-a"), String::from("model-b")]);
        let reference = find_model_reference(
            &payload,
            &HeaderMap::new(),
            &candidates,
            &AssignmentSessions::new(),
        )
        .unwrap();
        assert!(reference.ambiguous);
        assert_eq!(reference.candidates, vec!["model-a", "model-b"]);
    }

    #[test]
    fn assignment_header_wins_without_scanning_untrusted_payload() {
        let sessions = AssignmentSessions::new();
        let assignment = sessions.issue("model-b");
        let mut headers = HeaderMap::new();
        headers.insert("x-devin-assignment", assignment.token.parse().unwrap());
        let candidates = HashSet::from([String::from("model-a")]);
        let reference =
            find_model_reference(b"not-a-protobuf-payload", &headers, &candidates, &sessions)
                .unwrap();
        assert_eq!(reference.uid, "model-b");
        assert_eq!(reference.source, "assignment-header");
    }
}
