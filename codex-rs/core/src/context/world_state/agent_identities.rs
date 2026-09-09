//! Current V1 aliases, independent of environment context and retained history size.

use super::PreviousSectionState;
use super::WorldStateSection;
use crate::agent::control::V1AgentIdentitySnapshot;
use crate::context::ContextualUserFragment;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;

#[path = "agent_identity_delta.rs"]
mod agent_identity_delta;

use agent_identity_delta::IdentityMapping;
use agent_identity_delta::body_from_rendered_fragment;
use agent_identity_delta::resolve_retained_chain;

#[derive(Clone, Debug)]
pub(crate) struct AgentIdentitiesState {
    body: String,
}

#[derive(Clone, Debug)]
struct AgentIdentityContextFragment {
    body: String,
}

impl AgentIdentitiesState {
    pub(crate) fn new(snapshot: &V1AgentIdentitySnapshot) -> Self {
        Self {
            body: snapshot.hydration_body(),
        }
    }

    /// Preserves a usable retained base and delta chain, appending only the update needed to
    /// reach the current authority. Missing or malformed chains recover with a full snapshot.
    pub(crate) fn reconcile_annotated(&self, items: &mut Vec<ResponseItemEnvelope>) {
        let retained_bodies = items
            .iter()
            .flat_map(|envelope| {
                let ResponseItem::Message { role, content, .. } = &envelope.item else {
                    return Vec::new();
                };
                if role != "developer" {
                    return Vec::new();
                }
                content
                    .iter()
                    .filter_map(|content| {
                        let ContentItem::InputText { text } = content else {
                            return None;
                        };
                        if !Self::matches_text(text) {
                            return None;
                        }
                        Some(body_from_rendered_fragment(text, Self::type_markers()).unwrap_or(""))
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let desired = IdentityMapping::from_authority_body(&self.body);
        let retained = resolve_retained_chain(retained_bodies.iter().copied());
        let update = match (desired, retained) {
            (Some(desired), Some(retained)) => match desired.delta_from(&retained) {
                Some(delta) => (!delta.is_empty()).then(|| delta.body()),
                None => Some(IdentityMapping::full_body(&self.body)),
            },
            (Some(_), None) if self.body.is_empty() && retained_bodies.is_empty() => None,
            (Some(_), None) | (None, _) => Some(IdentityMapping::full_body(&self.body)),
        };
        if let Some(body) = update {
            items.push(ResponseItemEnvelope::new(ContextualUserFragment::into(
                AgentIdentityContextFragment { body },
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

impl ContextualUserFragment for AgentIdentityContextFragment {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.identities".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        AgentIdentitiesState::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        AgentIdentitiesState::type_markers()
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
        let body = match previous {
            // Root-only state intentionally had no visible base to apply a first delta to.
            PreviousSectionState::Known(previous) if previous.is_empty() => {
                IdentityMapping::full_body(&self.body)
            }
            PreviousSectionState::Known(previous) => {
                match (
                    IdentityMapping::from_authority_body(previous),
                    IdentityMapping::from_authority_body(&self.body),
                ) {
                    (Some(previous), Some(current)) => {
                        let Some(delta) = current.delta_from(&previous) else {
                            return Some(Box::new(AgentIdentityContextFragment {
                                body: IdentityMapping::full_body(&self.body),
                            }));
                        };
                        if delta.is_empty() {
                            return None;
                        }
                        delta.body()
                    }
                    _ => IdentityMapping::full_body(&self.body),
                }
            }
            PreviousSectionState::Absent | PreviousSectionState::Unknown => {
                IdentityMapping::full_body(&self.body)
            }
        };
        Some(Box::new(AgentIdentityContextFragment { body }))
    }
}

#[cfg(test)]
#[path = "agent_identities_tests.rs"]
mod tests;
