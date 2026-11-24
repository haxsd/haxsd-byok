//! Stores the image-description fallback and its cached results.

use serde::{Deserialize, Serialize};

use crate::{
    store::{now_ms, Store},
    Error, Result,
};

const VISION_SETTINGS_KEY: &str = "vision";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum VisionSettings {
    #[default]
    Disabled,
    Model {
        /// Stable model hash from the model library.
        model_hash: String,
    },
}

impl Store {
    pub async fn vision_settings(&self) -> Result<VisionSettings> {
        let row: Option<String> =
            sqlx::query_scalar("SELECT value_json FROM service_settings WHERE setting_key = ?")
                .bind(VISION_SETTINGS_KEY)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row
            .map(|json| serde_json::from_str(&json))
            .transpose()?
            .unwrap_or_default())
    }

    pub async fn set_vision_settings(&self, settings: &VisionSettings) -> Result<VisionSettings> {
        if let VisionSettings::Model { model_hash } = settings {
            if model_hash.trim().is_empty() {
                return Err(Error::Config("vision model is empty".into()));
            }
        }
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) \
             ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(VISION_SETTINGS_KEY)
        .bind(serde_json::to_string(settings)?)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(settings.clone())
    }

    pub(crate) async fn image_description(
        &self,
        conversation_id: &str,
        message_id: &str,
        image_digest: &str,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT description FROM image_descriptions \
             WHERE conversation_id = ? AND message_id = ? AND image_digest = ?",
        )
        .bind(conversation_id)
        .bind(message_id)
        .bind(image_digest)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub(crate) async fn save_image_description(
        &self,
        conversation_id: &str,
        message_id: &str,
        image_digest: &str,
        description: &str,
    ) -> Result<String> {
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO image_descriptions(conversation_id, message_id, image_digest, description) \
             VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(conversation_id)
        .bind(message_id)
        .bind(image_digest)
        .bind(description)
        .execute(&self.pool)
        .await?;
        self.image_description(conversation_id, message_id, image_digest)
            .await?
            .ok_or_else(|| Error::Store("saved image description is missing".into()))
    }
}
