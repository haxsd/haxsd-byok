use sqlx::Row;

use crate::{model::ModelSpec, Result};

use super::{now_ms, Store};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DevinSessionModel {
    pub model_uid: String,
    pub model: ModelSpec,
}

impl Store {
    pub(crate) async fn devin_session_model(
        &self,
        session_id: &str,
    ) -> Result<Option<DevinSessionModel>> {
        let row = sqlx::query(
            "SELECT model_uid, model_spec_json FROM devin_session_models WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(DevinSessionModel {
                model_uid: row.get(0),
                model: serde_json::from_str(row.get::<&str, _>(1))?,
            })
        })
        .transpose()
    }

    pub(crate) async fn set_devin_session_model(
        &self,
        session_id: &str,
        selection: Option<&DevinSessionModel>,
    ) -> Result<()> {
        if session_id.is_empty() {
            return Ok(());
        }
        let _write = self.writes.lock().await;
        match selection {
            Some(selection) => {
                if !self.devin_settings().await?.enabled {
                    return Ok(());
                }
                sqlx::query(
                    "INSERT INTO devin_session_models
                     (session_id, model_uid, model_spec_json, updated_at_ms)
                     VALUES (?, ?, ?, ?)
                     ON CONFLICT(session_id) DO UPDATE SET
                     model_uid = excluded.model_uid,
                     model_spec_json = excluded.model_spec_json,
                     updated_at_ms = excluded.updated_at_ms",
                )
                .bind(session_id)
                .bind(&selection.model_uid)
                .bind(serde_json::to_string(&selection.model)?)
                .bind(now_ms())
                .execute(&self.pool)
                .await?;
            }
            None => {
                sqlx::query("DELETE FROM devin_session_models WHERE session_id = ?")
                    .bind(session_id)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devin::DevinSettings;

    #[tokio::test]
    async fn disabling_takeover_clears_session_ownership_before_reenabling() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("disable.db").display()
        ))
        .await
        .unwrap();
        let settings = DevinSettings {
            enabled: true,
            ..DevinSettings::default()
        };
        store.set_devin_settings(settings.clone()).await.unwrap();
        let selection = DevinSessionModel {
            model_uid: "haxsd-byok:sol~low~128000".into(),
            model: ModelSpec::new("sol-hash"),
        };
        store
            .set_devin_session_model("sol-session", Some(&selection))
            .await
            .unwrap();
        store
            .set_devin_settings(DevinSettings {
                enabled: false,
                ..settings.clone()
            })
            .await
            .unwrap();
        store
            .set_devin_session_model("sol-session", Some(&selection))
            .await
            .unwrap();
        store.set_devin_settings(settings).await.unwrap();
        assert!(store
            .devin_session_model("sol-session")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn session_schema_upgrades_existing_databases_without_changing_settings() {
        use sqlx::{migrate::Migrator, sqlite::SqlitePoolOptions};
        use std::borrow::Cow;

        static MIGRATIONS: Migrator = sqlx::migrate!("./migrations");
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("upgrade.db").display()
        );
        let pool = SqlitePoolOptions::new()
            .connect(&database_url)
            .await
            .unwrap();
        Migrator {
            migrations: Cow::Owned(
                MIGRATIONS
                    .iter()
                    .filter(|migration| migration.version < 26)
                    .cloned()
                    .collect(),
            ),
            ..Migrator::DEFAULT
        }
        .run(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO service_settings (setting_key, value_json, updated_at_ms)
             VALUES ('devin_router', '{\"enabled\":true}', 123)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
        let store = Store::connect(&database_url).await.unwrap();
        assert!(store.devin_settings().await.unwrap().enabled);
        let selection = DevinSessionModel {
            model_uid: "haxsd-byok:sol".into(),
            model: ModelSpec::new("sol-hash"),
        };
        store
            .set_devin_session_model("sol-session", Some(&selection))
            .await
            .unwrap();
        assert_eq!(
            store.devin_session_model("sol-session").await.unwrap(),
            Some(selection)
        );
        let updated_at: i64 = sqlx::query_scalar(
            "SELECT updated_at_ms FROM service_settings WHERE setting_key = 'devin_router'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(updated_at, 123);
    }
}
