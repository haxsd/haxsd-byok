//! Enforces Conversation ownership during concurrent writes.
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};

use crate::{Error, Result};

use super::{now_ms, Store};

/// 读路径续期 `last_used_at_ms` 的最小间隔。比它更近的读不再写库：读一个内容块
/// 不等于要把它钉住，无根窗口本身就是"最近一小时"（见 `retention::UNROOTED_BLOB_MS`），
/// 一小时写一次已经足够让还在被读的内容块活过每一趟回收。
const BLOB_TOUCH_WINDOW_MS: i64 = 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlobId([u8; 32]);

impl BlobId {
    pub fn digest(data: &[u8]) -> Self {
        Self(Sha256::digest(data).into())
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let value: [u8; 32] = bytes.try_into().map_err(|_| {
            Error::Protocol(format!("BlobID must be 32 bytes, got {}", bytes.len()))
        })?;
        Ok(Self(value))
    }

    pub fn from_base64(value: &str) -> Result<Self> {
        let decoded = STANDARD
            .decode(value)
            .map_err(|error| Error::Protocol(format!("invalid BlobID base64: {error}")))?;
        Self::from_bytes(&decoded)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn to_base64(&self) -> String {
        STANDARD.encode(self.0)
    }
}

#[derive(Clone, Debug)]
pub struct BlobEdge {
    pub child: BlobId,
    pub field_name: String,
}

impl Store {
    pub async fn put_blob(&self, data: &[u8], edges: &[BlobEdge]) -> Result<BlobId> {
        let _write = self.writes.lock().await;
        let blob_id = BlobId::digest(data);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Self::put_blob_tx(&mut tx, &blob_id, data, edges).await?;
        tx.commit().await?;
        Ok(blob_id)
    }

    pub(crate) async fn put_blob_tx(
        tx: &mut Transaction<'_, Sqlite>,
        blob_id: &BlobId,
        data: &[u8],
        edges: &[BlobEdge],
    ) -> Result<()> {
        sqlx::query("INSERT INTO blobs(blob_id, data, created_at_ms, last_used_at_ms) VALUES (?, ?, ?, ?) ON CONFLICT(blob_id) DO UPDATE SET last_used_at_ms = excluded.last_used_at_ms")
            .bind(blob_id.as_bytes().as_slice())
            .bind(data)
            .bind(now_ms())
            .bind(now_ms())
            .execute(&mut **tx)
            .await?;
        for edge in edges {
            sqlx::query(
                "INSERT OR IGNORE INTO blob_edges(parent_blob_id, child_blob_id, field_name) VALUES (?, ?, ?)",
            )
            .bind(blob_id.as_bytes().as_slice())
            .bind(edge.child.as_bytes().as_slice())
            .bind(&edge.field_name)
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }

    /// 读一个内容块，顺便把它从无根窗口里救回来（见 `retention::UNROOTED_BLOB_MS`）。
    ///
    /// 读路径不进写锁：续期前先按行里的 `last_used_at_ms` 判断这次读是否真的会把窗口
    /// 推后，只有需要推后才进写锁执行那条带同样判据的 UPDATE。水化一条历史会话会按
    /// 消息逐个调这里，过去每读一个内容块就要拿一次进程级写锁并写一条语句。
    /// 用行里的时间戳判据（而不是进程内的去抖表）让"读得多"自然收敛到"每个内容块
    /// 一小时最多写一次"，也不用为进程寿命维护第二份状态。
    pub async fn get_blob(&self, blob_id: &BlobId) -> Result<Option<Vec<u8>>> {
        let row = sqlx::query("SELECT data, last_used_at_ms FROM blobs WHERE blob_id = ?")
            .bind(blob_id.as_bytes().as_slice())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let data: Vec<u8> = row.get(0);
        let last_used_at_ms: i64 = row.get(1);
        if last_used_at_ms < now_ms() - BLOB_TOUCH_WINDOW_MS {
            self.touch_blob(blob_id).await?;
        }
        Ok(Some(data))
    }

    /// 把 `last_used_at_ms` 推到当前时间。判据与 [`Self::get_blob`] 里读到的一致，
    /// 所以并发读同一个内容块时重复的那几次是无害的空写。
    async fn touch_blob(&self, blob_id: &BlobId) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query(
            "UPDATE blobs SET last_used_at_ms = ? WHERE blob_id = ? AND last_used_at_ms < ?",
        )
        .bind(now_ms())
        .bind(blob_id.as_bytes().as_slice())
        .bind(now_ms() - BLOB_TOUCH_WINDOW_MS)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    async fn temp_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        (directory, store)
    }

    /// 读一个刚写过的内容块不碰写锁。
    ///
    /// 水化一条历史会话会按消息逐个读内容块；拿锁的那一版把每个读都排到维护事务和
    /// Cursor 落库后面（见 `retention::prune_inactive_storage` 的写锁范围）。这里把写锁
    /// 先占住，再读一个窗口内的内容块：拿锁的实现会一直等下不去。
    #[tokio::test]
    async fn reading_a_fresh_blob_never_waits_for_the_write_coordinator() {
        let (_directory, store) = temp_store().await;
        let blob = store.put_blob(b"fresh", &[]).await.unwrap();
        let maintenance = store.writes.lock().await;
        let read = tokio::time::timeout(Duration::from_millis(250), store.get_blob(&blob)).await;
        drop(maintenance);
        assert!(read.is_ok(), "读取仍在续期窗口内的内容块不该等写锁");
        assert_eq!(read.unwrap().unwrap().unwrap(), b"fresh");
    }

    /// 读过窗口的内容块会把无根窗口推后（否则被取代的版本会在窗口结束时被回收），
    /// 但同一小时里的第二次读不再写库——"读"不等于"每读一次写一条 UPDATE"。
    #[tokio::test]
    async fn reading_an_expired_blob_renews_it_once_per_window() {
        let (_directory, store) = temp_store().await;
        let blob = store.put_blob(b"old", &[]).await.unwrap();
        sqlx::query("UPDATE blobs SET last_used_at_ms = 1")
            .execute(store.pool())
            .await
            .unwrap();

        assert_eq!(store.get_blob(&blob).await.unwrap().unwrap(), b"old");
        let renewed: i64 =
            sqlx::query_scalar("SELECT last_used_at_ms FROM blobs WHERE blob_id = ?")
                .bind(blob.as_bytes().as_slice())
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert!(
            renewed > now_ms() - BLOB_TOUCH_WINDOW_MS,
            "读过期的内容块必须把它的窗口推到当前时间"
        );

        let inside_window = now_ms() - BLOB_TOUCH_WINDOW_MS / 2;
        sqlx::query("UPDATE blobs SET last_used_at_ms = ?")
            .bind(inside_window)
            .execute(store.pool())
            .await
            .unwrap();
        let maintenance = store.writes.lock().await;
        let read = tokio::time::timeout(Duration::from_millis(250), store.get_blob(&blob)).await;
        drop(maintenance);
        assert!(read.is_ok(), "窗口内的重复读不该再写库");
        let untouched: i64 =
            sqlx::query_scalar("SELECT last_used_at_ms FROM blobs WHERE blob_id = ?")
                .bind(blob.as_bytes().as_slice())
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(
            untouched, inside_window,
            "窗口内的读不改写 last_used_at_ms，也不该拿写锁"
        );
    }
}
