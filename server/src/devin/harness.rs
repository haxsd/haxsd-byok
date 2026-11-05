use std::collections::HashMap;

use crate::Result;

use super::{
    catalog::client_model_uid,
    directory::is_native_model_uid,
    wire::{self, Field, FieldValue},
};

#[derive(Default)]
pub(super) struct HarnessCatalog {
    models: HashMap<String, Vec<(bool, Vec<String>)>>,
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
            if harnesses.is_empty() {
                continue;
            }
            let metadata = message(&model, 30)?;
            let default = boolean(&model, 31) || boolean(&metadata, 3);
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
                    .push((default, harnesses.clone()));
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
        let harnesses = self.resolve(&model_id);
        info.retain(|field| field.number != 20);
        info.extend(harnesses.into_iter().map(|uid| Field::bytes(20, uid)));
        info_field.value = FieldValue::Bytes(wire::serialize_fields(&info)?);
        Ok(Field::bytes(1, wire::serialize_fields(&model)?))
    }

    fn resolve(&self, model_id: &str) -> Vec<String> {
        let Some(profiles) = self.models.get(&normalize(model_id)) else {
            return Vec::new();
        };
        let defaults = profiles
            .iter()
            .filter(|(default, _)| *default)
            .collect::<Vec<_>>();
        let profiles = if defaults.is_empty() {
            profiles.iter().collect::<Vec<_>>()
        } else {
            defaults
        };
        let Some((_, first)) = profiles.first() else {
            return Vec::new();
        };
        if profiles.iter().any(|(_, harnesses)| harnesses != first) {
            return Vec::new();
        }
        first.clone()
    }
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
