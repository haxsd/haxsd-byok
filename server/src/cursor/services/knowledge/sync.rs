//! Replays the offline journal to upstream and mirrors upstream list state.
use std::collections::HashMap;

use axum::{
    body::{Body, Bytes},
    http::{header, HeaderMap, HeaderValue, Method, Request},
};
use prost::Message;

use crate::{api::cursor::proxy, cursor::protocol::connect, Result};

use super::{
    store::{JournalOp, RuleRecord, RuleStore},
    KnowledgeBaseAddRequest, KnowledgeBaseAddResponse, KnowledgeBaseListItem,
    KnowledgeBaseRemoveRequest, KnowledgeBaseRemoveResponse, KnowledgeBaseUpdateRequest,
    KnowledgeBaseUpdateResponse,
};

const ADD_PATH: &str = "/aiserver.v1.AiService/KnowledgeBaseAdd";
const UPDATE_PATH: &str = "/aiserver.v1.AiService/KnowledgeBaseUpdate";
const REMOVE_PATH: &str = "/aiserver.v1.AiService/KnowledgeBaseRemove";

/// 逐条把离线日志推送到上游。返回 true 表示日志已清空(上游可用),
/// false 表示上游不可达,剩余日志保留、调用方应降级到本地。
pub async fn replay(
    upstream: &proxy::CursorProxy,
    headers: &HeaderMap,
    store: &RuleStore,
) -> Result<bool> {
    while let Some(entry) = store.journal_front()? {
        let advanced = match entry.op {
            JournalOp::Add => replay_add(upstream, headers, store, &entry.id).await?,
            JournalOp::Update => replay_update(upstream, headers, store, &entry.id).await?,
            JournalOp::Remove => replay_remove(upstream, headers, store, &entry.id).await?,
        };
        if !advanced {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 用上游返回的完整列表覆盖本地镜像。仅应在日志已清空时调用。
///
/// 上游的列表项里没有 `git_origin`（`KnowledgeBaseListItem` 没有这个字段），整体覆盖会把
/// 本地已知的仓库归属抹成空串——离线时按仓库过滤（`KnowledgeBaseListRequest.git_origin`）
/// 就永远返回空列表。这里按 id 保留本地已有的值：上游既然不给，本地那份就是唯一来源。
pub fn mirror(store: &RuleStore, items: Vec<KnowledgeBaseListItem>) -> Result<()> {
    let known = store
        .list()?
        .into_iter()
        .map(|record| (record.id, record.git_origin))
        .collect::<HashMap<_, _>>();
    let records = items
        .into_iter()
        .map(|item| RuleRecord {
            git_origin: known.get(&item.id).cloned().unwrap_or_default(),
            id: item.id,
            knowledge: item.knowledge,
            title: item.title,
            created_at: item.created_at,
            is_generated: item.is_generated,
        })
        .collect::<Vec<_>>();
    store.replace_all(&records)
}

async fn replay_add(
    upstream: &proxy::CursorProxy,
    headers: &HeaderMap,
    store: &RuleStore,
    id: &str,
) -> Result<bool> {
    let Some(record) = store.get(id)? else {
        // 规则文件已不在(被手动删除等),日志作废。
        store.pop_journal()?;
        return Ok(true);
    };
    let message = KnowledgeBaseAddRequest {
        knowledge: record.knowledge,
        title: record.title,
        git_origin: record.git_origin,
        composer_id: None,
    };
    let Some(body) = send(upstream, headers, ADD_PATH, &message).await else {
        return Ok(false);
    };
    let Ok(reply) = connect::decode_unary::<KnowledgeBaseAddResponse>(&body) else {
        return Ok(false);
    };
    if !reply.success || reply.id.is_empty() {
        tracing::warn!(
            id,
            "rules upstream declined replayed add; dropping journal entry"
        );
        store.pop_journal()?;
        return Ok(true);
    }
    commit_replayed_add(store, id, &reply.id);
    tracing::info!(
        local_id = id,
        upstream_id = reply.id,
        "replayed offline rule add to upstream"
    );
    Ok(true)
}

/// 上游已接受这条 add 之后的本地收尾。
///
/// 顺序是刻意的：先把日志标记为已提交（`pop_journal` 一次写盘），再做本地改名。反过来的话
/// `promote` 失败会让日志留着，下一次 replay 把同一条规则再 add 一遍——上游出现重复规则，
/// 而用户只看到"重试成功了"。改名失败本身只记警告：规则内容已经在上游，本地这份镜像的
/// 文件名不是数据本身。
fn commit_replayed_add(store: &RuleStore, local_id: &str, upstream_id: &str) {
    if let Err(error) = store.pop_journal() {
        tracing::warn!(%error, "cannot mark the replayed rule add as committed");
    }
    if let Err(error) = store.promote(local_id, upstream_id) {
        tracing::warn!(
            local_id,
            upstream_id,
            %error,
            "rules upstream accepted the replayed add but the local mirror could not be renamed"
        );
    }
}

async fn replay_update(
    upstream: &proxy::CursorProxy,
    headers: &HeaderMap,
    store: &RuleStore,
    id: &str,
) -> Result<bool> {
    let Some(record) = store.get(id)? else {
        store.pop_journal()?;
        return Ok(true);
    };
    let message = KnowledgeBaseUpdateRequest {
        id: id.into(),
        knowledge: record.knowledge,
        title: record.title,
    };
    let Some(body) = send(upstream, headers, UPDATE_PATH, &message).await else {
        return Ok(false);
    };
    let Ok(reply) = connect::decode_unary::<KnowledgeBaseUpdateResponse>(&body) else {
        return Ok(false);
    };
    if !reply.success {
        tracing::warn!(
            id,
            "rules upstream declined replayed update; dropping journal entry"
        );
    }
    store.pop_journal()?;
    Ok(true)
}

async fn replay_remove(
    upstream: &proxy::CursorProxy,
    headers: &HeaderMap,
    store: &RuleStore,
    id: &str,
) -> Result<bool> {
    let message = KnowledgeBaseRemoveRequest { id: id.into() };
    let Some(body) = send(upstream, headers, REMOVE_PATH, &message).await else {
        return Ok(false);
    };
    let Ok(reply) = connect::decode_unary::<KnowledgeBaseRemoveResponse>(&body) else {
        return Ok(false);
    };
    if !reply.success {
        tracing::warn!(
            id,
            "rules upstream declined replayed remove; dropping journal entry"
        );
    }
    store.pop_journal()?;
    Ok(true)
}

/// 以当前请求的头为模板向上游发起一次 unary RPC。
/// 成功(2xx)返回响应体;不可达或被拒绝返回 None,由调用方保留日志。
async fn send(
    upstream: &proxy::CursorProxy,
    template: &HeaderMap,
    path: &str,
    message: &impl Message,
) -> Option<Bytes> {
    let mut headers = template.clone();
    // 模板里的上游 URL 头指向原始 RPC 路径,必须移除才能命中回放路径。
    headers.remove(proxy::UPSTREAM_URL_HEADER);
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    let mut request = Request::new(Body::from(message.encode_to_vec()));
    *request.method_mut() = Method::POST;
    *request.uri_mut() = path.parse().expect("replay path is a valid URI");
    *request.headers_mut() = headers;

    match proxy::forward_buffered(upstream, request).await {
        Ok(response) if response.status.is_success() => Some(response.body),
        Ok(response) => {
            tracing::warn!(path, status = %response.status, "rules journal replay rejected by upstream");
            None
        }
        Err(error) => {
            tracing::warn!(path, %error, "rules journal replay cannot reach upstream");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, knowledge: &str) -> KnowledgeBaseListItem {
        KnowledgeBaseListItem {
            id: id.into(),
            knowledge: knowledge.into(),
            title: format!("title-{id}"),
            created_at: "2026-02-01T00:00:00.000Z".into(),
            is_generated: false,
        }
    }

    fn stored(id: &str, knowledge: &str, git_origin: &str) -> RuleRecord {
        RuleRecord {
            id: id.into(),
            knowledge: knowledge.into(),
            title: format!("title-{id}"),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            is_generated: false,
            git_origin: git_origin.into(),
        }
    }

    /// 上游列表项不带 `git_origin`：覆盖镜像时要保留本地已知的仓库归属，
    /// 否则离线时按仓库过滤恒为空（上游此时不可达，本地是唯一数据源）。
    #[test]
    fn mirror_keeps_the_local_git_origin_per_rule() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        let origin = "https://github.com/acme/repo";
        store.upsert(&stored("42", "old", origin)).unwrap();

        mirror(
            &store,
            vec![item("42", "from upstream"), item("43", "new rule")],
        )
        .unwrap();

        let updated = store.get("42").unwrap().unwrap();
        assert_eq!(updated.knowledge, "from upstream");
        assert_eq!(updated.git_origin, origin);
        assert_eq!(
            store.get("43").unwrap().unwrap().git_origin,
            "",
            "本地没有的规则只能留空"
        );

        // 离线过滤走的就是这个字段：镜像之后仍要能按仓库命中。
        let filtered = store
            .list()
            .unwrap()
            .into_iter()
            .filter(|record| record.git_origin == origin)
            .map(|record| record.id)
            .collect::<Vec<_>>();
        assert_eq!(filtered, ["42"]);
    }

    /// 上游接受 add 之后先标记日志已提交，再本地改名。
    #[test]
    fn committing_a_replayed_add_pops_the_journal_before_promoting() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&stored("local-1", "offline rule", ""))
            .unwrap();
        store.record_add("local-1").unwrap();

        commit_replayed_add(&store, "local-1", "42");

        assert!(store.journal_front().unwrap().is_none(), "日志要清掉");
        assert!(store.get("42").unwrap().is_some(), "本地镜像要换成上游 id");
        assert!(store.get("local-1").unwrap().is_none());
    }

    /// 改名失败不能把日志留下：留着的话下一次 replay 会再 add 一遍，上游出现重复规则。
    #[test]
    fn a_failed_promote_still_marks_the_replayed_add_as_committed() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store
            .upsert(&stored("local-1", "offline rule", ""))
            .unwrap();
        store.record_add("local-1").unwrap();

        // 上游 id 非法（带路径分隔符）会让 promote 失败，但上游确实已经建好了那条规则。
        commit_replayed_add(&store, "local-1", "../escaped");

        assert!(
            store.journal_front().unwrap().is_none(),
            "上游已经接受，日志不能再留着重放一次"
        );
    }
}
