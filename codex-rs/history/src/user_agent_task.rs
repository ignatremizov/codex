//! Canonical provenance for user-authored task context, separate from DTO shape validation.

use std::collections::HashMap;
use std::collections::HashSet;

use codex_protocol::ResponseItemId;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::AgentResponsePromotedTaskContext;

use crate::ResponseItemEnvelope;
use crate::RolloutItem;

#[cfg(test)]
#[path = "user_agent_task_tests.rs"]
mod tests;

/// A task envelope and its adjacent canonical promotion evidence.
///
/// This permits cold context replay and checkpoint retention, not live control, subscriptions,
/// or acknowledgment of an uncertain write.
#[derive(Clone, Debug)]
pub struct UserAgentTaskContextEvidence {
    pub item: ResponseItemEnvelope,
    pub observation: AgentResponseObservation,
}

/// Streaming counterpart of [`committed_user_agent_task_contexts`] for canonical migration.
///
/// Feed every record in one raw lineage segment, including rollback and checkpoint records.
/// Start a fresh collector at each segment boundary so adjacency cannot cross source files.
#[derive(Default)]
pub struct UserAgentTaskContextCollector {
    evidence: HashMap<ResponseItemId, UserAgentTaskContextEvidence>,
    seen: HashMap<ResponseItemId, ResponseItemEnvelope>,
    conflicting: HashSet<ResponseItemId>,
    pending: Option<ResponseItemEnvelope>,
    previous_metadata: bool,
}

impl UserAgentTaskContextCollector {
    /// End a contiguous canonical run when a streaming reader skips an undecodable record.
    ///
    /// A skipped source line prevents a later snapshot from authorizing the preceding envelope.
    pub fn break_adjacency(&mut self) {
        self.pending = None;
        self.previous_metadata = false;
    }

    pub fn observe(&mut self, record: &RolloutItem) {
        let envelopes: &[ResponseItemEnvelope] = match record {
            RolloutItem::ResponseItem(item) => std::slice::from_ref(item),
            RolloutItem::Compacted(checkpoint) => checkpoint
                .replacement_history
                .as_deref()
                .unwrap_or_default(),
            _ => &[],
        };
        for envelope in envelopes {
            let Some(id) = envelope.id().filter(|id| {
                codex_protocol::protocol::is_user_agent_task_context_response_item_id(id.as_str())
            }) else {
                continue;
            };
            if self.seen.get(id).is_some_and(|item| item != envelope) {
                self.conflicting.insert(id.clone());
            } else {
                self.seen.insert(id.clone(), envelope.clone());
            }
        }
        match record {
            RolloutItem::AgentResponseObservation(observation) => {
                if let Some(item) = &self.pending
                    && let Some(task) =
                        AgentResponsePromotedTaskContext::from_response_item(&item.item)
                    && observation.promoted_task_context.as_ref() == Some(&task)
                {
                    self.evidence.insert(
                        task.response_item_id,
                        UserAgentTaskContextEvidence {
                            item: item.clone(),
                            observation: observation.clone(),
                        },
                    );
                }
            }
            RolloutItem::ResponseItem(item) => {
                self.pending = (self.previous_metadata
                    && AgentResponsePromotedTaskContext::from_response_item(&item.item).is_some())
                .then(|| item.clone());
            }
            _ => self.pending = None,
        }
        self.previous_metadata = matches!(
            record,
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false
            }
        );
    }

    pub fn finish(mut self) -> HashMap<ResponseItemId, UserAgentTaskContextEvidence> {
        self.evidence.retain(|id, _| !self.conflicting.contains(id));
        self.evidence
    }
}

/// Finds exact task links in one original, unfiltered canonical lineage segment.
///
/// A metadata/task/snapshot sequence must be physically adjacent. A later standalone snapshot
/// cannot promote another record. Conflicting payloads sharing an identity, including checkpoint
/// envelopes, invalidate that identity for the entire segment.
pub fn committed_user_agent_task_contexts(
    items: &[RolloutItem],
) -> HashMap<ResponseItemId, UserAgentTaskContextEvidence> {
    let mut collector = UserAgentTaskContextCollector::default();
    for item in items {
        collector.observe(item);
    }
    collector.finish()
}
