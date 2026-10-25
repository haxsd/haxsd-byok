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

/// 会话令牌是 UUID 的字符串形态（36 字符，8-4-4-4-12）。
///
/// 先按形状过滤，只有像令牌的字段才去取会话锁（`uid_for_token` 会在锁里 prune 一遍）：
/// 以前每个 protobuf bytes 字段都会字符串化并进一次锁，而请求里绝大多数 bytes 是
/// 嵌套子消息或模型 UID。
fn looks_like_assignment_token(value: &[u8]) -> bool {
    value.len() == 36
        && value.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
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
        // 非 UTF-8 的 bytes 是嵌套子消息：它既不是 UID 也不是令牌，跳过即可
        // （以前这里先 `from_utf8_lossy` 分配一个字符串再比对，二者都不可能命中）。
        let Ok(text) = std::str::from_utf8(value) else {
            continue;
        };
        let match_info = if candidates.contains(text) {
            Some((text.to_owned(), "uid"))
        } else if looks_like_assignment_token(value) {
            sessions.uid_for_token(text).map(|uid| (uid, "assignment"))
        } else {
            None
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
    let model_assignment = wire::serialize_fields(&[
        wire::Field::bytes(1, &assignment.token),
        wire::Field::bytes(2, &assignment.uid),
        wire::Field::bytes(3, harness_uid),
    ])?;
    wire::serialize_fields(&[wire::Field::bytes(1, model_assignment)])
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

    fn string_field(fields: &[wire::Field], number: u32) -> String {
        fields
            .iter()
            .find_map(|field| match (&field.number, &field.value) {
                (field_number, wire::FieldValue::Bytes(value)) if *field_number == number => {
                    String::from_utf8(value.clone()).ok()
                }
                _ => None,
            })
            .unwrap_or_default()
    }

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

    /// 新版 `AssignModelResponse.assignment`（字段 1）是嵌套的
    /// `ModelAssignment`；令牌、模型 UID 与 harness UID 写在它自己的字段里。
    /// 平铺成响应顶层会让真实客户端解析失败，而旧测试只检查字节存在，发现不了。
    #[test]
    fn assignment_response_uses_the_nested_model_assignment_message() {
        let sessions = AssignmentSessions::new();
        let assignment = sessions.issue("model-a");
        let response = assignment_token_response(&assignment, "cursor-byok:model-a").unwrap();
        let outer = wire::parse_fields(&response).unwrap();
        assert_eq!(outer.len(), 1);
        assert_eq!(outer[0].number, 1);
        let wire::FieldValue::Bytes(inner) = &outer[0].value else {
            panic!("assignment must be a nested message");
        };
        let inner = wire::parse_fields(inner).unwrap();
        assert_eq!(string_field(&inner, 1), assignment.token);
        assert_eq!(string_field(&inner, 2), "model-a");
        assert_eq!(string_field(&inner, 3), "cursor-byok:model-a");
    }

    /// 只有 UUID 形状的字段才值得当令牌查一次会话表：模型 UID 与二进制子消息都不该进锁。
    #[test]
    fn only_uuid_shaped_fields_are_looked_up_as_tokens() {
        assert!(looks_like_assignment_token(
            b"2f1c8f7e-1c2b-4a5d-9e6f-0a1b2c3d4e5f"
        ));
        assert!(!looks_like_assignment_token(b"model-a"));
        assert!(!looks_like_assignment_token(&[0xff, 0xfe, 0x00, 0x01]));
        assert!(!looks_like_assignment_token(
            b"2f1c8f7e1c2b4a5d9e6f0a1b2c3d4e5f"
        ));
        assert!(!looks_like_assignment_token(
            b"2f1c8f7e-1c2b-4a5d-9e6f-0a1b2c3d4e5g"
        ));
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
