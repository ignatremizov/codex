//! Current receiver identity is independent of the root's bounded discovery directory.

use super::PreviousSectionState;
use super::SectionTransition;
use super::WorldStateSection;
use super::WorldStateUpdate;
use crate::agent::control::V1AgentIdentitySnapshot;
use crate::context::ContextualUserFragment;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use serde_json::json;

/// Binds "you" to the receiving thread even when other context names its root or sender.
#[derive(Clone, Debug)]
pub(crate) struct AgentReceiverIdentityState {
    body: String,
}

impl AgentReceiverIdentityState {
    pub(crate) fn new(snapshot: &V1AgentIdentitySnapshot) -> Option<Self> {
        let agent_id = snapshot.receiver_thread_id?;
        let identity = json!({
            "agent_id": agent_id,
            "self_ref": snapshot.receiver_ref.map(|agent_ref| agent_ref.to_string()),
        });
        Some(Self {
            body: format!(
                "This is your identity for the current thread and replaces earlier self identities. \
                 Identity fields in agent messages identify their senders.\n{identity}"
            ),
        })
    }

    /// Restores identity after compaction or a preserved parent prefix without rewriting history.
    pub(crate) fn reconcile_annotated(&self, items: &mut Vec<ResponseItemEnvelope>) {
        let retained = items.iter().rev().find_map(|envelope| {
            let ResponseItem::Message { role, content, .. } = &envelope.item else {
                return None;
            };
            if role != "developer" {
                return None;
            }
            content.iter().rev().find_map(|content| match content {
                ContentItem::InputText { text } if Self::matches_text(text) => Some(text.as_str()),
                _ => None,
            })
        });
        let (open, close) = Self::type_markers();
        let current = retained
            .and_then(|text| text.strip_prefix(open))
            .and_then(|text| text.strip_suffix(close))
            .and_then(|text| text.strip_prefix('\n'))
            .and_then(|text| text.strip_suffix('\n'));
        if current != Some(self.body.as_str()) {
            items.push(ResponseItemEnvelope::new(ContextualUserFragment::into(
                self.clone(),
            )));
        }
    }
}

impl ContextualUserFragment for AgentReceiverIdentityState {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.self".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<agent_self>", "</agent_self>")
    }

    fn body(&self) -> ContentItem {
        codex_protocol::models::input_text(format!("\n{}\n", self.body))
    }
}

impl WorldStateSection for AgentReceiverIdentityState {
    const ID: &'static str = "agent_self";
    type Snapshot = String;

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "developer" && Self::matches_text(text)
    }

    fn has_retained_fragment_matcher() -> bool {
        true
    }

    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        Self::matches_legacy_fragment(role, text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> SectionTransition<Self::Snapshot> {
        let updates = if matches!(previous, PreviousSectionState::Known(previous) if previous == &self.body)
        {
            Vec::new()
        } else {
            vec![WorldStateUpdate::fragment(self.clone())]
        };
        (Some(self.body.clone()), updates)
    }
}

#[cfg(test)]
#[path = "agent_receiver_identity_tests.rs"]
mod tests;
