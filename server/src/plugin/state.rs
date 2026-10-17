//! Owns core-side persistence of plugin resources and model catalogs.
use serde::{Deserialize, Serialize};

use super::data::PluginDataStore;
use crate::{Error, Result};

/// 核心理解的资源运行状态;插件只能通过 draft/patch/report 改变它。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResourceState {
    Ready,
    Cooling {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_at_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Invalid {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl ResourceState {
    /// 冷却到期后自动恢复可用。
    pub fn is_ready(&self, now_ms: i64) -> bool {
        match self {
            Self::Ready => true,
            Self::Cooling { retry_at_ms, .. } => retry_at_ms.is_some_and(|at| at <= now_ms),
            Self::Invalid { .. } => false,
        }
    }
}

/// 核心持久化的一条插件资源。`private_data` 只回传给插件。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceRecord {
    pub id: String,
    pub key: String,
    pub private_data: serde_json::Value,
    pub state: ResourceState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl ResourceRecord {
    /// 传给插件的快照形状(SDK 的 ResourceSnapshot)。
    pub fn snapshot(&self, resource_type: &str) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "type": resource_type,
            "key": self.key,
            "privateData": self.private_data,
            "state": state_json(&self.state),
        })
    }
}

fn state_json(state: &ResourceState) -> serde_json::Value {
    match state {
        ResourceState::Ready => serde_json::json!({ "status": "ready" }),
        ResourceState::Cooling {
            retry_at_ms,
            message,
        } => serde_json::json!({
            "status": "cooling",
            "retryAtMs": retry_at_ms,
            "message": message,
        }),
        ResourceState::Invalid { message } => serde_json::json!({
            "status": "invalid",
            "message": message,
        }),
    }
}

/// 插件返回的新资源(SDK 的 ResourceDraft)。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceDraft {
    pub key: String,
    pub private_data: serde_json::Value,
    #[serde(default)]
    pub state: Option<ResourceStateInput>,
}

/// 插件对单条资源的部分更新(SDK 的 ResourcePatch)。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourcePatch {
    #[serde(default)]
    pub private_data: Option<serde_json::Value>,
    #[serde(default)]
    pub state: Option<ResourceStateInput>,
}

/// SDK 侧 camelCase 状态输入,转换成核心存储形状。
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ResourceStateInput {
    Ready,
    Cooling {
        #[serde(default, rename = "retryAtMs")]
        retry_at_ms: Option<i64>,
        #[serde(default)]
        message: Option<String>,
    },
    Invalid {
        #[serde(default)]
        message: Option<String>,
    },
}

impl From<ResourceStateInput> for ResourceState {
    fn from(input: ResourceStateInput) -> Self {
        match input {
            ResourceStateInput::Ready => Self::Ready,
            ResourceStateInput::Cooling {
                retry_at_ms,
                message,
            } => Self::Cooling {
                retry_at_ms,
                message,
            },
            ResourceStateInput::Invalid { message } => Self::Invalid { message },
        }
    }
}

/// 插件发现的一个模型(SDK 的 ModelDefinition),由核心整体替换目录。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredModel {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub images: bool,
    #[serde(default = "default_model_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub private_data: serde_json::Value,
}

fn default_model_enabled() -> bool {
    true
}

impl StoredModel {
    pub fn from_definition(value: &serde_json::Value) -> Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| Error::Protocol("plugin model definition must be an object".into()))?;
        let id = object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| Error::Protocol("plugin model definition requires id".into()))?;
        let display_name = object
            .get("displayName")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                Error::Protocol("plugin model definition requires displayName".into())
            })?;
        let capabilities = object
            .get("capabilities")
            .and_then(|value| value.as_object());
        let capability = |name: &str| {
            capabilities
                .and_then(|value| value.get(name))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        };
        Ok(Self {
            id: id.to_owned(),
            display_name: display_name.to_owned(),
            description: object
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            max_output_tokens: object
                .get("maxOutputTokens")
                .and_then(serde_json::Value::as_u64),
            images: capability("images"),
            enabled: true,
            private_data: object
                .get("privateData")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        })
    }

    /// 传给插件的模型快照(SDK 的 ModelSnapshot)。
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "displayName": self.display_name,
            "description": self.description,
            "maxOutputTokens": self.max_output_tokens,
            "capabilities": { "images": self.images },
            "privateData": self.private_data,
        })
    }
}

/// 资源与模型目录的核心存储,构建在插件私有 JSON 文件之上。
#[derive(Clone)]
pub struct PluginStateStore {
    data: PluginDataStore,
}

pub struct UpsertOutcome {
    pub added: usize,
    pub updated: usize,
}

impl PluginStateStore {
    pub fn new(data: PluginDataStore) -> Self {
        Self { data }
    }

    pub async fn resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
    ) -> Result<Vec<ResourceRecord>> {
        let value = self
            .data
            .read(plugin_id, &resource_key(resource_type))
            .await?;
        decode_records(value)
    }

    /// 用插件上报的草稿合并资源。整段读改写在一个临界区里（`PluginDataStore::modify`）：
    /// 宿主会并发调用同一插件，分两次加锁会让后写的用旧快照盖掉先写的。
    pub async fn upsert_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
        drafts: Vec<ResourceDraft>,
    ) -> Result<UpsertOutcome> {
        self.data
            .modify(plugin_id, &resource_key(resource_type), |current| {
                let mut records = decode_records::<ResourceRecord>(current)?;
                let now = now_ms();
                let mut outcome = UpsertOutcome {
                    added: 0,
                    updated: 0,
                };
                for draft in drafts {
                    if draft.key.trim().is_empty() {
                        return Err(Error::Protocol("plugin resource draft requires key".into()));
                    }
                    let state = draft
                        .state
                        .map_or(ResourceState::Ready, ResourceState::from);
                    match records.iter_mut().find(|record| record.key == draft.key) {
                        Some(existing) => {
                            existing.private_data = draft.private_data;
                            existing.state = state;
                            existing.updated_at_ms = now;
                            outcome.updated += 1;
                        }
                        None => {
                            records.push(ResourceRecord {
                                id: uuid::Uuid::new_v4().to_string(),
                                key: draft.key,
                                private_data: draft.private_data,
                                state,
                                created_at_ms: now,
                                updated_at_ms: now,
                            });
                            outcome.added += 1;
                        }
                    }
                }
                Ok((serde_json::to_value(records)?, outcome))
            })
            .await
    }

    pub async fn apply_patch(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
        patch: ResourcePatch,
    ) -> Result<()> {
        self.data
            .modify(plugin_id, &resource_key(resource_type), |current| {
                let mut records = decode_records::<ResourceRecord>(current)?;
                let record = records
                    .iter_mut()
                    .find(|record| record.id == resource_id)
                    .ok_or_else(|| Error::RunNotFound(format!("plugin resource {resource_id}")))?;
                if let Some(private_data) = patch.private_data {
                    record.private_data = private_data;
                }
                if let Some(state) = patch.state {
                    record.state = state.into();
                }
                record.updated_at_ms = now_ms();
                Ok((serde_json::to_value(records)?, ()))
            })
            .await
    }

    pub async fn remove_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<ResourceRecord> {
        self.data
            .modify(plugin_id, &resource_key(resource_type), |current| {
                let mut records = decode_records::<ResourceRecord>(current)?;
                let index = records
                    .iter()
                    .position(|record| record.id == resource_id)
                    .ok_or_else(|| Error::RunNotFound(format!("plugin resource {resource_id}")))?;
                let removed = records.remove(index);
                Ok((serde_json::to_value(&records)?, removed))
            })
            .await
    }

    pub async fn models(&self, plugin_id: &str, provider_id: &str) -> Result<Vec<StoredModel>> {
        let value = self.data.read(plugin_id, &model_key(provider_id)).await?;
        decode_records(value)
    }

    /// 整体替换模型目录，但保留本地已有的启用开关。读改写同样在一个临界区里。
    pub async fn replace_models(
        &self,
        plugin_id: &str,
        provider_id: &str,
        models: &[StoredModel],
    ) -> Result<()> {
        self.data
            .modify(plugin_id, &model_key(provider_id), |current| {
                let previous = decode_records::<StoredModel>(current)?;
                let models = models
                    .iter()
                    .cloned()
                    .map(|mut model| {
                        if let Some(old) = previous.iter().find(|old| old.id == model.id) {
                            model.enabled = old.enabled;
                        }
                        model
                    })
                    .collect::<Vec<_>>();
                Ok((serde_json::to_value(models)?, ()))
            })
            .await
    }

    pub async fn set_model_enabled(
        &self,
        plugin_id: &str,
        provider_id: &str,
        model_id: &str,
        enabled: bool,
    ) -> Result<()> {
        self.data
            .modify(plugin_id, &model_key(provider_id), |current| {
                let mut models = decode_records::<StoredModel>(current)?;
                let model = models
                    .iter_mut()
                    .find(|model| model.id == model_id)
                    .ok_or_else(|| Error::RunNotFound(format!("plugin model {model_id}")))?;
                model.enabled = enabled;
                Ok((serde_json::to_value(models)?, ()))
            })
            .await
    }

    pub async fn clear(&self, plugin_id: &str) -> Result<()> {
        self.data.clear(plugin_id).await
    }
}

/// 存储里的 JSON 解成记录列表：文件缺失（`Null`）等于空表。
fn decode_records<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<Vec<T>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_value(value)?)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn resource_key(resource_type: &str) -> String {
    format!("resources-{resource_type}")
}

fn model_key(provider_id: &str) -> String {
    format!("models-{provider_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PluginStateStore) {
        let root = tempfile::tempdir().unwrap();
        let data = PluginDataStore::for_test(root.path().join("data")).unwrap();
        (root, PluginStateStore::new(data))
    }

    #[tokio::test]
    async fn upserts_resources_by_key_and_applies_patches() {
        let (_root, store) = store();
        let outcome = store
            .upsert_resources(
                "dev.example",
                "account",
                vec![ResourceDraft {
                    key: "acct-1".into(),
                    private_data: serde_json::json!({"token":"one"}),
                    state: None,
                }],
            )
            .await
            .unwrap();
        assert_eq!(outcome.added, 1);
        let outcome = store
            .upsert_resources(
                "dev.example",
                "account",
                vec![ResourceDraft {
                    key: "acct-1".into(),
                    private_data: serde_json::json!({"token":"two"}),
                    state: None,
                }],
            )
            .await
            .unwrap();
        assert_eq!(outcome.updated, 1);
        let records = store.resources("dev.example", "account").await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].private_data["token"], "two");

        store
            .apply_patch(
                "dev.example",
                "account",
                &records[0].id,
                ResourcePatch {
                    private_data: None,
                    state: Some(ResourceStateInput::Cooling {
                        retry_at_ms: Some(200),
                        message: None,
                    }),
                },
            )
            .await
            .unwrap();
        let records = store.resources("dev.example", "account").await.unwrap();
        assert!(!records[0].state.is_ready(100));
        assert!(records[0].state.is_ready(300), "cooling expires over time");
    }

    #[tokio::test]
    async fn replaces_model_catalogs() {
        let (_root, store) = store();
        let model = StoredModel::from_definition(&serde_json::json!({
            "id": "gpt-test",
            "displayName": "GPT Test",
            "capabilities": {"images": true},
            "privateData": {"reasoningEfforts": ["low"]},
        }))
        .unwrap();
        store
            .replace_models("dev.example", "codex", &[model])
            .await
            .unwrap();
        let models = store.models("dev.example", "codex").await.unwrap();
        assert_eq!(models.len(), 1);
        assert!(models[0].images);
        assert_eq!(models[0].private_data["reasoningEfforts"][0], "low");
    }

    /// 整体替换目录时要保留本地已有的启用开关：插件的刷新不该把用户的停用选择改回去。
    #[tokio::test]
    async fn replacing_models_keeps_the_enabled_switch() {
        let (_root, store) = store();
        let model = StoredModel::from_definition(&serde_json::json!({
            "id": "gpt-test",
            "displayName": "GPT Test",
        }))
        .unwrap();
        store
            .replace_models("dev.example", "codex", std::slice::from_ref(&model))
            .await
            .unwrap();
        store
            .set_model_enabled("dev.example", "codex", "gpt-test", false)
            .await
            .unwrap();

        store
            .replace_models("dev.example", "codex", std::slice::from_ref(&model))
            .await
            .unwrap();

        let models = store.models("dev.example", "codex").await.unwrap();
        assert!(!models[0].enabled, "刷新目录不能把用户停用的模型改回启用");
    }

    /// 并发补丁不能互相覆盖：读改写必须在一个临界区里。这条在旧实现（读一次、写一次，
    /// 两次加锁）下会丢更新——每个任务都读到同一份快照，最后一个写回的把其他补丁盖掉。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_patches_on_one_plugin_do_not_lose_updates() {
        let (_root, store) = store();
        let drafts = (0..16)
            .map(|index| ResourceDraft {
                key: format!("acct-{index}"),
                private_data: serde_json::json!({"token": "initial"}),
                state: None,
            })
            .collect::<Vec<_>>();
        store
            .upsert_resources("dev.example", "account", drafts)
            .await
            .unwrap();
        let ids = store
            .resources("dev.example", "account")
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 16);

        let tasks = ids
            .into_iter()
            .map(|id| {
                let store = store.clone();
                tokio::spawn(async move {
                    store
                        .apply_patch(
                            "dev.example",
                            "account",
                            &id,
                            ResourcePatch {
                                private_data: Some(serde_json::json!({"token": "patched"})),
                                state: None,
                            },
                        )
                        .await
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        let records = store.resources("dev.example", "account").await.unwrap();
        let patched = records
            .iter()
            .filter(|record| record.private_data["token"] == "patched")
            .count();
        assert_eq!(patched, 16, "每个补丁都要落盘，不能被并发写覆盖掉");
    }

    /// 确定性版本：第一个 modify 卡在闭包里时，第二个 modify 必须拿不到锁——
    /// 也就是说它不会读到"读了一半"的快照。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_second_modify_cannot_enter_between_read_and_write() {
        let (_root, store) = store();
        let data = store.data.clone();
        data.update("dev.example", "counter", &serde_json::json!({"n": 0}))
            .await
            .unwrap();
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let first = {
            let data = data.clone();
            tokio::spawn(async move {
                data.modify("dev.example", "counter", move |current| {
                    entered.send(()).unwrap();
                    release_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    let next = current["n"].as_i64().unwrap() + 1;
                    Ok((serde_json::json!({"n": next}), next))
                })
                .await
            })
        };
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("第一个 modify 要进到闭包里");

        let second = {
            let data = data.clone();
            tokio::spawn(async move {
                data.modify("dev.example", "counter", |current| {
                    let next = current["n"].as_i64().unwrap() + 1;
                    Ok((serde_json::json!({"n": next}), next))
                })
                .await
            })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), second)
                .await
                .is_err(),
            "第一个读改写还没结束时，第二个不能进临界区"
        );

        release.send(()).unwrap();
        assert_eq!(first.await.unwrap().unwrap(), 1);
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), {
                let data = data.clone();
                async move { data.read("dev.example", "counter").await.unwrap() }
            })
            .await
            .unwrap()["n"],
            2
        );
    }
}
