//! Current V1 aliases, independent of environment context and retained history size.

use super::PreviousSectionState;
use super::WorldStateSection;
use crate::agent::control::V1AgentIdentitySnapshot;
use crate::context::ContextualUserFragment;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;

#[derive(Clone, Debug)]
pub(crate) struct AgentIdentitiesState {
    body: String,
}

impl AgentIdentitiesState {
    pub(crate) fn new(snapshot: &V1AgentIdentitySnapshot) -> Self {
        Self {
            body: snapshot.hydration_body(),
        }
    }

    /// Reuses an identical hydration fragment in place, removes superseded copies, and
    /// supplies a missing fragment (notably on compaction requests). Other content and
    /// retained harness metadata remain unchanged.
    pub(crate) fn reconcile_annotated(&self, items: &mut Vec<ResponseItemEnvelope>) {
        let expected = self.render();
        let mut retained = false;
        for envelope in items.iter_mut().rev() {
            let ResponseItem::Message {
                role,
                content,
                internal_chat_message_metadata_passthrough,
                ..
            } = &mut envelope.item
            else {
                continue;
            };
            if role != "developer" {
                continue;
            }
            let keep = content
                .iter()
                .map(|content| {
                    let ContentItem::InputText { text } = content else {
                        return true;
                    };
                    if !Self::matches_text(text) {
                        return true;
                    }
                    if !self.body.is_empty() && !retained && text == &expected {
                        retained = true;
                        true
                    } else {
                        false
                    }
                })
                .collect::<Vec<_>>();
            let mut index = 0;
            content.retain(|_| {
                let retain = keep[index];
                index += 1;
                retain
            });
            if let Some(kinds) = internal_chat_message_metadata_passthrough
                .as_mut()
                .and_then(|metadata| metadata.content_item_kinds.as_mut())
            {
                let mut index = 0;
                kinds.retain(|_| {
                    let retain = keep.get(index).copied().unwrap_or(true);
                    index += 1;
                    retain
                });
            }
        }
        items.retain(|envelope| {
            !matches!(&envelope.item, ResponseItem::Message { role, content, .. }
                if role == "developer" && content.is_empty())
        });
        if !self.body.is_empty() && !retained {
            items.push(ResponseItemEnvelope::new(ContextualUserFragment::into(
                self.clone(),
            )));
        }
    }
}

/// Hydrates and projects only a disposable prompt copy using a single receiver snapshot.
pub(crate) fn prepare_v1_agent_model_input(
    items: &mut Vec<ResponseItem>,
    snapshot: &V1AgentIdentitySnapshot,
) {
    let mut annotated = std::mem::take(items)
        .into_iter()
        .map(ResponseItemEnvelope::new)
        .collect();
    AgentIdentitiesState::new(snapshot).reconcile_annotated(&mut annotated);
    *items = annotated
        .into_iter()
        .map(ResponseItemEnvelope::into_item)
        .collect();
    crate::context::project_v1_agent_envelopes(items, &snapshot.refs);
}

impl ContextualUserFragment for AgentIdentitiesState {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.identities".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<agent_identity_context>", "</agent_identity_context>")
    }

    fn body(&self) -> String {
        format!("\n{}\n", self.body)
    }
}

impl WorldStateSection for AgentIdentitiesState {
    const ID: &'static str = "agent_identities";
    type Snapshot = String;

    fn snapshot(&self) -> Self::Snapshot {
        self.body.clone()
    }

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
    ) -> Option<Box<dyn ContextualUserFragment>> {
        if matches!(previous, PreviousSectionState::Known(previous) if previous == &self.body) {
            return None;
        }
        if self.body.is_empty() && matches!(previous, PreviousSectionState::Absent) {
            return None;
        }
        Some(Box::new(self.clone()))
    }
}

#[cfg(test)]
#[path = "agent_identities_tests.rs"]
mod tests;
