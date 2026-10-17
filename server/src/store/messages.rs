//! Persists canonical Messages and enforces event idempotency.
use sqlx::{Row, Sqlite, Transaction};

use crate::{
    model::{CanonicalMessage, ConversationId, MessageContent},
    Error, Result,
};

use super::{now_ms, Store};

impl Store {
    pub async fn message(
        &self,
        conversation_id: &ConversationId,
        message_id: &str,
    ) -> Result<Option<CanonicalMessage>> {
        let payload: Option<String> = sqlx::query_scalar(
            "SELECT payload_json FROM messages WHERE conversation_id = ? AND message_id = ?",
        )
        .bind(conversation_id.as_str())
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;
        payload
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }

    pub(crate) async fn put_message_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
        message: &CanonicalMessage,
    ) -> Result<()> {
        let payload = serde_json::to_string(message)?;
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO messages
             (conversation_id, message_id, role, origin, payload_json, runtime_event_id, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(conversation_id.as_str())
        .bind(&message.message_id)
        .bind(role_name(&message.role))
        .bind(origin_name(&message.origin))
        .bind(&payload)
        .bind(&message.runtime_event_id)
        .bind(now_ms())
        .execute(&mut **tx)
        .await?
        .rows_affected()
            == 1;
        if inserted {
            Self::register_message_image_tx(tx, conversation_id, message).await?;
            return Ok(());
        }

        let existing: Option<String> = sqlx::query_scalar(
            "SELECT payload_json FROM messages
             WHERE conversation_id = ? AND (message_id = ? OR runtime_event_id = ?)",
        )
        .bind(conversation_id.as_str())
        .bind(&message.message_id)
        .bind(&message.runtime_event_id)
        .fetch_optional(&mut **tx)
        .await?;
        match existing {
            Some(existing) if existing == payload => Ok(()),
            Some(_) => Err(Error::Store(format!(
                "message id or runtime event reused with different content: {}",
                message.message_id
            ))),
            None => Err(Error::Store(format!(
                "message insert was ignored without an existing object: {}",
                message.message_id
            ))),
        }
    }

    /// 登记消息里的工具图片，作为"这个内容块还在被会话使用"的一条钉子。
    ///
    /// 图片内容块（Read 工具读进来的图片）没有边、也不在 `conversation_blob_roots` 里，
    /// 回收趟过去只能每 60 秒扫一遍整张 `messages`、逐行 `json_extract` 才认得出它们还在用
    /// （库一大就长时间占住写锁）。引用在写消息时就是确定的，这里顺手记进
    /// `message_image_blobs`；存的是 payload 里那份 base64 文本，需要与 `blobs.blob_id`
    /// 比较的地方（回收趟、清理详细记录）在 Rust 里解一次。
    async fn register_message_image_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
        message: &CanonicalMessage,
    ) -> Result<()> {
        let Some(image) = message_image_blob_id(message) else {
            return Ok(());
        };
        sqlx::query(
            "INSERT OR IGNORE INTO message_image_blobs(conversation_id, blob_id) VALUES (?, ?)",
        )
        .bind(conversation_id.as_str())
        .bind(image)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub(crate) async fn load_checkpoint_messages_tx(
        tx: &mut Transaction<'_, Sqlite>,
        checkpoint_id: i64,
    ) -> Result<Vec<CanonicalMessage>> {
        let rows = sqlx::query(
            "WITH RECURSIVE lineage(checkpoint_id, parent_checkpoint_id, depth) AS (
                 SELECT checkpoint_id, parent_checkpoint_id, 0
                 FROM conversation_checkpoints WHERE checkpoint_id = ?
                 UNION ALL
                 SELECT r.checkpoint_id, r.parent_checkpoint_id, lineage.depth + 1
                 FROM conversation_checkpoints r
                 JOIN lineage ON r.checkpoint_id = lineage.parent_checkpoint_id
             )
             SELECT m.payload_json
             FROM lineage
             JOIN checkpoint_messages rm ON rm.checkpoint_id = lineage.checkpoint_id
             JOIN messages m
               ON m.conversation_id = rm.conversation_id AND m.message_id = rm.message_id
             ORDER BY lineage.depth DESC, rm.ordinal ASC",
        )
        .bind(checkpoint_id)
        .fetch_all(&mut **tx)
        .await?;
        rows.into_iter()
            .map(|row| serde_json::from_str(row.get::<&str, _>(0)).map_err(Into::into))
            .collect()
    }
}

fn role_name(role: &crate::model::Role) -> &'static str {
    match role {
        crate::model::Role::System => "system",
        crate::model::Role::User => "user",
        crate::model::Role::Assistant => "assistant",
        crate::model::Role::Tool => "tool",
    }
}

fn origin_name(origin: &crate::model::Origin) -> &'static str {
    match origin {
        crate::model::Origin::Prompt => "prompt",
        crate::model::Origin::User => "user",
        crate::model::Origin::Runtime => "runtime",
        crate::model::Origin::Assistant => "assistant",
        crate::model::Origin::Tool => "tool",
    }
}

/// 工具结果里的图片内容块（序列化路径是 `$.content.image.blob_id`）。
fn message_image_blob_id(message: &CanonicalMessage) -> Option<&str> {
    match &message.content {
        MessageContent::ToolResult(result) => {
            result.image.as_ref().map(|image| image.blob_id.as_str())
        }
        MessageContent::Parts { .. } | MessageContent::Assistant { .. } => None,
    }
}
