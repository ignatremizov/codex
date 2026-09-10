//! Canonical evidence for already delivered finals when a runtime is resumed.

use super::*;
use codex_history::rollout::rollout_without_exact_rollback_ranges;
use std::collections::HashSet;

impl InitialTerminalReconciliation {
    pub(super) fn without_delivered_finals(
        mut self,
        delivered_final_turns: &HashSet<String>,
    ) -> Self {
        if self.terminal.as_ref().is_some_and(|(turn_id, status)| {
            matches!(status, AgentStatus::Completed(_)) && delivered_final_turns.contains(turn_id)
        }) {
            // Preserve the real terminal status and future observation policy, but do not
            // publish an already committed final again after cleanup or cold resume.
            self.terminal = None;
        }
        self
    }
}

/// Read before acquiring mailbox/observation transactions: canonical history loading must not
/// introduce an observation-to-durable-context lock dependency. Missing evidence never suppresses
/// delivery. Receipts are scoped to both endpoints and the child turn, not the response text.
pub(super) async fn delivered_final_turns(
    state: &ThreadManagerState,
    observer: ThreadId,
    child: ThreadId,
) -> HashSet<String> {
    let history = match state
        .load_canonical_thread_history(LoadThreadHistoryParams {
            thread_id: observer,
            include_archived: false,
        })
        .await
    {
        Ok(history) => history,
        Err(error) => {
            warn!(%observer, %child, %error, "could not verify prior final delivery on resume");
            return HashSet::new();
        }
    };
    delivered_final_turns_in_history(&history.items, observer, child)
}

fn delivered_final_turns_in_history(
    history: &[RolloutItem],
    observer: ThreadId,
    child: ThreadId,
) -> HashSet<String> {
    let items = rollout_without_exact_rollback_ranges(history);
    let persisted_ids = items
        .iter()
        .filter_map(|item| {
            let RolloutItem::ResponseItem(item) = item else {
                return None;
            };
            item.id()
        })
        .collect::<HashSet<_>>();
    items
        .iter()
        .filter_map(|item| {
            let RolloutItem::AgentResponseObservation(observation) = item else {
                return None;
            };
            if observation.observer_thread_id != observer || observation.target_thread_id != child {
                return None;
            }
            let final_id = observation.final_delivery_response_item_id.as_ref()?;
            (persisted_ids.contains(final_id)
                && observation
                    .committed_delivery_response_item_ids
                    .contains(final_id))
            .then(|| observation.target_turn_id.clone())
            .flatten()
        })
        .collect()
}

#[cfg(test)]
#[path = "resume_delivery_tests.rs"]
mod tests;
