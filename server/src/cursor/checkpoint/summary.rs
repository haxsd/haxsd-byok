//! Builds compacted checkpoint summary state.
use prost::Message;

use crate::{
    cursor::{checkpoint::PendingSteps, protocol::proto::agent::v1 as pb},
    model::{estimate_context_tokens, project_messages, CanonicalMessage, PromptSpec},
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
        if let Some(details) = self.base.token_details.as_mut() {
            details.breakdown = Some(crate::cursor::services::usage::breakdown(
                details.used_tokens,
                details.max_tokens,
                details.breakdown.as_ref(),
                &self.instructions,
                &self.tool_definitions,
                &self.dynamic_tools,
                messages,
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
