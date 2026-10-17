//! Persists model and provider configuration.
use std::{collections::HashSet, str::FromStr};

use sqlx::{Row, Sqlite, Transaction};

use crate::{
    model::{model_fingerprint, normalize_model_input, ModelConfig, ModelConfigInput, ModelType},
    Error, Result,
};

use super::{now_ms, Store};

/// 两份配置内容相同：创建与编辑都拒绝落成重复配置。
const DUPLICATE_CONFIGURATION: &str = "model configurations must be unique";

/// 模型不在了：说清发生了什么，并给出下一步。
///
/// 以前这里回 `Error::RunNotFound("model …")`——界面原样弹 "run not found"，用户既不知道
/// 是模型没了、也不知道该做什么（模型根本不是 run）。`Config` 这一类在界面里是"配置问题"，
/// 文案带上"刷新模型列表"这个可执行步骤。
fn missing_model(hash: &str) -> Error {
    Error::Config(format!(
        "model {hash} no longer exists; reload the model list and try again"
    ))
}

const MODEL_COLUMNS: &str = r#"
    model_hash, sort_order, display_name, group_name, model_type, base_url, use_full_url, api_key, tooltip_data,
    model_id, reasoning_effort, openai_endpoint, openai_extra_params_enabled,
    openai_extra_params_json, custom_headers_enabled, custom_headers_json,
    anthropic_extra_params_enabled, anthropic_extra_params_json, context_window_tokens,
    max_completion_tokens, anthropic_max_tokens, anthropic_thinking_effort,
    thinking_budget_tokens, created_at_ms, updated_at_ms
"#;

impl Store {
    pub async fn models(&self) -> Result<Vec<ModelConfig>> {
        let query =
            format!("SELECT {MODEL_COLUMNS} FROM model_configs ORDER BY sort_order, display_name");
        sqlx::query(&query)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(model_from_row)
            .collect()
    }

    pub async fn model(&self, hash: &str) -> Result<Option<ModelConfig>> {
        let query = format!("SELECT {MODEL_COLUMNS} FROM model_configs WHERE model_hash = ?");
        sqlx::query(&query)
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?
            .map(model_from_row)
            .transpose()
    }

    /// 取一份模型配置；模型不在了就报"配置问题 + 下一步"这条统一文案。
    ///
    /// 发现模型回填凭据、连通性测试这类"按 hash 找模型"的入口都该走这里：过去它们回
    /// `Error::RunNotFound("model …")`，界面原样弹 "run not found"（模型根本不是 run，
    /// 用户也不知道下一步做什么），与更新/删除/复制已经统一过的那条文案不一致
    /// （审计 11/P3-5）。
    pub async fn require_model(&self, hash: &str) -> Result<ModelConfig> {
        self.model(hash).await?.ok_or_else(|| missing_model(hash))
    }

    pub async fn create_model(&self, input: &ModelConfigInput) -> Result<ModelConfig> {
        let mut models = self.create_models(std::slice::from_ref(input)).await?;
        Ok(models.remove(0))
    }

    /// 创建一批模型配置。身份（`model_hash`）在这里分配，之后不再随配置变化；
    /// 去重按配置内容（`model_fingerprint`）判断，与身份无关。
    pub async fn create_models(&self, inputs: &[ModelConfigInput]) -> Result<Vec<ModelConfig>> {
        let _write = self.writes.lock().await;
        self.insert_models(inputs).await
    }

    /// [`create_models`](Self::create_models) 的本体：调用方必须已持有写锁。
    ///
    /// 单独拆出来是为了让 `duplicate_model` 把"算下一个序号"与"插入"放进同一把锁——
    /// 写锁不可重入，它不能先拿锁再调 `create_models`。
    async fn insert_models(&self, inputs: &[ModelConfigInput]) -> Result<Vec<ModelConfig>> {
        if inputs.is_empty() {
            return Err(Error::Config("at least one model is required".into()));
        }
        let mut normalized: Vec<(String, ModelConfigInput)> = Vec::with_capacity(inputs.len());
        let mut batch = HashSet::with_capacity(inputs.len());
        for input in inputs {
            let input = normalize_model_input(input)?;
            let fingerprint = model_fingerprint(&input)?;
            if !batch.insert(fingerprint.clone()) {
                return Err(Error::Config(DUPLICATE_CONFIGURATION.into()));
            }
            normalized.push((fingerprint, input));
        }
        let now = now_ms();
        let stored = self.stored_fingerprints(None).await?;
        if normalized
            .iter()
            .any(|(fingerprint, _)| stored.contains(fingerprint))
        {
            return Err(Error::Config(DUPLICATE_CONFIGURATION.into()));
        }
        let mut transaction = self.pool.begin().await?;
        let mut hashes = Vec::with_capacity(normalized.len());
        for (_, input) in &normalized {
            let hash = allocate_model_hash(&mut transaction).await?;
            insert_model(&mut transaction, &hash, input, now).await?;
            hashes.push(hash);
        }
        transaction.commit().await?;

        let mut saved = Vec::with_capacity(hashes.len());
        for hash in hashes {
            saved.push(self.model(&hash).await?.expect("inserted model must exist"));
        }
        Ok(saved)
    }

    /// 旧配置导入：内容已经存在的配置跳过（批内与库内都比对内容指纹），返回新增条数。
    pub(super) async fn create_models_if_missing(
        &self,
        inputs: &[ModelConfigInput],
    ) -> Result<usize> {
        let mut normalized: Vec<(String, ModelConfigInput)> = Vec::with_capacity(inputs.len());
        let mut batch = HashSet::with_capacity(inputs.len());
        for input in inputs {
            let input = normalize_model_input(input)?;
            let fingerprint = model_fingerprint(&input)?;
            if batch.insert(fingerprint.clone()) {
                normalized.push((fingerprint, input));
            }
        }
        let now = now_ms();
        let _write = self.writes.lock().await;
        let stored = self.stored_fingerprints(None).await?;
        let mut transaction = self.pool.begin().await?;
        let mut inserted = 0;
        for (fingerprint, input) in &normalized {
            if stored.contains(fingerprint) {
                continue;
            }
            let hash = allocate_model_hash(&mut transaction).await?;
            insert_model(&mut transaction, &hash, input, now).await?;
            inserted += 1;
        }
        transaction.commit().await?;
        Ok(inserted)
    }

    /// 库里已保存配置的内容指纹；`exclude` 用于更新时排除正在编辑的那一行。
    ///
    /// 任何一行无法归一化都直接报错：去重集合必须完整，不能拿残缺集合当事实。
    pub(super) async fn stored_fingerprints(
        &self,
        exclude: Option<&str>,
    ) -> Result<HashSet<String>> {
        let mut fingerprints = HashSet::new();
        for model in self.models().await? {
            if Some(model.model_hash.as_str()) == exclude {
                continue;
            }
            let input = model.config_input(model.display_name.clone(), model.sort_order);
            fingerprints.insert(model_fingerprint(&input)?);
        }
        Ok(fingerprints)
    }

    /// 更新一条模型配置。身份在创建时分配，这里不重算也不改写：Devin 绑定、
    /// 提交消息设置与历史调用都按身份引用这条模型，改名或轮换凭据不影响它们。
    pub async fn update_model(
        &self,
        current_hash: &str,
        input: &ModelConfigInput,
    ) -> Result<ModelConfig> {
        let current = self
            .model(current_hash)
            .await?
            .ok_or_else(|| missing_model(current_hash))?;
        // 接口不再回传明文 key，编辑表单留空即为"不修改"：沿用已保存的凭据。
        let mut input: ModelConfigInput = input.clone();
        if input.api_key.trim().is_empty() {
            input.api_key = current.api_key.clone();
        }
        let input = normalize_model_input(&input)?;
        let fingerprint = model_fingerprint(&input)?;
        let now = now_ms();
        let _write = self.writes.lock().await;
        if self
            .stored_fingerprints(Some(current_hash))
            .await?
            .contains(&fingerprint)
        {
            return Err(Error::Config(DUPLICATE_CONFIGURATION.into()));
        }
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query(
            r#"UPDATE model_configs SET
                sort_order = ?, display_name = ?, group_name = ?, model_type = ?, base_url = ?,
                use_full_url = ?, api_key = ?, tooltip_data = ?, model_id = ?, reasoning_effort = ?,
                openai_endpoint = ?, openai_extra_params_enabled = ?, openai_extra_params_json = ?,
                custom_headers_enabled = ?, custom_headers_json = ?,
                anthropic_extra_params_enabled = ?, anthropic_extra_params_json = ?,
                context_window_tokens = ?, max_completion_tokens = ?, anthropic_max_tokens = ?,
                anthropic_thinking_effort = ?, thinking_budget_tokens = ?, updated_at_ms = ?
            WHERE model_hash = ?"#,
        )
        .bind(input.sort_order)
        .bind(&input.display_name)
        .bind(&input.group_name)
        .bind(input.model_type.as_str())
        .bind(&input.base_url)
        .bind(input.use_full_url)
        .bind(&input.api_key)
        .bind(&input.tooltip_data)
        .bind(&input.model_id)
        .bind(&input.reasoning_effort)
        .bind(&input.openai_endpoint)
        .bind(input.openai_extra_params_enabled)
        .bind(serde_json::to_string(&input.openai_extra_params)?)
        .bind(input.custom_headers_enabled)
        .bind(serde_json::to_string(&input.custom_headers)?)
        .bind(input.anthropic_extra_params_enabled)
        .bind(serde_json::to_string(&input.anthropic_extra_params)?)
        .bind(input.context_window_tokens.map(to_i64).transpose()?)
        .bind(input.max_completion_tokens.map(to_i64).transpose()?)
        .bind(input.anthropic_max_tokens.map(to_i64).transpose()?)
        .bind(&input.anthropic_thinking_effort)
        .bind(input.thinking_budget_tokens.map(to_i64).transpose()?)
        .bind(now)
        .bind(current_hash)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() != 1 {
            return Err(missing_model(current_hash));
        }
        transaction.commit().await?;
        Ok(self
            .model(current_hash)
            .await?
            .expect("updated model must exist"))
    }

    /// 复制一份模型配置，连凭据一起。凭据不再回传前端，"连 key 一起复制"只能由服务端做。
    ///
    /// 下一个序号与插入在同一把写锁里算：以前先在锁外读 `MAX(sort_order)` 再走 `create_model`
    /// （它自己再拿锁），两次并发复制会读到同一个序号，插进去就是两条并列的配置——列表顺序
    /// 从此不稳定。写锁不可重入，所以这里直接用锁内的插入本体。
    pub async fn duplicate_model(&self, hash: &str, display_name: &str) -> Result<ModelConfig> {
        let source = self.model(hash).await?.ok_or_else(|| missing_model(hash))?;
        let _write = self.writes.lock().await;
        let sort_order: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(sort_order), 0) + 1 FROM model_configs")
                .fetch_one(&self.pool)
                .await?;
        let mut saved = self
            .insert_models(&[source.config_input(display_name.to_string(), sort_order)])
            .await?;
        Ok(saved.remove(0))
    }

    pub async fn delete_model(&self, hash: &str) -> Result<()> {
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin().await?;
        sqlx::query("UPDATE llm_calls SET model_hash = NULL WHERE model_hash = ?")
            .bind(hash)
            .execute(&mut *transaction)
            .await?;
        let result = sqlx::query("DELETE FROM model_configs WHERE model_hash = ?")
            .bind(hash)
            .execute(&mut *transaction)
            .await?;
        if result.rows_affected() != 1 {
            return Err(missing_model(hash));
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn reorder_models(&self, model_hashes: &[String]) -> Result<Vec<ModelConfig>> {
        let current = self.models().await?;
        let current_hashes = current
            .iter()
            .map(|model| model.model_hash.as_str())
            .collect::<HashSet<_>>();
        let requested_hashes = model_hashes
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        if model_hashes.len() != current.len()
            || requested_hashes.len() != current.len()
            || requested_hashes != current_hashes
        {
            return Err(Error::Config(
                "model configuration changed; refresh and try sorting again".into(),
            ));
        }

        let now = now_ms();
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin().await?;
        for (index, hash) in model_hashes.iter().enumerate() {
            sqlx::query(
                "UPDATE model_configs SET sort_order = ?, updated_at_ms = ? WHERE model_hash = ?",
            )
            .bind(i64::try_from(index + 1).expect("model order fits in i64"))
            .bind(now)
            .bind(hash)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        self.models().await
    }
}

/// 分配模型身份：创建时随机分配，之后永久保留，不再按配置内容推导。
///
/// 8 字节十六进制与旧的内容哈希同形；对客户端与已保存的引用而言它只是不透明标识。
/// 与已有行比对一次，避免极小概率的重复（同一事务里的插入也在这个查询里可见）。
async fn allocate_model_hash(transaction: &mut Transaction<'_, Sqlite>) -> Result<String> {
    for _ in 0..8 {
        let hash = hex::encode(&uuid::Uuid::new_v4().as_bytes()[..8]);
        let taken: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM model_configs WHERE model_hash = ?")
                .bind(&hash)
                .fetch_optional(&mut **transaction)
                .await?;
        if taken.is_none() {
            return Ok(hash);
        }
    }
    Err(Error::Config("cannot allocate a model identity".into()))
}

async fn insert_model(
    transaction: &mut Transaction<'_, Sqlite>,
    hash: &str,
    input: &ModelConfigInput,
    now: i64,
) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO model_configs(
            model_hash, sort_order, display_name, group_name, model_type, base_url, use_full_url, api_key, tooltip_data,
            model_id, reasoning_effort, openai_endpoint, openai_extra_params_enabled,
            openai_extra_params_json, custom_headers_enabled, custom_headers_json,
            anthropic_extra_params_enabled, anthropic_extra_params_json, context_window_tokens,
            max_completion_tokens, anthropic_max_tokens, anthropic_thinking_effort,
            thinking_budget_tokens, created_at_ms, updated_at_ms
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(hash)
    .bind(input.sort_order)
    .bind(&input.display_name)
    .bind(&input.group_name)
    .bind(input.model_type.as_str())
    .bind(&input.base_url)
    .bind(input.use_full_url)
    .bind(&input.api_key)
    .bind(&input.tooltip_data)
    .bind(&input.model_id)
    .bind(&input.reasoning_effort)
    .bind(&input.openai_endpoint)
    .bind(input.openai_extra_params_enabled)
    .bind(serde_json::to_string(&input.openai_extra_params)?)
    .bind(input.custom_headers_enabled)
    .bind(serde_json::to_string(&input.custom_headers)?)
    .bind(input.anthropic_extra_params_enabled)
    .bind(serde_json::to_string(&input.anthropic_extra_params)?)
    .bind(input.context_window_tokens.map(to_i64).transpose()?)
    .bind(input.max_completion_tokens.map(to_i64).transpose()?)
    .bind(input.anthropic_max_tokens.map(to_i64).transpose()?)
    .bind(&input.anthropic_thinking_effort)
    .bind(input.thinking_budget_tokens.map(to_i64).transpose()?)
    .bind(now)
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn model_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ModelConfig> {
    Ok(ModelConfig {
        model_hash: row.try_get("model_hash")?,
        sort_order: row.try_get("sort_order")?,
        display_name: row.try_get("display_name")?,
        group_name: row.try_get("group_name")?,
        model_type: ModelType::from_str(row.try_get("model_type")?)?,
        base_url: row.try_get("base_url")?,
        use_full_url: row.try_get("use_full_url")?,
        api_key: row.try_get("api_key")?,
        tooltip_data: row.try_get("tooltip_data")?,
        model_id: row.try_get("model_id")?,
        reasoning_effort: row.try_get("reasoning_effort")?,
        openai_endpoint: row.try_get("openai_endpoint")?,
        openai_extra_params_enabled: row.try_get("openai_extra_params_enabled")?,
        openai_extra_params: serde_json::from_str(
            row.try_get::<String, _>("openai_extra_params_json")?
                .as_str(),
        )?,
        custom_headers_enabled: row.try_get("custom_headers_enabled")?,
        custom_headers: serde_json::from_str(
            row.try_get::<String, _>("custom_headers_json")?.as_str(),
        )?,
        anthropic_extra_params_enabled: row.try_get("anthropic_extra_params_enabled")?,
        anthropic_extra_params: serde_json::from_str(
            row.try_get::<String, _>("anthropic_extra_params_json")?
                .as_str(),
        )?,
        context_window_tokens: optional_u64(&row, "context_window_tokens")?,
        max_completion_tokens: optional_u64(&row, "max_completion_tokens")?,
        anthropic_max_tokens: optional_u64(&row, "anthropic_max_tokens")?,
        anthropic_thinking_effort: row.try_get("anthropic_thinking_effort")?,
        thinking_budget_tokens: optional_u64(&row, "thinking_budget_tokens")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

fn optional_u64(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<Option<u64>> {
    row.try_get::<Option<i64>, _>(column)?
        .map(|value| {
            u64::try_from(value).map_err(|_| Error::Config(format!("{column} cannot be negative")))
        })
        .transpose()
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Config("token value is too large".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        devin::{DevinModelBinding, DevinSettings},
        model::{NewLlmCall, ProviderType},
        store::CommitSettings,
    };

    /// 每个用例一份真实临时库:迁移、约束与写事务都按生产路径执行。
    async fn test_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        (directory, store)
    }

    fn model_input(group_name: Option<&str>) -> ModelConfigInput {
        ModelConfigInput {
            sort_order: 0,
            display_name: "Test Model".into(),
            group_name: group_name.map(String::from),
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "test-key".into(),
            tooltip_data: "Test Model".into(),
            model_id: "test-model".into(),
            reasoning_effort: None,
            openai_endpoint: crate::model::OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        }
    }

    /// 分组名是纯展示字段:入库时去除首尾空白、空串归一为 NULL,
    /// 更新分组名不得改变模型身份哈希。
    #[tokio::test]
    async fn group_name_round_trips_without_changing_model_identity() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();

        let created = store
            .create_model(&model_input(Some("  My Group  ")))
            .await
            .unwrap();
        assert_eq!(created.group_name.as_deref(), Some("My Group"));

        let renamed = store
            .update_model(&created.model_hash, &model_input(Some("Renamed")))
            .await
            .unwrap();
        assert_eq!(renamed.model_hash, created.model_hash);
        assert_eq!(renamed.group_name.as_deref(), Some("Renamed"));

        let cleared = store
            .update_model(&created.model_hash, &model_input(Some("   ")))
            .await
            .unwrap();
        assert_eq!(cleared.model_hash, created.model_hash);
        assert_eq!(cleared.group_name, None);
    }

    /// 编辑表单不回填明文 key：留空必须沿用已保存的凭据，而不是清空或报错。
    #[tokio::test]
    async fn an_empty_api_key_on_update_keeps_the_stored_credential() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let created = store.create_model(&model_input(None)).await.unwrap();

        let updated = store
            .update_model(
                &created.model_hash,
                &ModelConfigInput {
                    api_key: "   ".into(),
                    context_window_tokens: Some(123_456),
                    ..model_input(None)
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.api_key, "test-key");
        assert_eq!(updated.context_window_tokens, Some(123_456));
        // 凭据没变，其余字段也没动到身份：模型哈希保持原样。
        assert_eq!(updated.model_hash, created.model_hash);
        let stored: String =
            sqlx::query_scalar("SELECT api_key FROM model_configs WHERE model_hash = ?")
                .bind(&updated.model_hash)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(stored, "test-key");
    }

    /// 复制模型要连凭据一起复制。凭据不回传前端，所以这是服务端的动作。
    #[tokio::test]
    async fn duplicating_a_model_copies_the_credential_under_a_new_identity() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let created = store.create_model(&model_input(None)).await.unwrap();

        let copy = store
            .duplicate_model(&created.model_hash, "Test Model 副本")
            .await
            .unwrap();

        assert_ne!(copy.model_hash, created.model_hash);
        assert_eq!(copy.display_name, "Test Model 副本");
        assert_eq!(copy.api_key, "test-key");
        assert_eq!(copy.base_url, created.base_url);
        assert_eq!(copy.model_id, created.model_id);
        assert_eq!(copy.sort_order, created.sort_order + 1);
        assert_eq!(store.models().await.unwrap().len(), 2);
    }

    /// 模型身份在创建后不可变:改名、换地址/上游模型 ID、轮换凭据都不改写主键。
    /// Devin 绑定、提交消息设置与历史调用都按这个标识保存引用,必须继续解析到同一行。
    #[tokio::test]
    async fn editing_a_model_keeps_its_identity_and_saved_references() {
        let (_directory, store) = test_store().await;
        let created = store.create_model(&model_input(None)).await.unwrap();
        store
            .set_devin_settings(DevinSettings {
                bindings: vec![DevinModelBinding::new(
                    "devin-model",
                    created.model_hash.clone(),
                )],
                ..DevinSettings::default()
            })
            .await
            .unwrap();
        store
            .set_commit_settings(CommitSettings {
                model_id: created.model_hash.clone(),
                ..CommitSettings::default()
            })
            .await
            .unwrap();
        store
            .start_llm_call(&NewLlmCall {
                client_source: crate::model::CallClient::Internal,
                call_id: "call-1".into(),
                run_id: "run-1".into(),
                conversation_id: "conversation-1".into(),
                provider_call_index: 0,
                model_hash: created.model_hash.clone(),
                provider_type: ProviderType::OpenAiChat,
                provider_url: "https://example.com".into(),
                request_type: ProviderType::OpenAiChat,
                request_url: "https://example.com/v1/chat/completions".into(),
                model_id: "test-model".into(),
                display_name: "Test Model".into(),
                reasoning_effort: None,
                fast: false,
                message_count: 1,
                tool_count: 0,
                detailed: false,
            })
            .await
            .unwrap();

        let updated = store
            .update_model(
                &created.model_hash,
                &ModelConfigInput {
                    display_name: "Renamed Model".into(),
                    base_url: "https://relay.example.com/v1/chat/completions".into(),
                    model_id: "test-model-v2".into(),
                    api_key: "rotated-key".into(),
                    ..model_input(None)
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.model_hash, created.model_hash);
        assert_eq!(updated.display_name, "Renamed Model");
        assert_eq!(updated.api_key, "rotated-key");
        // 改的仍是同一行,不是新建。
        assert_eq!(store.models().await.unwrap().len(), 1);
        let stored = store
            .model(&created.model_hash)
            .await
            .unwrap()
            .expect("the original identity must keep resolving");
        assert_eq!(stored.display_name, "Renamed Model");
        assert_eq!(stored.model_id, "test-model-v2");
        assert_eq!(stored.api_key, "rotated-key");

        // Devin 与提交消息的已保存引用仍然可解析。
        let devin = store.devin_settings().await.unwrap();
        assert_eq!(devin.bindings[0].effective_model_hash(), created.model_hash);
        let commit = store.commit_settings().await.unwrap();
        assert_eq!(commit.model_id, created.model_hash);
        assert!(store.model(&commit.model_id).await.unwrap().is_some());

        // 历史调用没有被解除关联。
        let call = store.llm_call("call-1").await.unwrap().unwrap();
        assert_eq!(
            call.model_hash.as_deref(),
            Some(created.model_hash.as_str())
        );
    }

    /// 编辑过的模型不再持有它最初的配置:用最初的配置再建一份必须是新模型,
    /// 既不能因为"指纹曾是那条模型的身份"被判重复,也不能覆盖已编辑的那行。
    #[tokio::test]
    async fn recreating_the_original_configuration_after_an_edit_creates_a_new_model() {
        let (_directory, store) = test_store().await;
        let original = model_input(None);
        let created = store.create_model(&original).await.unwrap();
        let edited = store
            .update_model(
                &created.model_hash,
                &ModelConfigInput {
                    display_name: "Renamed Model".into(),
                    api_key: "rotated-key".into(),
                    ..original.clone()
                },
            )
            .await
            .unwrap();
        assert_eq!(edited.model_hash, created.model_hash);

        let recreated = store.create_model(&original).await.unwrap();

        assert_ne!(recreated.model_hash, created.model_hash);
        assert_eq!(recreated.display_name, "Test Model");
        assert_eq!(recreated.api_key, "test-key");
        assert_eq!(store.models().await.unwrap().len(), 2);
        let edited_row = store.model(&created.model_hash).await.unwrap().unwrap();
        assert_eq!(edited_row.display_name, "Renamed Model");
        assert_eq!(edited_row.api_key, "rotated-key");
    }

    /// 去重按配置内容判断:库里已有同样配置时,再次创建同样的配置必须被拒绝
    /// （配置类错误）,而不是落成第二份同样的模型。
    #[tokio::test]
    async fn creating_a_model_whose_configuration_already_exists_is_rejected() {
        let (_directory, store) = test_store().await;
        let created = store.create_model(&model_input(None)).await.unwrap();

        let error = store.create_model(&model_input(None)).await.unwrap_err();

        assert!(
            matches!(error, Error::Config(_)),
            "unexpected error: {error}"
        );
        let models = store.models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_hash, created.model_hash);
    }

    /// 同一批创建里出现两份相同配置同样被拒绝,不因身份改为分配制就落成两行。
    #[tokio::test]
    async fn creating_the_same_configuration_twice_in_one_batch_is_rejected() {
        let (_directory, store) = test_store().await;

        let error = store
            .create_models(&[model_input(None), model_input(None)])
            .await
            .unwrap_err();

        assert!(
            matches!(error, Error::Config(_)),
            "unexpected error: {error}"
        );
        assert!(store.models().await.unwrap().is_empty());
    }

    /// 把一条模型编辑成另一条模型已有的配置必须被拒绝:身份不再随配置变化,
    /// 否则库里会静默出现两份相同配置。
    #[tokio::test]
    async fn editing_a_model_into_another_models_configuration_is_rejected() {
        let (_directory, store) = test_store().await;
        let first = store.create_model(&model_input(None)).await.unwrap();
        let second = store
            .create_model(&ModelConfigInput {
                display_name: "Second Model".into(),
                ..model_input(None)
            })
            .await
            .unwrap();

        let error = store
            .update_model(
                &first.model_hash,
                &ModelConfigInput {
                    display_name: "Second Model".into(),
                    ..model_input(None)
                },
            )
            .await
            .unwrap_err();

        assert!(
            matches!(error, Error::Config(_)),
            "unexpected error: {error}"
        );
        assert_eq!(
            store
                .model(&first.model_hash)
                .await
                .unwrap()
                .unwrap()
                .display_name,
            "Test Model"
        );
        assert_eq!(
            store
                .model(&second.model_hash)
                .await
                .unwrap()
                .unwrap()
                .display_name,
            "Second Model"
        );
    }

    /// 复制模型的序号必须逐次递增：算序号与插入在同一把写锁里，两次复制不可能拿到同一个
    /// 序号（旧写法在锁外读 `MAX(sort_order)`，并发复制会插出两条并列配置）。
    #[tokio::test]
    async fn duplicating_a_model_assigns_a_strictly_greater_order() {
        let (_directory, store) = test_store().await;
        let source = store.create_model(&model_input(None)).await.unwrap();

        let first = store
            .duplicate_model(&source.model_hash, "Copy A")
            .await
            .unwrap();
        let second = store
            .duplicate_model(&source.model_hash, "Copy B")
            .await
            .unwrap();

        assert!(first.sort_order > source.sort_order, "{}", first.sort_order);
        assert!(
            second.sort_order > first.sort_order,
            "复制序号必须严格递增: {} vs {}",
            second.sort_order,
            first.sort_order
        );
        assert_ne!(first.model_hash, second.model_hash);
    }

    /// 模型不存在时回的是"配置问题 + 下一步"，不再是 `run not found`。
    ///
    /// 界面把 `RunNotFound` 原样弹成 "run not found"：用户既不知道是模型没了，也不知道该
    /// 做什么。这条错误必须说清是模型、并给出"刷新模型列表"这个动作。
    #[tokio::test]
    async fn a_missing_model_reports_an_actionable_configuration_error() {
        let (_directory, store) = test_store().await;

        let error = store
            .duplicate_model("no-such-model", "Copy")
            .await
            .expect_err("模型不存在必须报错");

        assert!(matches!(error, Error::Config(_)), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("no-such-model"), "{message}");
        assert!(message.contains("reload the model list"), "{message}");
    }
}
