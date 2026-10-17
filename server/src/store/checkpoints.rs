//! Persists immutable internal Conversation checkpoints.
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};

use crate::{
    model::{CanonicalMessage, CheckpointId, ConversationId, RunId},
    Error, Result,
};

use super::{now_ms, Store};

impl Store {
    pub async fn ensure_conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<CheckpointId> {
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let checkpoint = Self::ensure_conversation_tx(&mut tx, conversation_id).await?;
        tx.commit().await?;
        Ok(checkpoint)
    }

    pub async fn load_checkpoint_messages(
        &self,
        checkpoint_id: CheckpointId,
    ) -> Result<Vec<CanonicalMessage>> {
        let mut tx = self.pool.begin().await?;
        let messages = Self::load_checkpoint_messages_tx(&mut tx, checkpoint_id.0).await?;
        tx.commit().await?;
        Ok(messages)
    }

    pub async fn checkpoint_parent(
        &self,
        checkpoint_id: CheckpointId,
    ) -> Result<Option<CheckpointId>> {
        let parent = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT parent_checkpoint_id FROM conversation_checkpoints WHERE checkpoint_id = ?",
        )
        .bind(checkpoint_id.0)
        .fetch_optional(&self.pool)
        .await?
        .flatten()
        .map(CheckpointId);
        Ok(parent)
    }

    pub async fn load_current_messages(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Vec<CanonicalMessage>> {
        let Some(checkpoint_id) = sqlx::query_scalar::<_, i64>(
            "SELECT current_checkpoint_id FROM conversations WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(Vec::new());
        };
        self.load_checkpoint_messages(CheckpointId(checkpoint_id))
            .await
    }

    pub async fn match_checkpoint_prefix(
        &self,
        conversation_id: &ConversationId,
        base_checkpoint_id: CheckpointId,
        additions: &[CanonicalMessage],
    ) -> Result<(CheckpointId, usize)> {
        let mut checkpoint = base_checkpoint_id;
        // 每追加一条只算"父摘要 + 这一条"：摘要的定义就是 `chained_digest(父摘要, 本
        // checkpoint 的新增)`，所以这里不必把整条链重新序列化、重算一次全量摘要，也不必
        // 把子 checkpoint 的整条链装载回来比内容（审计 05/P2-4）。子节点的定位本来就是
        // "父 + 摘要"，摘要覆盖了子链的全部字节，装载回来比内容不会多拦下任何东西。
        let mut parent_digest = self.checkpoint_digest(checkpoint).await?;
        for (index, addition) in additions.iter().enumerate() {
            let digest = chained_digest(&parent_digest, std::slice::from_ref(addition))?;
            let child = sqlx::query_scalar::<_, i64>(
                "SELECT checkpoint_id FROM conversation_checkpoints
                 WHERE conversation_id = ? AND parent_checkpoint_id = ? AND state_digest = ?",
            )
            .bind(conversation_id.as_str())
            .bind(checkpoint.0)
            .bind(digest.as_slice())
            .fetch_optional(&self.pool)
            .await?
            .map(CheckpointId);
            let Some(child) = child else {
                return Ok((checkpoint, index));
            };
            checkpoint = child;
            parent_digest = digest.to_vec();
        }
        Ok((checkpoint, additions.len()))
    }

    pub async fn import_checkpoint(
        &self,
        conversation_id: &ConversationId,
        messages: &[CanonicalMessage],
    ) -> Result<CheckpointId> {
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = Self::ensure_conversation_tx(&mut tx, conversation_id).await?;
        // 先把本地链读出来：父节点（摘要从哪儿接）与"客户端这段是不是已经存下的那段"
        // 都要用它。摘要是链式的，所以不能像过去那样先按"整条历史的摘要"查一次去重
        // ——同一个父加同一批新增才会得到同一个摘要，而这正是去重要回答的问题。
        let current_messages = Self::load_checkpoint_messages_tx(&mut tx, current.0).await?;
        let (parent, additions) = if extends_prefix(&current_messages, messages) {
            (current, &messages[current_messages.len()..])
        } else {
            // 客户端回带的历史与本地链对不上(例如客户端自己重投影过):把它的整段
            // 历史挂在会话的根下面,保持"一个会话一个根"。
            (
                Self::conversation_root_tx(&mut tx, conversation_id).await?,
                messages,
            )
        };
        // 客户端回带的历史正好等于本地链：没有新增就不该多插一个空节点。过去这里照样
        // `insert_checkpoint_tx(current, [], digest)`，于是每轮多一层空节点、一次重投叠
        // 一层（审计 05/P2-4 的残留）；`append_checkpoint` 早就有同规则的早退。
        //
        // 只限"父节点就是当前链尾"这一种空新增。另一种空新增落在**分叉**分支（客户端
        // 回带的历史对不上本地链、且过滤掉系统提示根之后一条不剩）：那时必须真的建一个
        // "链为空"的 checkpoint——它是这个客户端视角的基线，`error_lifecycle` 的
        // `newer_run_request_on_one_bidi_stream_replaces_the_active_run` 钉住的就是
        // "新请求只看到裁剪后的历史"；直接返回 `current` 会让新 run 带上旧 run 的消息。
        if additions.is_empty() && parent == current {
            tx.commit().await?;
            return Ok(current);
        }
        let parent_digest = Self::checkpoint_digest_tx(&mut tx, parent).await?;
        let digest = chained_digest(&parent_digest, additions)?;
        if let Some(existing) = sqlx::query_scalar::<_, i64>(
            "SELECT checkpoint_id FROM conversation_checkpoints
             WHERE conversation_id = ? AND state_digest = ?",
        )
        .bind(conversation_id.as_str())
        .bind(digest.as_slice())
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return Ok(CheckpointId(existing));
        }

        let checkpoint =
            Self::insert_checkpoint_tx(&mut tx, conversation_id, parent, additions, digest).await?;
        tx.commit().await?;
        Ok(checkpoint)
    }

    pub async fn append_checkpoint(
        &self,
        conversation_id: &ConversationId,
        run_id: &RunId,
        expected: CheckpointId,
        additions: &[CanonicalMessage],
    ) -> Result<CheckpointId> {
        if additions.is_empty() {
            return Ok(expected);
        }
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // 父摘要一行就够：链式摘要只依赖父摘要与本次新增，不必再装载整条历史
        // （审计 05/P2-4：每条助手消息都整链重算一次）。
        let parent_digest = Self::checkpoint_digest_tx(&mut tx, expected).await?;
        let digest = chained_digest(&parent_digest, additions)?;
        let checkpoint = Self::append_checkpoint_with_digest_tx(
            &mut tx,
            conversation_id,
            run_id,
            expected,
            additions,
            digest,
        )
        .await?;
        tx.commit().await?;
        Ok(checkpoint)
    }

    pub async fn replace_checkpoint(
        &self,
        conversation_id: &ConversationId,
        run_id: &RunId,
        expected: CheckpointId,
        messages: &[CanonicalMessage],
    ) -> Result<CheckpointId> {
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Self::require_active_head_tx(&mut tx, conversation_id, run_id, expected).await?;
        let root = Self::conversation_root_tx(&mut tx, conversation_id).await?;
        // 整段替换（压缩、客户端投影）挂回会话根，摘要同样从根接起：链式摘要下
        // "这条链是从根长出来的"与 `conversation_checkpoints.parent_checkpoint_id` 恒等。
        let root_digest = Self::checkpoint_digest_tx(&mut tx, root).await?;
        let digest = chained_digest(&root_digest, messages)?;
        let checkpoint =
            Self::insert_checkpoint_tx(&mut tx, conversation_id, root, messages, digest).await?;
        let updated = sqlx::query(
            "UPDATE conversations SET current_checkpoint_id = ?, updated_at_ms = ?
             WHERE conversation_id = ? AND current_checkpoint_id = ? AND active_run_id = ?",
        )
        .bind(checkpoint.0)
        .bind(now_ms())
        .bind(conversation_id.as_str())
        .bind(expected.0)
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            return Err(Error::Store(format!(
                "lost active ownership while replacing checkpoint for run {run_id}"
            )));
        }
        sqlx::query("UPDATE runs SET head_checkpoint_id = ?, updated_at_ms = ? WHERE run_id = ?")
            .bind(checkpoint.0)
            .bind(now_ms())
            .bind(run_id.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(checkpoint)
    }

    pub async fn append_message_once(
        &self,
        conversation_id: &ConversationId,
        run_id: &RunId,
        expected: CheckpointId,
        message: &CanonicalMessage,
    ) -> Result<(CheckpointId, bool)> {
        let mut appended = self
            .append_messages_once(
                conversation_id,
                run_id,
                expected,
                std::slice::from_ref(message),
            )
            .await?;
        Ok(appended.pop().expect("one message appended"))
    }

    /// 一批消息一次提交：装载一次链、一个写锁、一个事务，每条的 checkpoint 与摘要语义不变。
    ///
    /// 返回每条消息对应的 `(checkpoint, 是否新插入)`，与逐条调用 `append_message_once`
    /// 同形（重复身份的消息返回它当前所在的 checkpoint，不写库、不报错）。协议形态因此
    /// 不变：调用方仍然每条新消息各发一个提交事件，省掉的是每条一次的整链装载与
    /// `chain_message` 查询、一次写锁、一个事务——一批 N 条消息的开销从 O(N×历史) 降到
    /// O(历史 + N)（审计 09/F2）。
    ///
    /// 批内任意一条报冲突（同一身份不同内容）时整批回滚：调用方是拿到返回值之后才发事件
    /// 的，"没提交"与"没宣布"一致，不会留下半批。
    pub async fn append_messages_once(
        &self,
        conversation_id: &ConversationId,
        run_id: &RunId,
        expected: CheckpointId,
        messages: &[CanonicalMessage],
    ) -> Result<Vec<(CheckpointId, bool)>> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        // 一次装载：只要链上已有的身份（正文留给真正撞上身份的那一条去取）。
        let mut known = self.chain_identities(conversation_id, expected).await?;
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Self::require_active_head_tx(&mut tx, conversation_id, run_id, expected).await?;
        let mut parent_digest = Self::checkpoint_digest_tx(&mut tx, expected).await?;
        let mut head = expected;
        let mut appended = Vec::with_capacity(messages.len());
        for message in messages {
            if let Some(existing) = known
                .iter()
                .find(|known| known.matches(message))
                .map(|known| known.body)
            {
                let stored = match existing {
                    // 批内刚插入的那条：正文还在手上。
                    Some(stored) => stored == message,
                    None => {
                        let payload = sqlx::query_scalar::<_, String>(
                            "SELECT payload_json FROM messages
                              WHERE conversation_id = ? AND message_id = ?",
                        )
                        .bind(conversation_id.as_str())
                        .bind(&message.message_id)
                        .fetch_optional(&mut *tx)
                        .await?;
                        payload
                            .map(|payload| {
                                serde_json::from_str::<CanonicalMessage>(&payload)
                                    .map(|stored| stored == *message)
                                    .map_err(Error::from)
                            })
                            .transpose()?
                            .unwrap_or(false)
                    }
                };
                if !stored {
                    return Err(Error::Store(format!(
                        "message id or runtime event reused with different content: {}",
                        message.message_id
                    )));
                }
                appended.push((head, false));
                continue;
            }
            let digest = chained_digest(&parent_digest, std::slice::from_ref(message))?;
            if sqlx::query_scalar::<_, i64>(
                "SELECT checkpoint_id FROM conversation_checkpoints
                 WHERE conversation_id = ? AND state_digest = ?",
            )
            .bind(conversation_id.as_str())
            .bind(digest.as_slice())
            .fetch_optional(&mut *tx)
            .await?
            .is_some()
            {
                return Err(Error::Store(
                    "active append would reuse an existing checkpoint instead of creating a child"
                        .into(),
                ));
            }
            head = Self::insert_checkpoint_tx(
                &mut tx,
                conversation_id,
                head,
                std::slice::from_ref(message),
                digest,
            )
            .await?;
            parent_digest = digest.to_vec();
            known.push(ChainIdentity {
                message_id: message.message_id.clone(),
                runtime_event_id: message.runtime_event_id.clone(),
                body: Some(message),
            });
            appended.push((head, true));
        }
        if head != expected {
            // 与逐条提交一致：批尾成为会话的当前节点与 run 的头（中途的中间态不外露，
            // 因为整个过程都持着写锁）。
            let updated = sqlx::query(
                "UPDATE conversations SET current_checkpoint_id = ?, updated_at_ms = ?
                 WHERE conversation_id = ? AND current_checkpoint_id = ? AND active_run_id = ?",
            )
            .bind(head.0)
            .bind(now_ms())
            .bind(conversation_id.as_str())
            .bind(expected.0)
            .bind(run_id.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if updated != 1 {
                return Err(Error::Store(format!(
                    "lost active ownership while appending checkpoint for run {run_id}"
                )));
            }
            sqlx::query(
                "UPDATE runs SET head_checkpoint_id = ?, updated_at_ms = ? WHERE run_id = ?",
            )
            .bind(head.0)
            .bind(now_ms())
            .bind(run_id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(appended)
    }

    pub(crate) async fn append_checkpoint_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
        run_id: &RunId,
        expected: CheckpointId,
        additions: &[CanonicalMessage],
    ) -> Result<CheckpointId> {
        let parent_digest = Self::checkpoint_digest_tx(tx, expected).await?;
        let digest = chained_digest(&parent_digest, additions)?;
        Self::append_checkpoint_with_digest_tx(
            tx,
            conversation_id,
            run_id,
            expected,
            additions,
            digest,
        )
        .await
    }

    async fn append_checkpoint_with_digest_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
        run_id: &RunId,
        expected: CheckpointId,
        additions: &[CanonicalMessage],
        digest: [u8; 32],
    ) -> Result<CheckpointId> {
        Self::require_active_head_tx(tx, conversation_id, run_id, expected).await?;
        if sqlx::query_scalar::<_, i64>(
            "SELECT checkpoint_id FROM conversation_checkpoints
             WHERE conversation_id = ? AND state_digest = ?",
        )
        .bind(conversation_id.as_str())
        .bind(digest.as_slice())
        .fetch_optional(&mut **tx)
        .await?
        .is_some()
        {
            return Err(Error::Store(
                "active append would reuse an existing checkpoint instead of creating a child"
                    .into(),
            ));
        }
        let checkpoint =
            Self::insert_checkpoint_tx(tx, conversation_id, expected, additions, digest).await?;
        let updated = sqlx::query(
            "UPDATE conversations SET current_checkpoint_id = ?, updated_at_ms = ?
             WHERE conversation_id = ? AND current_checkpoint_id = ? AND active_run_id = ?",
        )
        .bind(checkpoint.0)
        .bind(now_ms())
        .bind(conversation_id.as_str())
        .bind(expected.0)
        .bind(run_id.as_str())
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if updated != 1 {
            return Err(Error::Store(format!(
                "lost active ownership while appending checkpoint for run {run_id}"
            )));
        }
        sqlx::query("UPDATE runs SET head_checkpoint_id = ?, updated_at_ms = ? WHERE run_id = ?")
            .bind(checkpoint.0)
            .bind(now_ms())
            .bind(run_id.as_str())
            .execute(&mut **tx)
            .await?;
        Ok(checkpoint)
    }

    /// 会话的根 checkpoint(`ensure_conversation_tx` 建的那条空历史)。
    /// 一个会话恰好一个根,所以这里取最早的 NULL 父节点;取不到说明数据被外部改坏了,
    /// 报可读的错,而不是让调用方拿着错误的父节点继续写。
    async fn conversation_root_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
    ) -> Result<CheckpointId> {
        let root = sqlx::query_scalar::<_, i64>(
            "SELECT checkpoint_id FROM conversation_checkpoints
             WHERE conversation_id = ? AND parent_checkpoint_id IS NULL
             ORDER BY checkpoint_id LIMIT 1",
        )
        .bind(conversation_id.as_str())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| {
            Error::Store(format!(
                "conversation {} has no root checkpoint",
                conversation_id.as_str()
            ))
        })?;
        Ok(CheckpointId(root))
    }

    async fn insert_checkpoint_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
        parent: CheckpointId,
        additions: &[CanonicalMessage],
        digest: [u8; 32],
    ) -> Result<CheckpointId> {
        for message in additions {
            Self::put_message_tx(tx, conversation_id, message).await?;
        }
        let checkpoint = sqlx::query(
            "INSERT INTO conversation_checkpoints
             (conversation_id, parent_checkpoint_id, state_digest, created_at_ms)
             VALUES (?, ?, ?, ?)",
        )
        .bind(conversation_id.as_str())
        .bind(parent.0)
        .bind(digest.as_slice())
        .bind(now_ms())
        .execute(&mut **tx)
        .await?
        .last_insert_rowid();
        for (ordinal, message) in additions.iter().enumerate() {
            sqlx::query(
                "INSERT INTO checkpoint_messages(checkpoint_id, ordinal, conversation_id, message_id)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(checkpoint)
            .bind(ordinal as i64)
            .bind(conversation_id.as_str())
            .bind(&message.message_id)
            .execute(&mut **tx)
            .await?;
        }
        Ok(CheckpointId(checkpoint))
    }
}

/// `incoming` 的前 `stored.len()` 条是不是就是 `stored` 那条链。
///
/// 只比内容,不比内部身份:客户端回带的 state 水合出来的消息身份(`cursor-root:{blob}:{序号}`)
/// 与库里存的那条(`cursor:user:*`、runtime 事件 id 等)本来就不同——checkpoint 编码只在
/// runtime / request-context 上保留 wire 身份。用整条 `CanonicalMessage` 相等去判断,会让
/// 每一轮都认为"客户端的历史对不上本地链",于是把整段历史当新增重写一遍,写放大与行数
/// 随轮数平方增长。要回答的问题其实是"客户端手上这段是不是我们已经存下的那段",那是内容问题。
fn extends_prefix(stored: &[CanonicalMessage], incoming: &[CanonicalMessage]) -> bool {
    incoming.len() >= stored.len()
        && stored.iter().zip(incoming).all(|(stored, wire)| {
            stored.role == wire.role
                && stored.origin == wire.origin
                && stored.content == wire.content
                && stored.runtime_event_id == wire.runtime_event_id
        })
}

/// 会话根的摘要：空历史的摘要。
///
/// 根没有父节点，链式摘要从它起算（`conversations.rs` 建根时写这一份）。
pub(crate) fn root_digest() -> [u8; 32] {
    Sha256::new().finalize().into()
}

/// checkpoint 的链式摘要：`父摘要 || 本次新增消息`。
///
/// `state_digest` 过去是"整条历史的摘要"，于是每次追加都要把**整条历史**重新装载、
/// 反序列化、再哈希一遍：一轮里每条运行时消息、每条助手消息都付一次 O(历史) 的代价，
/// 客户端每轮回带历史时 `import_checkpoint` 还要再算一次（审计 05/P2-4）。
///
/// 改成链式之后，追加只需要父 checkpoint 那一行的摘要加本次新增的消息。这不会削弱
/// 身份判定：父子定位本来就带 `parent_checkpoint_id`，而同一个父 + 同一批新增必然得到
/// 同一个摘要（决定论），不同路径上内容相同的历史各自成链、互不混淆。会话根的摘要
/// 仍是空历史的摘要，所以"从根沿链重算"与落库的 `state_digest` 恒等——见
/// `the_chain_digest_rebuilds_from_the_root`。
fn chained_digest(parent: &[u8], additions: &[CanonicalMessage]) -> Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(parent);
    update_digest(&mut hasher, additions)?;
    Ok(hasher.finalize().into())
}

fn update_digest(hasher: &mut Sha256, messages: &[CanonicalMessage]) -> Result<()> {
    for message in messages {
        let bytes = serde_json::to_vec(message)?;
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    Ok(())
}

impl Store {
    /// 一个 checkpoint 落库的摘要（父摘要的唯一入口）。
    async fn checkpoint_digest(&self, checkpoint_id: CheckpointId) -> Result<Vec<u8>> {
        let digest = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT state_digest FROM conversation_checkpoints WHERE checkpoint_id = ?",
        )
        .bind(checkpoint_id.0)
        .fetch_optional(&self.pool)
        .await?;
        digest.ok_or_else(|| {
            Error::Store(format!(
                "checkpoint {} is missing while computing its parent digest",
                checkpoint_id.0
            ))
        })
    }

    async fn checkpoint_digest_tx(
        tx: &mut Transaction<'_, Sqlite>,
        checkpoint_id: CheckpointId,
    ) -> Result<Vec<u8>> {
        let digest = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT state_digest FROM conversation_checkpoints WHERE checkpoint_id = ?",
        )
        .bind(checkpoint_id.0)
        .fetch_optional(&mut **tx)
        .await?;
        digest.ok_or_else(|| {
            Error::Store(format!(
                "checkpoint {} is missing while computing its parent digest",
                checkpoint_id.0
            ))
        })
    }

    /// 这条链上已有的消息身份，按链序（根 → 尾）。
    ///
    /// 只走 `checkpoint_messages` 的身份列 + `messages` 的主键/唯一索引，不装载正文：
    /// 冲突检查过去是把整条历史装载出来再逐条比 `message_id`（审计 05/P2-4 的
    /// `append_message_once` 全链重载）。一批消息共用这一次结果（审计 09/F2）。
    async fn chain_identities(
        &self,
        conversation_id: &ConversationId,
        checkpoint_id: CheckpointId,
    ) -> Result<Vec<ChainIdentity<'_>>> {
        #[cfg(test)]
        probe::record_chain_load(conversation_id.as_str());
        let rows = sqlx::query(
            "WITH RECURSIVE chain(checkpoint_id, parent_checkpoint_id) AS (
                 SELECT checkpoint_id, parent_checkpoint_id FROM conversation_checkpoints
                  WHERE checkpoint_id = ?1 AND conversation_id = ?2
                 UNION ALL
                 SELECT parent.checkpoint_id, parent.parent_checkpoint_id
                   FROM conversation_checkpoints parent
                   JOIN chain ON parent.checkpoint_id = chain.parent_checkpoint_id
             )
             SELECT checkpoint_messages.message_id, messages.runtime_event_id
               FROM chain
               JOIN checkpoint_messages ON checkpoint_messages.checkpoint_id = chain.checkpoint_id
               JOIN messages
                 ON messages.conversation_id = checkpoint_messages.conversation_id
                AND messages.message_id = checkpoint_messages.message_id
              ORDER BY checkpoint_messages.checkpoint_id ASC, checkpoint_messages.ordinal ASC",
        )
        .bind(checkpoint_id.0)
        .bind(conversation_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ChainIdentity {
                message_id: row.get(0),
                runtime_event_id: row.get(1),
                body: None,
            })
            .collect())
    }
}

/// 链上一条消息的身份（批内刚插入的那条连正文一起带着，省一次回库）。
struct ChainIdentity<'a> {
    message_id: String,
    runtime_event_id: Option<String>,
    body: Option<&'a CanonicalMessage>,
}

impl ChainIdentity<'_> {
    /// 与 `append_message_once` 过去的冲突判定同一个谓词与同一个顺序。
    fn matches(&self, message: &CanonicalMessage) -> bool {
        self.message_id == message.message_id
            || message.runtime_event_id.is_some()
                && self.runtime_event_id == message.runtime_event_id
    }
}

/// 测试用的装载计数：某个会话整链装载（身份查询）了几次。
///
/// 09/F2 是性能条目——"N 条消息只装载一次"只能落在"做了多少次装载"上：计数按会话 id
/// 记，每条用例用自己的会话 id，并行跑也不会互相干扰。只在测试构建里存在。
#[cfg(test)]
pub(crate) mod probe {
    use std::{collections::HashMap, sync::Mutex};

    static CHAIN_LOADS: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

    pub(crate) fn record_chain_load(conversation_id: &str) {
        let mut guard = CHAIN_LOADS.lock().unwrap();
        *guard
            .get_or_insert_with(HashMap::new)
            .entry(conversation_id.to_owned())
            .or_default() += 1;
    }

    /// 取走并清零这个会话的计数。
    pub(crate) fn take_chain_loads(conversation_id: &str) -> usize {
        let mut guard = CHAIN_LOADS.lock().unwrap();
        guard
            .get_or_insert_with(HashMap::new)
            .remove(conversation_id)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Origin, Role};

    async fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            dir.path().join("test.db").display()
        ))
        .await
        .unwrap();
        (dir, store)
    }

    fn user(id: &str, text: &str) -> CanonicalMessage {
        CanonicalMessage::text(id, Role::User, Origin::User, text)
    }

    async fn root_count(store: &Store, conversation: &ConversationId) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_checkpoints
             WHERE conversation_id = ? AND parent_checkpoint_id IS NULL",
        )
        .bind(conversation.as_str())
        .fetch_one(store.pool())
        .await
        .unwrap()
    }

    async fn message_rows(store: &Store, conversation: &ConversationId) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM checkpoint_messages WHERE conversation_id = ?")
            .bind(conversation.as_str())
            .fetch_one(store.pool())
            .await
            .unwrap()
    }

    async fn checkpoint_rows(store: &Store, conversation: &ConversationId) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_checkpoints WHERE conversation_id = ?",
        )
        .bind(conversation.as_str())
        .fetch_one(store.pool())
        .await
        .unwrap()
    }

    /// 一轮跑完后的状态:会话的当前 checkpoint 推进到这一轮的 head。
    /// (`import_checkpoint` 只负责把客户端回带的历史落成 run 的基线,推进由 run 的提交做。)
    async fn advance(store: &Store, conversation: &ConversationId, head: CheckpointId) {
        sqlx::query("UPDATE conversations SET current_checkpoint_id = ? WHERE conversation_id = ?")
            .bind(head.0)
            .bind(conversation.as_str())
            .execute(store.pool())
            .await
            .unwrap();
    }

    /// 复现:客户端回带的 state 水合出的消息身份与库里那条不同(checkpoint 编码只在
    /// runtime / request-context 上保留 wire 身份),整条相等的前缀比较必然为假,于是
    /// 每一轮都把**整段历史**当新增再写一遍——写放大与 `checkpoint_messages` 行数随
    /// 轮数平方增长,而内容其实一个字都没变。这里钉住:会话始终只有一个根,第二轮只
    /// 新增真正的新消息,历史内容仍然完整。
    #[tokio::test]
    async fn reimporting_the_same_history_neither_forks_a_root_nor_rewrites_every_message() {
        let (_dir, store) = fixture().await;
        let conversation = ConversationId::new("reimport");

        // 第一轮:客户端回带 [u1]。
        let first = vec![user("cursor:user:1", "你好")];
        let head = store
            .import_checkpoint(&conversation, &first)
            .await
            .unwrap();
        advance(&store, &conversation, head).await;
        let after_first = message_rows(&store, &conversation).await;

        // 第二轮:同一段历史水合回来身份变了,后面多一条真正的新消息。
        let second = vec![
            user("cursor-root:AAAA:0", "你好"),
            user("cursor-root:AAAA:1", "继续"),
        ];
        let head = store
            .import_checkpoint(&conversation, &second)
            .await
            .unwrap();
        advance(&store, &conversation, head).await;

        assert_eq!(
            root_count(&store, &conversation).await,
            1,
            "会话只能有一个根"
        );
        assert_eq!(
            message_rows(&store, &conversation).await - after_first,
            1,
            "第二轮只应写入那一条新消息,不能把整段历史重写一遍"
        );
        assert_eq!(
            store
                .load_current_messages(&conversation)
                .await
                .unwrap()
                .len(),
            2,
            "历史内容仍要完整"
        );
    }

    /// 链式摘要必须能从会话根一路重算出来：落库的 `state_digest` 是"父摘要 + 新增"，
    /// 只要父在、内容没变，任何一次重算都必须逐字节相同。算错的话，客户端回带的历史
    /// 会永远对不上本地链（每轮整段重写）。
    #[tokio::test]
    async fn the_chain_digest_rebuilds_from_the_root() {
        let (_dir, store) = fixture().await;
        let conversation = ConversationId::new("chain-digest");
        let (run_id, root) = claimed_run(&store, &conversation).await;

        let first = user("runtime:1", "你好");
        let (second, _) = store
            .append_message_once(&conversation, &run_id, root, &first)
            .await
            .unwrap();
        let third_message = user("runtime:2", "继续");
        let (third, _) = store
            .append_message_once(&conversation, &run_id, second, &third_message)
            .await
            .unwrap();

        let root_stored = store.checkpoint_digest(root).await.unwrap();
        assert_eq!(
            root_stored,
            root_digest().to_vec(),
            "会话根的摘要就是空历史的摘要"
        );
        // 一步一步从根重算（每个 checkpoint 的新增只有一条）。
        let step_one = chained_digest(&root_stored, std::slice::from_ref(&first)).unwrap();
        assert_eq!(
            store.checkpoint_digest(second).await.unwrap(),
            step_one.to_vec(),
            "第一跳必须能从根重算出来"
        );
        let step_two = chained_digest(&step_one, std::slice::from_ref(&third_message)).unwrap();
        assert_eq!(
            store.checkpoint_digest(third).await.unwrap(),
            step_two.to_vec(),
            "从根沿链重算必须与落库的摘要一致"
        );

        // 摘要必须真的挂在父上：它不能等于"只对内容求的摘要"（那就是被换掉的旧算法），
        // 换一个父摘要也必须换一个结果——否则不同历史链会撞在同一个 state_digest 上，
        // 前缀匹配会认错链。
        let content_only = {
            let mut hasher = Sha256::new();
            update_digest(&mut hasher, std::slice::from_ref(&first)).unwrap();
            hasher.finalize().to_vec()
        };
        assert_ne!(
            store.checkpoint_digest(second).await.unwrap(),
            content_only,
            "摘要必须把父摘要算进去"
        );
        assert_ne!(
            chained_digest(&root_stored, std::slice::from_ref(&first)).unwrap(),
            chained_digest(&[0u8; 32], std::slice::from_ref(&first)).unwrap(),
            "换一个父摘要必须换一个结果"
        );

        // 前缀匹配走的就是这份摘要：两步都必须命中，且返回链尾。
        let (matched, reused) = store
            .match_checkpoint_prefix(&conversation, root, &[first.clone(), third_message.clone()])
            .await
            .unwrap();
        assert_eq!((matched, reused), (third, 2));

        // 内容改一个字：第一跳仍然命中，第二跳必须停在原处。
        let changed = user("runtime:2", "改一下");
        let (matched, reused) = store
            .match_checkpoint_prefix(&conversation, root, &[first, changed])
            .await
            .unwrap();
        assert_eq!((matched, reused), (second, 1));
    }

    /// 一批消息只装载一次链（审计 09/F2）：批内每条仍各拿一个 checkpoint、摘要逐条
    /// 链在父上，省掉的是每条一次的身份装载。
    ///
    /// 用 `probe` 精确数装载次数：整批 1 次；作为对照，逐条调用就是每条 1 次。
    #[tokio::test]
    async fn appending_a_batch_loads_the_chain_once() {
        let (_dir, store) = fixture().await;
        let conversation = ConversationId::new("batch-loads");
        let (run_id, root) = claimed_run(&store, &conversation).await;
        // 先垫一段历史，让"整链装载"这件事有分量。
        let (head, _) = store
            .append_message_once(&conversation, &run_id, root, &user("runtime:0", "历史"))
            .await
            .unwrap();

        let batch = (1..=4)
            .map(|index| user(&format!("runtime:{index}"), "批内"))
            .collect::<Vec<_>>();
        let _ = probe::take_chain_loads(conversation.as_str());
        let appended = store
            .append_messages_once(&conversation, &run_id, head, &batch)
            .await
            .unwrap();
        assert_eq!(appended.len(), 4);
        assert!(appended.iter().all(|(_, inserted)| *inserted));
        assert_eq!(
            probe::take_chain_loads(conversation.as_str()),
            1,
            "整批只装载一次链"
        );
        // 每条各一个 checkpoint，依次递增，摘要逐条链在父上。
        assert!(appended.windows(2).all(|pair| pair[0].0 .0 < pair[1].0 .0));
        assert_eq!(
            store
                .load_checkpoint_messages(appended.last().unwrap().0)
                .await
                .unwrap()
                .len(),
            5,
            "历史 + 批内四条"
        );
        let mut parent_digest = store.checkpoint_digest(head).await.unwrap();
        for (index, (checkpoint, _)) in appended.iter().enumerate() {
            let digest = store.checkpoint_digest(*checkpoint).await.unwrap();
            assert_eq!(
                digest,
                chained_digest(&parent_digest, std::slice::from_ref(&batch[index]))
                    .unwrap()
                    .to_vec(),
                "批内第 {} 条的摘要仍然挂在它自己的父上",
                index + 1
            );
            parent_digest = digest;
        }

        // 对照：逐条调用就是每条一次装载。
        let mut head = appended.last().unwrap().0;
        let _ = probe::take_chain_loads(conversation.as_str());
        for index in 5..=7 {
            let (next, _) = store
                .append_message_once(
                    &conversation,
                    &run_id,
                    head,
                    &user(&format!("runtime:{index}"), "逐条"),
                )
                .await
                .unwrap();
            head = next;
        }
        assert_eq!(
            probe::take_chain_loads(conversation.as_str()),
            3,
            "逐条调用是每条一次装载（旧行为的成本形状）"
        );

        // 批内重复身份：返回同一个 checkpoint、不写库；批内新身份照常插入。
        let repeated = store
            .append_messages_once(
                &conversation,
                &run_id,
                head,
                &[batch[0].clone(), user("runtime:8", "新")],
            )
            .await
            .unwrap();
        assert_eq!(repeated[0], (head, false), "重复身份返回当前 checkpoint");
        assert!(repeated[1].1);
        assert_eq!(message_rows(&store, &conversation).await, 9);

        // 批内冲突：整批回滚，前面那条也不能留下。
        let conflicting = user("runtime:9", "先落地");
        let mut clash = user("runtime:9", "改过");
        clash.runtime_event_id = Some("event:runtime:9".into());
        let mut first = conflicting.clone();
        first.runtime_event_id = Some("event:runtime:9".into());
        let outcome = store
            .append_messages_once(&conversation, &run_id, repeated[1].0, &[first, clash])
            .await;
        assert!(outcome.is_err(), "同身份不同内容必须报错");
        assert!(
            store
                .message(&conversation, "runtime:9")
                .await
                .unwrap()
                .is_none(),
            "整批回滚：批内第一条不能留在库里"
        );
    }

    /// 客户端回带的历史和本地链完全一样时不能多插空节点（审计 05/P2-4 的残留）。
    ///
    /// 过去 import 照样插一个 0 条新增的 checkpoint：每轮多一层空节点、一次重投叠一层，
    /// 链越来越深而内容一字未变。`append_checkpoint` 早就有同样规则的早退，import 漏了。
    #[tokio::test]
    async fn reimporting_the_exact_history_adds_no_checkpoint() {
        let (_dir, store) = fixture().await;
        let conversation = ConversationId::new("empty-import");
        let history = vec![user("cursor:user:1", "你好"), user("cursor:user:2", "继续")];

        let first = store
            .import_checkpoint(&conversation, &history)
            .await
            .unwrap();
        // 让会话的当前节点就是这条链：下一次回带的历史"正好等于本地链"，新增为空 —
        // 这正是过去会多插一个空节点的那条路径（`current` 不是根时才会走到）。
        advance(&store, &conversation, first).await;
        let rows_after_first = checkpoint_rows(&store, &conversation).await;
        assert_eq!(rows_after_first, 2, "根 + 一条 import 的 checkpoint");

        let second = store
            .import_checkpoint(&conversation, &history)
            .await
            .unwrap();
        assert_eq!(second, first, "同样的历史必须返回同一个 head");
        assert_eq!(
            checkpoint_rows(&store, &conversation).await,
            rows_after_first,
            "回带相同历史不能新增空节点"
        );
        // 再投一次也一样（客户端重投是常态）。
        let third = store
            .import_checkpoint(&conversation, &history)
            .await
            .unwrap();
        assert_eq!(third, first);
        assert_eq!(
            checkpoint_rows(&store, &conversation).await,
            rows_after_first
        );
        // 多一条真正的新消息才该出现新的 checkpoint。
        let mut extended = history;
        extended.push(user("cursor:user:3", "再来"));
        let fourth = store
            .import_checkpoint(&conversation, &extended)
            .await
            .unwrap();
        assert_ne!(fourth, first);
        assert_eq!(
            checkpoint_rows(&store, &conversation).await,
            rows_after_first + 1
        );
        assert_eq!(
            store.load_checkpoint_messages(fourth).await.unwrap().len(),
            3
        );
    }

    /// 空历史 + 非空本地链是**分叉**，不是"没有新增"：要建一个链为空的基线。
    ///
    /// 客户端（裁剪过的 state）看到的历史一条不剩时，它的视角就是"没有历史"；新 run 必须
    /// 从这里起跑，否则会带上本地链里它没看到的消息（`error_lifecycle` 的
    /// `newer_run_request_on_one_bidi_stream_replaces_the_active_run` 钉住这个语义）。
    #[tokio::test]
    async fn importing_an_empty_history_forks_at_the_root() {
        let (_dir, store) = fixture().await;
        let conversation = ConversationId::new("empty-import-root");
        let head = store
            .import_checkpoint(&conversation, &[user("cursor:user:1", "你好")])
            .await
            .unwrap();
        advance(&store, &conversation, head).await;
        let rows = checkpoint_rows(&store, &conversation).await;
        let forked = store.import_checkpoint(&conversation, &[]).await.unwrap();
        assert_ne!(forked, head, "空历史是新的分叉点，不是当前链尾");
        assert_eq!(checkpoint_rows(&store, &conversation).await, rows + 1);
        assert!(
            store
                .load_checkpoint_messages(forked)
                .await
                .unwrap()
                .is_empty(),
            "这个客户端视角没有历史"
        );
        // 再投一次同样的空历史：分叉点已经存在，摘要命中，不再叠节点。
        let again = store.import_checkpoint(&conversation, &[]).await.unwrap();
        assert_eq!(again, forked);
        assert_eq!(checkpoint_rows(&store, &conversation).await, rows + 1);
    }

    /// 运行时消息提交：一次装载算出的摘要要落进 checkpoint，幂等与冲突语义都不变。
    #[tokio::test]
    async fn appending_a_runtime_message_stores_the_chain_digest_once() {
        let (_dir, store) = fixture().await;
        let conversation = ConversationId::new("runtime-append");
        let (run_id, root) = claimed_run(&store, &conversation).await;
        let message = user("runtime:1", "hello");

        let (head, inserted) = store
            .append_message_once(&conversation, &run_id, root, &message)
            .await
            .unwrap();
        assert!(inserted);
        let digest: Vec<u8> = sqlx::query_scalar(
            "SELECT state_digest FROM conversation_checkpoints WHERE checkpoint_id = ?",
        )
        .bind(head.0)
        .fetch_one(store.pool())
        .await
        .unwrap();
        let history = store.load_checkpoint_messages(head).await.unwrap();
        assert_eq!(history, vec![message.clone()]);
        assert_eq!(
            digest,
            chained_digest(&root_digest(), &history).unwrap().to_vec(),
            "落库的摘要必须是'父摘要 + 本次新增'"
        );

        let (again, inserted) = store
            .append_message_once(&conversation, &run_id, head, &message)
            .await
            .unwrap();
        assert!(!inserted, "同一条消息第二次提交不写新 checkpoint");
        assert_eq!(again, head);
        assert_eq!(message_rows(&store, &conversation).await, 1);

        let conflicting = user("runtime:1", "different");
        assert!(
            store
                .append_message_once(&conversation, &run_id, head, &conflicting)
                .await
                .is_err(),
            "同身份不同内容必须报错"
        );
        assert_eq!(message_rows(&store, &conversation).await, 1);

        // runtime 事件身份跨消息复用（`message_id` 不同）同样是冲突。
        let mut with_event = user("runtime:event-1", "hello");
        with_event.runtime_event_id = Some("evt-1".into());
        let (event_head, inserted) = store
            .append_message_once(&conversation, &run_id, head, &with_event)
            .await
            .unwrap();
        assert!(inserted);
        let mut replayed_event = user("runtime:event-2", "hello");
        replayed_event.runtime_event_id = Some("evt-1".into());
        assert!(
            store
                .append_message_once(&conversation, &run_id, event_head, &replayed_event)
                .await
                .is_err(),
            "同一个运行时事件不能以别的 message_id 再来一次"
        );
        // 同样的身份与内容：幂等。
        let (unchanged, inserted) = store
            .append_message_once(&conversation, &run_id, event_head, &with_event)
            .await
            .unwrap();
        assert!(!inserted);
        assert_eq!(unchanged, event_head);
    }

    async fn claimed_run(store: &Store, conversation: &ConversationId) -> (RunId, CheckpointId) {
        use crate::model::{ModelSpec, PreparedRun, PromptSpec, RunAction, RunKind};

        let root = store.ensure_conversation(conversation).await.unwrap();
        let run_id = RunId::new("run-1");
        let prepared = PreparedRun {
            run_id: run_id.clone(),
            cursor_request_id: None,
            conversation_id: conversation.clone(),
            kind: RunKind::Root,
            model: ModelSpec::new("model"),
            prompt: PromptSpec {
                instructions: String::new(),
                tools: Vec::new(),
            },
            initial_messages: Vec::new(),
            action: RunAction::Start,
            base_checkpoint_id: root,
        };
        let claimed = store.claim_run(&prepared).await.unwrap();
        (run_id, claimed.head_checkpoint_id)
    }

    /// 不变式：每个 checkpoint 落库的摘要都等于"父摘要 + 它自己的新增"。
    ///
    /// 换摘要算法最容易错的地方是"有一条写入路径没跟着换"（append / import / replace
    /// 各自写摘要）：只要有一条不自洽，前缀匹配就会静默退化成"每轮整段重写"。这里把
    /// 三条路径都走一遍，然后按库里存的新增逐行重算。
    #[tokio::test]
    async fn every_checkpoint_digest_chains_to_its_parent() {
        use sqlx::Row;

        let (_dir, store) = fixture().await;
        let conversation = ConversationId::new("chain-invariant");
        let (run_id, root) = claimed_run(&store, &conversation).await;

        // import：客户端一次带回两条（批量为 2 的 checkpoint）。
        let imported = vec![
            user("cursor-root:AAAA:0", "你好"),
            user("cursor-root:AAAA:1", "继续"),
        ];
        let batch = store
            .import_checkpoint(&conversation, &imported)
            .await
            .unwrap();
        let batch_parent: Option<i64> = sqlx::query_scalar(
            "SELECT parent_checkpoint_id FROM conversation_checkpoints WHERE checkpoint_id = ?",
        )
        .bind(batch.0)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(batch_parent, Some(root.0), "import 的第一条挂在根下面");
        advance(&store, &conversation, batch).await;
        // append：一条运行时消息。
        let (appended, inserted) = store
            .append_message_once(&conversation, &run_id, batch, &user("runtime:1", "runtime"))
            .await
            .unwrap();
        assert!(inserted);
        advance(&store, &conversation, appended).await;
        // replace：整段替换（压缩后的历史）挂回根。
        let replaced = store
            .replace_checkpoint(
                &conversation,
                &run_id,
                appended,
                &[user("summary:1", "摘要"), user("cursor:user:9", "新问")],
            )
            .await
            .unwrap();
        assert!(replaced.0 > appended.0);

        let rows = sqlx::query(
            "SELECT checkpoint_id, parent_checkpoint_id, state_digest
               FROM conversation_checkpoints WHERE conversation_id = ? ORDER BY checkpoint_id",
        )
        .bind(conversation.as_str())
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert!(rows.len() >= 4, "根 + 三条写入路径各一条");
        let mut digests: std::collections::HashMap<i64, Vec<u8>> = std::collections::HashMap::new();
        for row in rows {
            let id: i64 = row.get(0);
            let parent: Option<i64> = row.get(1);
            let stored: Vec<u8> = row.get(2);
            let payloads = sqlx::query_scalar::<_, String>(
                "SELECT messages.payload_json FROM checkpoint_messages
                   JOIN messages
                     ON messages.conversation_id = checkpoint_messages.conversation_id
                    AND messages.message_id = checkpoint_messages.message_id
                  WHERE checkpoint_messages.checkpoint_id = ?
                  ORDER BY checkpoint_messages.ordinal",
            )
            .bind(id)
            .fetch_all(store.pool())
            .await
            .unwrap();
            let additions = payloads
                .iter()
                .map(|payload| serde_json::from_str::<CanonicalMessage>(payload).unwrap())
                .collect::<Vec<_>>();
            // 根没有父摘要，它自己的值就是"空历史的摘要"；其余每一行都必须由父摘要
            // 与自己的新增重算出来。
            let expected = match parent {
                Some(parent) => chained_digest(&digests[&parent], &additions)
                    .unwrap()
                    .to_vec(),
                None => root_digest().to_vec(),
            };
            assert_eq!(
                stored, expected,
                "checkpoint {id} 的摘要必须由其父摘要与自己的新增重算出来"
            );
            digests.insert(id, stored);
        }
    }
}
