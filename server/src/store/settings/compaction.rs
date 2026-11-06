//! 上下文压缩策略：开关、摘要模型、输出与保留比例。

use serde::{Deserialize, Serialize};

use crate::store::{now_ms, Store};
use crate::Result;

use super::{
    COMPACTION_SETTINGS_KEY, MAX_RETAINED_TOKENS_MAX, RESERVE_PERCENT_DEFAULT,
    RESERVE_RETENTION_PERCENT_MAX, RETENTION_PERCENT_DEFAULT, SUMMARY_OUTPUT_TOKENS_MAX,
};

/// User-facing context-compaction policy.
///
/// Every field is optional with a deployment default so an older row (or no row
/// at all) keeps the shipped behaviour. The summary model is addressed by its
/// stable model hash; an empty hash means "reuse the conversation's model".
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct CompactionSettings {
    /// Run the proactive between-request pressure check and compaction.
    pub auto: bool,
    /// Model hash used for summarize calls only. Empty reuses the run model.
    pub summary_model_hash: String,
    /// Output cap for the summarize call; clamped by that model's own limit.
    pub summary_output_tokens: Option<u64>,
    /// Share of the window kept free for estimator drift and provider framing.
    pub reserve_percent: Option<u64>,
    /// Share of the window retained verbatim after a compaction.
    pub retention_percent: Option<u64>,
    /// Absolute ceiling on the retained tail; `None` keeps only the percentage.
    pub max_retained_tokens: Option<u64>,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            auto: true,
            summary_model_hash: String::new(),
            summary_output_tokens: None,
            reserve_percent: None,
            retention_percent: None,
            max_retained_tokens: None,
        }
    }
}

impl CompactionSettings {
    pub fn reserve_percent(&self) -> u64 {
        // Mirrors `run::compaction::CONTEXT_RESERVE_DIVISOR = 10`.
        self.reserve_percent.unwrap_or(RESERVE_PERCENT_DEFAULT)
    }

    pub fn retention_percent(&self) -> u64 {
        // Mirrors `run::compaction::RETAIN_WINDOW_DIVISOR = 5`.
        self.retention_percent.unwrap_or(RETENTION_PERCENT_DEFAULT)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(tokens) = self.summary_output_tokens {
            if tokens == 0 {
                return Err(crate::Error::Config(
                    "compaction.summary_output_tokens must be positive".into(),
                ));
            }
            if tokens > SUMMARY_OUTPUT_TOKENS_MAX {
                return Err(crate::Error::Config(format!(
                    "compaction.summary_output_tokens must be at most {SUMMARY_OUTPUT_TOKENS_MAX} tokens"
                )));
            }
        }
        if self.summary_model_hash.len() > 256 {
            return Err(crate::Error::Config(
                "compaction.summary_model_hash is too long".into(),
            ));
        }
        if let Some(percent) = self.reserve_percent {
            if !(1..=50).contains(&percent) {
                return Err(crate::Error::Config(
                    "compaction.reserve_percent must be between 1 and 50".into(),
                ));
            }
        }
        if let Some(percent) = self.retention_percent {
            if !(1..=50).contains(&percent) {
                return Err(crate::Error::Config(
                    "compaction.retention_percent must be between 1 and 50".into(),
                ));
            }
        }
        if let Some(tokens) = self.max_retained_tokens {
            if tokens == 0 {
                return Err(crate::Error::Config(
                    "compaction.max_retained_tokens must be positive".into(),
                ));
            }
            if tokens > MAX_RETAINED_TOKENS_MAX {
                return Err(crate::Error::Config(format!(
                    "compaction.max_retained_tokens must be at most {MAX_RETAINED_TOKENS_MAX} tokens"
                )));
            }
        }
        // The reserve and the retained tail are carved out of the same window.
        // Treating an unset field as its shipped default is what makes the rule
        // meaningful for the `null` the desktop card sends: 10 + 20 is fine,
        // 50 + 50 is not, and the message names the values actually in effect.
        let reserve = self.reserve_percent();
        let retention = self.retention_percent();
        if reserve + retention > RESERVE_RETENTION_PERCENT_MAX {
            return Err(crate::Error::Config(format!(
                "compaction.reserve_percent + compaction.retention_percent must be at most {RESERVE_RETENTION_PERCENT_MAX} (effective values: reserve {reserve}%, retention {retention}%; unset fields use {RESERVE_PERCENT_DEFAULT}% and {RETENTION_PERCENT_DEFAULT}%)"
            )));
        }
        Ok(())
    }
}

impl Store {
    /// Current context-compaction policy; a missing or unreadable row uses the
    /// shipped defaults, so a broken manual edit can never disable compaction by
    /// accident.
    ///
    /// The read side deliberately never fails: `validate` runs on write, and a
    /// row that no longer parses or no longer validates (external edit, or a
    /// row written by a build with different rules) falls back to the shipped
    /// defaults with a warning. Propagating the error would be unrecoverable
    /// rather than noisy: every run builds its budgets from this value and the
    /// settings page could not load the row it needs to replace.
    pub async fn compaction_settings(&self) -> Result<CompactionSettings> {
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(COMPACTION_SETTINGS_KEY)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match value {
            Some(value) => match serde_json::from_str::<CompactionSettings>(&value) {
                Ok(settings) => match settings.validate() {
                    Ok(()) => settings,
                    Err(error) => {
                        tracing::warn!(%error, "ignoring invalid compaction settings row");
                        CompactionSettings::default()
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "ignoring unreadable compaction settings row");
                    CompactionSettings::default()
                }
            },
            None => CompactionSettings::default(),
        })
    }

    pub async fn set_compaction_settings(
        &self,
        settings: CompactionSettings,
    ) -> Result<CompactionSettings> {
        let settings = CompactionSettings {
            auto: settings.auto,
            summary_model_hash: settings.summary_model_hash.trim().to_owned(),
            summary_output_tokens: settings.summary_output_tokens,
            reserve_percent: settings.reserve_percent,
            retention_percent: settings.retention_percent,
            max_retained_tokens: settings.max_retained_tokens,
        };
        settings.validate()?;
        let value_json = serde_json::to_string(&settings)?;
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(COMPACTION_SETTINGS_KEY)
        .bind(value_json)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(settings)
    }
}
