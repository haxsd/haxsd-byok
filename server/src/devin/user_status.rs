//! Additive model injection for Devin's native `GetUserStatus` picker.
//!
//! The official response stays authoritative for login, plan, quota and every
//! field we do not understand. Only `UserStatus.cascade_model_config_data`
//! gains reserved local model entries; official model entries and account
//! fields stay byte-for-byte at their protobuf level.

use crate::{Error, Result};

use super::{
    catalog,
    wire::{self, Field, FieldValue, MAX_BODY_SIZE},
};

const USER_STATUS_FIELD: u32 = 1;
const CASCADE_MODEL_CONFIG_DATA_FIELD: u32 = 33;

pub fn with_local_models(
    payload: &[u8],
    local_models: &[Field],
    local_default_uid: Option<&str>,
) -> Result<Vec<u8>> {
    if local_models.is_empty() && local_default_uid.is_none() {
        return Ok(payload.to_vec());
    }

    let mut response = wire::parse_fields(payload)?;
    let mut changed = false;
    for field in response
        .iter_mut()
        .filter(|field| field.number == USER_STATUS_FIELD)
    {
        let FieldValue::Bytes(user_bytes) = &field.value else {
            return Err(Error::Protocol("Devin user status is not a message".into()));
        };
        if user_bytes.is_empty() {
            continue;
        }
        let mut user = wire::parse_fields(user_bytes)?;
        let catalog_index = user
            .iter()
            .position(|field| field.number == CASCADE_MODEL_CONFIG_DATA_FIELD);
        let catalog = match catalog_index {
            Some(index) => match &user[index].value {
                FieldValue::Bytes(value) => value.as_slice(),
                _ => {
                    return Err(Error::Protocol(
                        "Devin cascade model config is not a message".into(),
                    ))
                }
            },
            None => &[],
        };
        let updated = catalog::with_local_models(catalog, local_models, local_default_uid, true)?;
        if updated == catalog {
            continue;
        }
        let catalog_field = Field::bytes(CASCADE_MODEL_CONFIG_DATA_FIELD, updated);
        match catalog_index {
            Some(index) => user[index] = catalog_field,
            None => user.push(catalog_field),
        }
        field.value = FieldValue::Bytes(wire::serialize_fields(&user)?);
        changed = true;
    }

    if !changed {
        return Ok(payload.to_vec());
    }
    let output = wire::serialize_fields(&response)?;
    if output.len() > MAX_BODY_SIZE {
        return Err(Error::Protocol("Devin user status exceeds 24 MiB".into()));
    }
    Ok(output)
}

pub fn response_body(
    body: &[u8],
    content_type: &str,
    content_encoding: Option<&str>,
    local_models: &[Field],
    local_default_uid: Option<&str>,
) -> Result<Vec<u8>> {
    wire::rewrite_response_body(body, content_type, content_encoding, |payload| {
        with_local_models(payload, local_models, local_default_uid)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devin::wire::{self, Field, FieldValue, MAX_BODY_SIZE};

    const OFFICIAL_UID: &str = "swe-1-6-slow";
    const LOCAL_A: &str = "haxsd-byok:model-a";
    const LOCAL_B: &str = "haxsd-byok:model-b";

    fn model_field(uid: &str, label: &str) -> Field {
        Field::bytes(
            1,
            wire::serialize_fields(&[Field::bytes(1, label), Field::bytes(22, uid)]).unwrap(),
        )
    }

    fn local_models() -> Vec<Field> {
        vec![
            model_field(LOCAL_A, "Local A"),
            model_field(LOCAL_B, "Local B"),
        ]
    }

    fn sorted_official_catalog() -> Vec<u8> {
        let group = Field::bytes(
            2,
            wire::serialize_fields(&[Field::bytes(1, ""), Field::bytes(2, "Official model")])
                .unwrap(),
        );
        let sort = Field::bytes(
            2,
            wire::serialize_fields(&[Field::bytes(1, "Recommended"), group]).unwrap(),
        );
        wire::serialize_fields(&[
            model_field(OFFICIAL_UID, "Official model"),
            sort,
            Field::bytes(
                3,
                wire::serialize_fields(&[Field::bytes(3, OFFICIAL_UID)]).unwrap(),
            ),
            Field::fixed32(99, [1, 2, 3, 4]),
        ])
        .unwrap()
    }

    fn payload_with_sorted_catalog() -> Vec<u8> {
        let account = wire::serialize_fields(&[
            Field::varint(1, 0),
            Field::bytes(13, "real-plan"),
            Field::bytes(33, sorted_official_catalog()),
            Field::fixed32(91, [9, 8, 7, 6]),
        ])
        .unwrap();
        wire::serialize_fields(&[Field::bytes(1, account), Field::bytes(2, "real-plan-info")])
            .unwrap()
    }

    fn user_fields(payload: &[u8]) -> Vec<Field> {
        let response = wire::parse_fields(payload).unwrap();
        let FieldValue::Bytes(user) = &wire::first_field(&response, 1).unwrap().value else {
            panic!("user status must be a message")
        };
        wire::parse_fields(user).unwrap()
    }

    fn catalog_fields(payload: &[u8]) -> Vec<Field> {
        let user = user_fields(payload);
        let FieldValue::Bytes(catalog) = &wire::first_field(&user, 33).unwrap().value else {
            panic!("catalog must be a message")
        };
        wire::parse_fields(catalog).unwrap()
    }

    fn model_uids(catalog: &[Field]) -> Vec<String> {
        wire::all_fields(catalog, 1)
            .map(|field| {
                let FieldValue::Bytes(model) = &field.value else {
                    panic!("model must be a message")
                };
                let model = wire::parse_fields(model).unwrap();
                let FieldValue::Bytes(uid) = &wire::first_field(&model, 22).unwrap().value else {
                    panic!("UID must be bytes")
                };
                String::from_utf8(uid.clone()).unwrap()
            })
            .collect()
    }

    fn sort_labels(catalog: &[Field]) -> Vec<String> {
        let FieldValue::Bytes(sort) = &wire::first_field(catalog, 2).unwrap().value else {
            panic!("sort must be a message")
        };
        let sort = wire::parse_fields(sort).unwrap();
        let FieldValue::Bytes(group) = &wire::first_field(&sort, 2).unwrap().value else {
            panic!("group must be a message")
        };
        let group = wire::parse_fields(group).unwrap();
        wire::all_fields(&group, 2)
            .map(|field| {
                let FieldValue::Bytes(label) = &field.value else {
                    panic!("label must be bytes")
                };
                String::from_utf8(label.clone()).unwrap()
            })
            .collect()
    }

    #[test]
    fn official_fields_survive_and_local_models_join_the_default_group() {
        let payload = payload_with_sorted_catalog();
        let output = with_local_models(&payload, &local_models(), Some(LOCAL_A)).unwrap();

        let before = wire::parse_fields(&payload).unwrap();
        let after = wire::parse_fields(&output).unwrap();
        assert_eq!(
            after
                .iter()
                .filter(|field| field.number != 1)
                .cloned()
                .collect::<Vec<_>>(),
            before
                .iter()
                .filter(|field| field.number != 1)
                .cloned()
                .collect::<Vec<_>>()
        );
        let original_user = user_fields(&payload);
        let rewritten_user = user_fields(&output);
        assert_eq!(
            rewritten_user
                .iter()
                .filter(|field| field.number != 33)
                .cloned()
                .collect::<Vec<_>>(),
            original_user
                .iter()
                .filter(|field| field.number != 33)
                .cloned()
                .collect::<Vec<_>>(),
            "account, plan and unknown user fields must stay untouched"
        );
        assert_eq!(
            model_uids(&catalog_fields(&output)),
            vec![OFFICIAL_UID, LOCAL_A, LOCAL_B]
        );
        assert_eq!(
            sort_labels(&catalog_fields(&output)),
            vec!["Official model", "Local A", "Local B"]
        );
        assert_eq!(
            wire::first_field(&catalog_fields(&output), 99),
            wire::first_field(&catalog_fields(&payload), 99)
        );
    }

    #[test]
    fn local_sort_injection_is_idempotent_and_can_create_an_all_group() {
        let payload = payload_with_sorted_catalog();
        let once = with_local_models(&payload, &local_models(), Some(LOCAL_A)).unwrap();
        let twice = with_local_models(&once, &local_models(), Some(LOCAL_A)).unwrap();
        assert_eq!(twice, once);

        let account = wire::serialize_fields(&[Field::varint(1, 0)]).unwrap();
        let unsorted = wire::serialize_fields(&[Field::bytes(1, account)]).unwrap();
        let output = with_local_models(&unsorted, &local_models(), None).unwrap();
        assert_eq!(model_uids(&catalog_fields(&output)), vec![LOCAL_A, LOCAL_B]);
        assert_eq!(
            sort_labels(&catalog_fields(&output)),
            vec!["Local A", "Local B"]
        );
    }

    #[test]
    fn conflicting_labels_skip_only_the_conflicting_local_model() {
        let payload = payload_with_sorted_catalog();
        // 展示名撞了官方模型：跳过这一个，官方目录和其余本地模型照常可用。
        let official =
            with_local_models(&payload, &[model_field(LOCAL_A, "Official model")], None).unwrap();
        assert_eq!(model_uids(&catalog_fields(&official)), vec![OFFICIAL_UID]);

        // 两个本地模型同名：保留第一个，跳过第二个，不能整份目录失败。
        let duplicate = with_local_models(
            &payload,
            &[model_field(LOCAL_A, "Same"), model_field(LOCAL_B, "Same")],
            None,
        )
        .unwrap();
        assert_eq!(
            model_uids(&catalog_fields(&duplicate)),
            vec![OFFICIAL_UID, LOCAL_A]
        );
    }

    #[test]
    fn a_missing_user_status_is_not_fabricated() {
        for payload in [
            Vec::new(),
            wire::serialize_fields(&[Field::bytes(2, "upstream-error")]).unwrap(),
            wire::serialize_fields(&[Field::bytes(1, [])]).unwrap(),
        ] {
            assert_eq!(
                with_local_models(&payload, &local_models(), None).unwrap(),
                payload
            );
        }
    }

    #[test]
    fn response_rewriting_preserves_http_and_connect_compression() {
        let payload = payload_with_sorted_catalog();
        for framed in [false, true] {
            for frame_gzip in [false, true] {
                for http_gzip in [false, true] {
                    let content_type = if framed {
                        "application/connect+proto"
                    } else {
                        "application/proto"
                    };
                    let mut body = if framed {
                        let mut data = wire::frame(&payload, frame_gzip).unwrap();
                        data.extend_from_slice(&wire::end_frame(None));
                        data
                    } else {
                        payload.clone()
                    };
                    if http_gzip {
                        body = wire::gzip(&body).unwrap();
                    }
                    let output = response_body(
                        &body,
                        content_type,
                        http_gzip.then_some("gzip"),
                        &local_models(),
                        Some(LOCAL_A),
                    )
                    .unwrap();
                    let output = if http_gzip {
                        wire::gunzip(&output).unwrap()
                    } else {
                        output
                    };
                    let response = if framed {
                        assert_eq!(output[0], u8::from(frame_gzip));
                        let length = u32::from_be_bytes(output[1..5].try_into().unwrap()) as usize;
                        assert_eq!(&output[5 + length..], &wire::end_frame(None));
                        wire::unwrap_request(&output[..5 + length], None, None)
                            .unwrap()
                            .payload
                    } else {
                        output
                    };
                    assert_eq!(
                        model_uids(&catalog_fields(&response)),
                        vec![OFFICIAL_UID, LOCAL_A, LOCAL_B]
                    );
                }
            }
        }
    }

    #[test]
    fn upstream_connect_errors_remain_unchanged() {
        let body = wire::end_frame(Some(wire::ConnectError {
            code: "unauthenticated".into(),
            message: "sign in required".into(),
        }));
        assert_eq!(
            response_body(
                &body,
                "application/connect+proto",
                None,
                &local_models(),
                None,
            )
            .unwrap(),
            body
        );
    }

    #[test]
    fn invalid_frames_unknown_types_and_oversized_gzip_are_rejected() {
        let payload = wire::gzip(&vec![0; MAX_BODY_SIZE + 1]).unwrap();
        assert!(response_body(
            &payload,
            "application/proto",
            Some("gzip"),
            &local_models(),
            None,
        )
        .is_err());
        for body in [
            vec![0, 0],
            vec![0, 0, 0, 0, 3, 1],
            vec![4, 0, 0, 0, 0],
            wire::frame(&payload_with_sorted_catalog(), false).unwrap(),
        ] {
            assert!(response_body(
                &body,
                "application/connect+proto",
                None,
                &local_models(),
                None,
            )
            .is_err());
        }
        assert!(response_body(
            &payload_with_sorted_catalog(),
            "application/json",
            None,
            &local_models(),
            None,
        )
        .is_err());
    }
}
