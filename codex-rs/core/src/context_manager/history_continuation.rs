//! Only an explicit continuation can omit a typed harness interrupt notice.
//! User text that happens to look like an interruption is never a removal candidate.

use super::ContextManager;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TurnContextItem;
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) fn is_turn_interrupt_notice(item: &ResponseItemEnvelope, turn_id: &str) -> bool {
    if item.turn_id() != Some(turn_id)
        || item
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.client_authored || metadata.inherited_user_message)
    {
        return false;
    }
    matches!(&item.item,
        ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } if matches!(role.as_str(), "user" | "developer")
            && content.len() == 1
            && metadata.content_item_kinds.as_ref().is_some_and(|kinds|
                kinds.len() == 1 && kinds[0].0 == "generic.turn_aborted"))
}

impl ContextManager {
    /// Applies a durably recorded same-turn context edit without changing instruction authority.
    /// Returns the number of removed items in the compacted prefix, for replay's media boundary.
    pub(crate) fn apply_turn_continuation(&mut self, context: &TurnContextItem) -> usize {
        let Some(turn_id) = &context.turn_id else {
            return 0;
        };
        let Some(saved) = &context.recovery_start_options else {
            return 0;
        };
        if saved.cleared_interrupt_message_ids.is_empty() {
            return 0;
        }
        let ids: HashSet<&str> = saved
            .cleared_interrupt_message_ids
            .iter()
            .map(String::as_str)
            .collect();
        let keep = |item: &ResponseItemEnvelope| {
            !(is_turn_interrupt_notice(item, turn_id)
                && item.id().is_some_and(|id| ids.contains(id.as_str())))
        };
        let prefix = self.compacted_prefix_len.unwrap_or_default();
        let removed_prefix = self
            .items
            .iter()
            .take(prefix)
            .filter(|item| !keep(item))
            .count();
        if self.items.iter().any(|item| !keep(item)) {
            Arc::make_mut(&mut self.items).retain(keep);
            if let Some(prefix) = &mut self.compacted_prefix_len {
                *prefix = prefix.saturating_sub(removed_prefix);
            }
            self.history_version = self.history_version.saturating_add(1);
            self.reset_version = self.history_version;
        }
        if let Some(review_history) = &mut self.review_history {
            let checkpoint = review_history.checkpoint();
            if checkpoint.0.iter().any(|item| !keep(item)) {
                review_history.reset(checkpoint.0.iter().filter(|item| keep(item)));
                self.guardian_review_context_revision =
                    super::next_guardian_review_context_revision();
            }
        }
        removed_prefix
    }
}

#[cfg(test)]
#[path = "history_continuation_tests.rs"]
mod tests;
