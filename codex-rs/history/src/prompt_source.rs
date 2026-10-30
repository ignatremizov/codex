//! Pair canonical prompt presentations with their model inputs without matching prompt text.

use codex_protocol::models::ResponseItem;

use crate::RetainedSourceRole;
use crate::RolloutItem;

/// Collects model-input coordinates for one turn, in the caller's original source space.
///
/// Presentations and model inputs can be persisted in either order. A cut must exclude both.
/// Missing or ambiguous pairs fail closed; presentation-only histories remain editable.
#[derive(Default)]
pub struct UserPromptSource<P> {
    inputs: Vec<P>,
    saw_user_response: bool,
}

impl<P: Copy + Ord> UserPromptSource<P> {
    /// Observe a surviving source record before resolving any prompt in this turn.
    pub fn observe(&mut self, item: &RolloutItem, position: P) {
        let RolloutItem::ResponseItem(envelope) = item else {
            return;
        };
        // Goal edits are real user instructions, but have no UserMessage presentation to pair.
        // Their retained-source role remains User, so classify them before consulting that role.
        if crate::user_goal_update_text(&envelope.item).is_some() {
            return;
        }
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &envelope.item
        else {
            return;
        };
        if role != "user" {
            return;
        }
        self.saw_user_response = true;
        let metadata = envelope.metadata.as_ref();
        if metadata.is_some_and(|metadata| metadata.compaction_output) {
            return;
        }
        let is_input = if let Some(source) =
            metadata.and_then(|metadata| metadata.retained_source.as_ref())
        {
            source.id.role == RetainedSourceRole::User
        } else {
            internal_chat_message_metadata_passthrough
                .as_ref()
                .and_then(|metadata| metadata.content_item_kinds.as_ref())
                .is_none_or(|kinds| {
                    kinds.is_empty()
                        || kinds.len() != content.len()
                        || kinds.iter().any(|kind| {
                            kind.0.starts_with("user.") || matches!(kind.0.as_str(), "" | "unknown")
                        })
                })
        };
        if is_input {
            self.inputs.push(position);
        }
    }

    /// Resolve an exact presentation among all surviving user presentations in source order.
    pub fn boundary(&self, presentations: &[P], selected: usize) -> Option<P> {
        let presentation = *presentations.get(selected)?;
        if !self.saw_user_response {
            return Some(presentation);
        }
        if self.inputs.len() != presentations.len()
            || self
                .inputs
                .windows(2)
                .zip(presentations.windows(2))
                .any(|(inputs, events)| inputs[0].max(events[0]) >= inputs[1].min(events[1]))
        {
            return None;
        }
        Some(presentation.min(self.inputs[selected]))
    }
}

#[cfg(test)]
#[path = "prompt_source_tests.rs"]
mod tests;
