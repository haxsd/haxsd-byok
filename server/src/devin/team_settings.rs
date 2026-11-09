use std::collections::HashSet;

use crate::{Error, Result};

use super::{
    directory::{is_native_model_uid, LocalModelEntry},
    wire::{self, Field, FieldValue},
};

const ALLOWED_MODEL_UID_FIELD: u32 = 7;

pub(super) fn with_local_models(payload: &[u8], entries: &[LocalModelEntry]) -> Result<Vec<u8>> {
    if entries.is_empty() {
        return Ok(payload.to_vec());
    }
    let fields = wire::parse_fields(payload)?;
    let mut allowed = HashSet::new();
    for field in wire::all_fields(&fields, ALLOWED_MODEL_UID_FIELD) {
        let FieldValue::Bytes(uid) = &field.value else {
            return Err(Error::Protocol(
                "Devin allowed model UID is not text".into(),
            ));
        };
        let uid = std::str::from_utf8(uid)
            .map_err(|_| Error::Protocol("Devin allowed model UID is not UTF-8".into()))?;
        if !uid.trim().is_empty() {
            allowed.insert(uid.to_owned());
        }
    }
    let mut output = payload.to_vec();
    for entry in entries {
        if !is_native_model_uid(&entry.uid) {
            return Err(Error::Protocol(
                "only namespaced local models may join the Devin model allowance".into(),
            ));
        }
        if allowed.insert(entry.uid.clone()) {
            output.extend(wire::serialize_fields(&[Field::bytes(
                ALLOWED_MODEL_UID_FIELD,
                &entry.uid,
            )])?);
        }
    }
    Ok(output)
}

#[cfg(test)]
pub(super) fn response_body(
    body: &[u8],
    content_type: &str,
    content_encoding: Option<&str>,
    entries: &[LocalModelEntry],
) -> Result<Vec<u8>> {
    wire::rewrite_response_body(body, content_type, content_encoding, |payload| {
        with_local_models(payload, entries)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<LocalModelEntry> {
        ["haxsd-byok:a", "haxsd-byok:b"]
            .into_iter()
            .map(|uid| LocalModelEntry {
                uid: uid.into(),
                display_name: uid.into(),
                model_hash: uid.into(),
                context_window_tokens: None,
                default: false,
                parameters: None,
                supports_images: true,
            })
            .collect()
    }

    #[test]
    fn preserves_official_bytes_and_appends_only_missing_local_uids() {
        let official = wire::serialize_fields(&[
            Field::bytes(7, "swe-1-6-slow"),
            Field::bytes(7, "haxsd-byok:a"),
            Field::varint(19, 2),
            Field::bytes(10, "Exec"),
            Field::fixed32(99, [1, 2, 3, 4]),
        ])
        .unwrap();
        let output = with_local_models(&official, &entries()).unwrap();
        let expected = [
            official.clone(),
            wire::serialize_fields(&[Field::bytes(7, "haxsd-byok:b")]).unwrap(),
        ]
        .concat();
        assert_eq!(output, expected);
        assert_eq!(with_local_models(&output, &entries()).unwrap(), output);
        assert_eq!(with_local_models(&official, &[]).unwrap(), official);
    }

    #[test]
    fn local_models_are_available_even_without_any_allowed_official_model() {
        for payload in [
            Vec::new(),
            wire::serialize_fields(&[Field::varint(1, 0)]).unwrap(),
        ] {
            let expected = [
                payload.clone(),
                wire::serialize_fields(&[
                    Field::bytes(7, "haxsd-byok:a"),
                    Field::bytes(7, "haxsd-byok:b"),
                ])
                .unwrap(),
            ]
            .concat();
            assert_eq!(with_local_models(&payload, &entries()).unwrap(), expected);
            assert_eq!(with_local_models(&payload, &[]).unwrap(), payload);
        }
    }

    #[test]
    fn local_allowance_does_not_add_official_model_privileges() {
        let official = wire::serialize_fields(&[Field::bytes(7, "swe-1-6-slow")]).unwrap();
        let mut local = entries();
        local[0].uid = "official-paid-model".into();
        assert!(with_local_models(&official, &local).is_err());
        assert!(with_local_models(&[0x38, 1], &entries()).is_err());
    }

    #[test]
    fn rewrites_raw_and_compressed_connect_responses_and_keeps_upstream_errors() {
        let official = wire::serialize_fields(&[Field::bytes(7, "swe-1-6-slow")]).unwrap();
        let expected = with_local_models(&official, &entries()).unwrap();
        for framed in [false, true] {
            for compressed in [false, true] {
                let body = if framed {
                    [
                        wire::frame(&official, compressed).unwrap(),
                        wire::end_frame(None),
                    ]
                    .concat()
                } else {
                    official.clone()
                };
                let body = wire::gzip(&body).unwrap();
                let content_type = if framed {
                    "application/connect+proto"
                } else {
                    "application/proto"
                };
                let output = response_body(&body, content_type, Some("gzip"), &entries()).unwrap();
                let decoded = wire::gunzip(&output).unwrap();
                let message = if framed {
                    let length = u32::from_be_bytes(decoded[1..5].try_into().unwrap()) as usize;
                    assert_eq!(&decoded[5 + length..], &wire::end_frame(None));
                    if compressed {
                        wire::gunzip(&decoded[5..5 + length]).unwrap()
                    } else {
                        decoded[5..5 + length].to_vec()
                    }
                } else {
                    decoded
                };
                assert_eq!(message, expected);
            }
        }
        let error = wire::end_frame(Some(wire::ConnectError {
            code: "permission_denied".into(),
            message: "official refusal".into(),
        }));
        assert_eq!(
            response_body(&error, "application/connect+proto", None, &entries()).unwrap(),
            error
        );
    }
}
