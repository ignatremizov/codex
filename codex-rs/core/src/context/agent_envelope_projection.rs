//! Lossless model-only projection of canonical agent envelopes.
//!
//! Parsing here is presentation, not attribution recovery: the receiving root's hydrated
//! aliases are authoritative for display refs. Stored JSON refs never populate that map.

use std::collections::HashMap;

use codex_protocol::ThreadId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;

use super::AgentContextIdentity;
use super::AttributedAgentMessage;
use super::SubagentCommentary;
use super::SubagentNotification;
use super::attributed_agent_message::ATTRIBUTED_AGENT_MESSAGE_KIND;

#[derive(Deserialize)]
pub(super) struct CanonicalEnvelopeIdentity {
    agent_id: ThreadId,
    agent_path: Option<String>,
}

impl CanonicalEnvelopeIdentity {
    pub(super) fn for_model(
        self,
        aliases: &HashMap<ThreadId, u64>,
    ) -> Option<AgentContextIdentity> {
        if self.agent_path.is_some() {
            return None;
        }
        Some(AgentContextIdentity::V1 {
            agent_id: self.agent_id,
            agent_ref: aliases.get(&self.agent_id).copied(),
            nickname: None,
            task_path: None,
        })
    }
}

/// Projects a disposable model-input copy, never stored history or presentation items.
///
/// Dedicated agent messages and the first text of harness-tagged attributed user messages
/// are eligible. Marker-looking ordinary user messages cannot acquire attribution through
/// this projection. Callers supply aliases from the same receiver-scoped snapshot included
/// in the model's identity context.
pub(crate) fn project_v1_agent_envelopes(
    items: &mut [ResponseItem],
    aliases: &HashMap<ThreadId, u64>,
) {
    for item in items {
        if let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = item
        {
            if role == "user"
                && metadata
                    .content_item_kinds
                    .as_ref()
                    .and_then(|kinds| kinds.first())
                    .is_some_and(|kind| kind.0 == ATTRIBUTED_AGENT_MESSAGE_KIND)
                && let Some(ContentItem::InputText { text }) = content.first_mut()
                && let Some(projected) = AttributedAgentMessage::project_model_text(text, aliases)
            {
                *text = projected;
            }
            continue;
        }
        let ResponseItem::AgentMessage { content, .. } = item else {
            continue;
        };
        let [AgentMessageInputContent::InputText { text }] = content.as_mut_slice() else {
            continue;
        };
        if let Some(projected) = SubagentCommentary::project_model_text(text, aliases)
            .or_else(|| SubagentNotification::project_model_text(text, aliases))
            .or_else(|| AttributedAgentMessage::project_model_text(text, aliases))
        {
            *text = projected;
        }
    }
}

#[cfg(test)]
#[path = "agent_envelope_projection_tests.rs"]
mod tests;
