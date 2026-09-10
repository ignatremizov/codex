//! Canonical evidence for already delivered finals when a runtime is resumed.

use super::response_observer::ResponseObserverStart;
use super::*;
use crate::CodexThread;
use crate::session::AgentResponseSnapshot;
use codex_history::rollout::exact_rollback_removed_items;
use std::collections::HashSet;

impl ResponseObserverStart {
    /// Filter only the existing live reconciliation candidate. History cannot supply a turn
    /// or activate a subscription, and delivered text is never an identity.
    pub(super) fn reconciled_terminal(
        &self,
        snapshot: &AgentResponseSnapshot,
    ) -> Option<(String, AgentStatus)> {
        match self {
            Self::CurrentOrNext {
                observed_status,
                delivered_final_turns,
            } if !crate::agent::status::is_final(observed_status)
                && snapshot.active_turn_id.is_none()
                && crate::agent::status::is_final(&snapshot.status) =>
            {
                snapshot.last_terminal.clone().filter(|(turn, status)| {
                    !matches!(status, AgentStatus::Completed(_))
                        || !delivered_final_turns.contains(turn)
                })
            }
            Self::FutureOnly | Self::CurrentOrNext { .. } => None,
        }
    }
}

/// Read before acquiring mailbox/observation transactions: canonical history loading must not
/// introduce an observation-to-durable-context lock dependency. Missing evidence never suppresses
/// delivery. Receipts are scoped to both endpoints and the child turn, not the response text.
pub(super) async fn delivered_final_turns(
    observer: &CodexThread,
    child: ThreadId,
) -> HashSet<String> {
    let observer_id = observer.session.thread_id();
    // Use the strict raw receiver-owned reader, not bounded model history or expanded lineage.
    // Its parse/ownership checks fail closed; no new recorder or graph ownership is created.
    let history = match observer
        .session
        .services
        .thread_store
        .load_mailbox_canonical_history(observer_id)
        .await
    {
        Ok(history) => history,
        Err(error) => {
            warn!(%observer_id, %child, %error, "could not verify prior final delivery on resume");
            return HashSet::new();
        }
    };
    delivered_final_turns_in_history(&history, observer_id, child)
}

fn delivered_final_turns_in_history(
    history: &[RolloutItem],
    observer: ThreadId,
    child: ThreadId,
) -> HashSet<String> {
    let removed = exact_rollback_removed_items(history);
    let mut persisted = HashMap::new();
    let mut conflicting = HashSet::new();
    for (item, removed) in history.iter().zip(&removed) {
        if !*removed
            && let RolloutItem::ResponseItem(item) = item
            && let Some(id) = item.id()
            && persisted
                .insert(id, item)
                .is_some_and(|previous| previous != item)
        {
            conflicting.insert(id);
        }
    }
    let mut delivered = HashSet::new();
    // Validate adjacency in the ORIGINAL coordinate space. Removing rollback ranges first
    // could manufacture a metadata/response/commit sequence that was never published.
    for (index, item) in history.iter().enumerate() {
        if removed[index] || !codex_history::is_committed_observed_response(history, index) {
            continue;
        }
        let RolloutItem::ResponseItem(item) = item else {
            continue;
        };
        let Some(id) = item.id().filter(|id| !conflicting.contains(id)) else {
            continue;
        };
        for (offset, item) in history[index + 1..].iter().enumerate() {
            let RolloutItem::AgentResponseObservation(observation) = item else {
                break;
            };
            if !removed[index + 1 + offset]
                && observation.observer_thread_id == observer
                && observation.target_thread_id == child
                && observation.final_delivery_response_item_id.as_ref() == Some(id)
                && observation
                    .committed_delivery_response_item_ids
                    .contains(id)
                && let Some(turn) = &observation.target_turn_id
            {
                delivered.insert(turn.clone());
            }
        }
    }
    delivered
}

#[cfg(test)]
#[path = "resume_delivery_tests.rs"]
mod tests;
