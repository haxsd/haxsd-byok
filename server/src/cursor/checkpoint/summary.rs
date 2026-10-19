//! Builds compacted checkpoint summary state.
use prost::Message;

use crate::{
    cursor::{checkpoint::PendingSteps, protocol::proto::agent::v1 as pb},
    model::{
        estimate_context_tokens, project_messages, CanonicalMessage, MessageContent, PromptSpec,
    },
    store::{BlobEdge, BlobId},
    Error, Result,
};

use super::{messages, CheckpointBuilder};

impl CheckpointBuilder {
    /// Rebuilds Cursor state after a compaction.
    ///
    /// `messages` is the replacement history the run committed, and
    /// `summary_message_id` names the one message in it that carries the
    /// summary. Turns the run kept verbatim reuse the wire bytes they already
    /// had, so they keep their blob ids and can be told apart from the history
    /// that was summarized away.
    pub async fn compacted(
        &mut self,
        messages: &[CanonicalMessage],
        mode: i32,
        summary: &str,
        summary_message_id: &str,
        presentation: &PendingSteps,
    ) -> Result<pb::ConversationStateStructure> {
        let previous = self
            .base
            .root_prompt_messages_json
            .iter()
            .map(|id| BlobId::from_bytes(id))
            .collect::<Result<Vec<_>>>()?;
        let root_ids = self.replace_roots(messages).await?;
        let summary_message = self.summary_root(messages, summary_message_id, &root_ids)?;
        let retained = root_ids
            .iter()
            .skip(1)
            .filter(|id| **id != summary_message && previous.contains(id))
            .count();
        let summarized = previous
            .iter()
            .skip(1)
            .filter(|id| !root_ids.contains(id))
            .cloned()
            .collect::<Vec<_>>();

        let summary_id = self
            .sync
            .persist(
                &pb::ConversationSummary {
                    summary: summary.into(),
                }
                .encode_to_vec(),
                &[],
            )
            .await?;
        let archive = pb::ConversationSummaryArchive {
            summarized_messages: summarized.iter().map(|id| id.as_bytes().to_vec()).collect(),
            summary: summary.into(),
            window_tail: retained.min(u32::MAX as usize) as u32,
            summary_message: summary_message.as_bytes().to_vec(),
        };
        let mut edges = summarized
            .iter()
            .enumerate()
            .map(|(index, child)| BlobEdge {
                child: child.clone(),
                field_name: format!("summarized_messages[{index}]"),
            })
            .collect::<Vec<_>>();
        edges.push(BlobEdge {
            child: summary_message,
            field_name: "summary_message".into(),
        });
        let archive_id = self.sync.persist(&archive.encode_to_vec(), &edges).await?;
        let turn_ids = self.project_turns(mode, presentation).await?;

        for path in &presentation.read_paths {
            if !self.base.read_paths.contains(path) {
                self.base.read_paths.push(path.clone());
            }
        }
        self.base.root_prompt_messages_json =
            root_ids.iter().map(|id| id.as_bytes().to_vec()).collect();
        self.base.turns = turn_ids.iter().map(|id| id.as_bytes().to_vec()).collect();
        self.base.pending_tool_calls.clear();
        // 压缩把逐字历史换成摘要。客户端带上来的文件状态基线描述的是压缩前的会话，
        // 原样写回去会让 Cursor 侧按旧 blob 把文件显示成压缩前的样子（审计 05/P2-1）。
        // 但全部清空又会丢掉仍保留在近轮里的文件锚点，所以只保留 replacement 里仍有
        // Read/Edit 工具调用引用的那些文件；被摘要掉的文件状态继续丢弃，由之后的
        // Read/Edit 重新建立。todos / plan 不走这条：它们由 `build_derived_state`
        // 从消息折叠，没有折叠结果时沿用基线（derived.rs）。
        let retained_paths = retained_file_paths(messages);
        self.base
            .file_states
            .retain(|path, _| retained_paths.contains(&normalize_path(path)));
        self.base
            .file_states_v2
            .retain(|path, _| retained_paths.contains(&normalize_path(path)));
        self.base.mode = Some(mode);
        self.base.summary = Some(summary_id.as_bytes().to_vec());
        self.base.summary_archive = Some(archive_id.as_bytes().to_vec());
        if !self
            .base
            .summary_archives
            .contains(&archive_id.as_bytes().to_vec())
        {
            self.base
                .summary_archives
                .push(archive_id.as_bytes().to_vec());
        }
        self.base.self_summary_count = self.base.self_summary_count.saturating_add(1);
        let projected = project_messages(messages)?;
        let prompt = PromptSpec {
            instructions: self.instructions.clone(),
            tools: self.tool_definitions.clone(),
        };
        self.record_context_tokens(Some(estimate_context_tokens(&prompt, &projected)));
        let statics = self.static_measures()?;
        if let Some(details) = self.base.token_details.as_mut() {
            details.breakdown = Some(crate::cursor::services::usage::breakdown(
                details.used_tokens,
                details.max_tokens,
                details.breakdown.as_ref(),
                statics,
                messages,
                &mut self.measure_cache,
            )?);
        }
        Ok(self.base.clone())
    }

    /// The root that carries the summary message.
    ///
    /// Roots are one projected message each (a tool round collapses several
    /// canonical messages into one), so the summary is located by its own wire
    /// bytes instead of by index into `messages`.
    fn summary_root(
        &self,
        messages: &[CanonicalMessage],
        summary_message_id: &str,
        root_ids: &[BlobId],
    ) -> Result<BlobId> {
        let message = messages
            .iter()
            .find(|message| message.message_id == summary_message_id)
            .ok_or_else(|| {
                Error::Protocol(format!(
                    "compaction summary message is missing from the replacement: {summary_message_id}"
                ))
            })?;
        let wire = messages::stable_messages(
            &self.instructions,
            std::slice::from_ref(message),
            &self.model,
        )?;
        let digest = wire
            .last()
            .map(|message| BlobId::digest(message))
            .ok_or_else(|| {
                Error::Protocol("compaction summary message encoded to nothing".into())
            })?;
        root_ids
            .iter()
            .find(|id| **id == digest)
            .cloned()
            .ok_or_else(|| {
                Error::Protocol("compaction summary message is not a checkpoint root".into())
            })
    }
}

/// File paths that the post-compaction replacement still references.
///
/// Only file-tool calls in the retained messages count: a path that appears
/// only in the summarized-away history has no live state to restore, and
/// keeping its stale blob would make the client show pre-compaction contents.
fn retained_file_paths(messages: &[CanonicalMessage]) -> std::collections::HashSet<String> {
    let mut paths = std::collections::HashSet::new();
    for message in messages {
        let MessageContent::Assistant { tool_calls, .. } = &message.content else {
            continue;
        };
        for call in tool_calls {
            if let Some(path) = file_tool_path(&call.name, &call.arguments) {
                paths.insert(normalize_path(&path));
            }
        }
    }
    paths
}

/// Canonical comparison form for a file path.
///
/// `file_states` keys come from the Cursor client while the path comes from a
/// model-authored tool argument. The two spellings can differ in separators
/// (`src\\lib.rs` vs `src/lib.rs` on Windows) or in a leading `./` while naming
/// the same file. Only those spelling differences are folded away: an absolute
/// path and a relative one stay distinct, so a mismatch keeps dropping the
/// stale state (the fail-safe behaviour) instead of guessing a different file.
fn normalize_path(path: &str) -> String {
    let mut normalized = path.trim().replace('\\', "/");
    while let Some(rest) = normalized.strip_prefix("./") {
        normalized = rest.to_string();
    }
    normalized
}

fn file_tool_path(name: &str, arguments: &serde_json::Value) -> Option<String> {
    let normalized: String = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    if !matches!(
        normalized.as_str(),
        "read"
            | "readfile"
            | "edit"
            | "write"
            | "strreplace"
            | "patchedit"
            | "patcheditlines"
            | "patcheditspan"
            | "editnotebook"
            | "applypatch"
            | "multiedit"
            | "createfile"
    ) {
        return None;
    }
    for key in [
        "path",
        "file_path",
        "target_file",
        "absolute_path",
        "filename",
        "file",
    ] {
        if let Some(value) = arguments.get(key).and_then(|value| value.as_str()) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Origin, Role, ToolCallContent};

    fn assistant(calls: Vec<ToolCallContent>) -> CanonicalMessage {
        CanonicalMessage {
            message_id: "a1".into(),
            role: Role::Assistant,
            origin: Origin::Assistant,
            content: MessageContent::Assistant {
                text: String::new(),
                thinking: String::new(),
                tool_round_id: None,
                replay_state: None,
                tool_calls: calls,
            },
            runtime_event_id: None,
        }
    }

    #[test]
    fn retained_paths_come_from_file_tool_calls_in_the_replacement() {
        let messages = vec![
            assistant(vec![ToolCallContent {
                index: 0,
                call_id: "c1".into(),
                name: "Read".into(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
            }]),
            assistant(vec![ToolCallContent {
                index: 0,
                call_id: "c2".into(),
                name: "Shell".into(),
                arguments: serde_json::json!({"command": "ls"}),
            }]),
        ];
        let paths = retained_file_paths(&messages);
        assert!(paths.contains("src/main.rs"));
        assert_eq!(paths.len(), 1, "non-file tools contribute no path");
    }

    #[test]
    fn retained_paths_match_state_keys_across_separator_spelling() {
        // The client's `file_states` key and the model's tool argument can name
        // the same file with `/` versus `\` or a leading `./`; the comparison
        // folds only those spelling differences.
        assert!(retained_file_paths(&[assistant(vec![ToolCallContent {
            index: 0,
            call_id: "c1".into(),
            name: "Read".into(),
            arguments: serde_json::json!({"path": "src\\lib.rs"}),
        }])])
        .contains(&normalize_path("./src/lib.rs")));
        assert_eq!(normalize_path(" src/main.rs "), "src/main.rs");
        assert_ne!(
            normalize_path("C:/repo/src/lib.rs"),
            normalize_path("src/lib.rs"),
            "absolute and relative paths must stay distinct"
        );
    }
}
