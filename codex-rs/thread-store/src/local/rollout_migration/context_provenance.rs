//! Trust is established from raw source adjacency, before rollback can remove intervening records.

use std::collections::HashMap;
use std::collections::HashSet;

use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutItem;

pub(super) type ContextProofs = HashMap<ResponseItemId, ResponseItemEnvelope>;

#[derive(Default)]
pub(super) struct ContextProvenance {
    tasks: codex_history::UserAgentTaskContextCollector,
    seen: ContextProofs,
    conflicting: HashSet<ResponseItemId>,
    observed: ContextProofs,
    pending: Option<ResponseItemEnvelope>,
    previous_metadata: bool,
}

impl ContextProvenance {
    pub(super) fn break_adjacency(&mut self) {
        self.tasks.break_adjacency();
        self.pending = None;
        self.previous_metadata = false;
    }

    pub(super) fn observe(&mut self, item: &RolloutItem) {
        self.tasks.observe(item);
        let envelopes = match item {
            RolloutItem::ResponseItem(item) => std::slice::from_ref(item),
            RolloutItem::Compacted(item) => item.replacement_history.as_deref().unwrap_or_default(),
            _ => &[],
        };
        for envelope in envelopes {
            if let Some(id) = envelope.id().filter(|id| {
                id.as_str().starts_with("amsg_")
                    || codex_protocol::protocol::is_sub_agent_completion_context_response_item_id(
                        id.as_str(),
                    )
            }) {
                if self
                    .seen
                    .get(id)
                    .is_some_and(|previous| previous != envelope)
                {
                    self.conflicting.insert(id.clone());
                } else {
                    self.seen.insert(id.clone(), envelope.clone());
                }
            }
        }
        match item {
            RolloutItem::ResponseItem(item) => {
                if self.previous_metadata
                    && let Some(id) = item.id().filter(|id| {
                        codex_protocol::protocol::is_sub_agent_completion_context_response_item_id(
                            id.as_str(),
                        )
                    })
                {
                    self.observed.insert(id.clone(), item.clone());
                }
                self.pending = (self.previous_metadata
                    && matches!(&item.item, ResponseItem::AgentMessage { .. }))
                .then(|| item.clone());
            }
            RolloutItem::AgentResponseObservation(observation) => {
                if let Some(item) = &self.pending
                    && let Some(id) = item.id().filter(|id| id.as_str().starts_with("amsg_"))
                    && observation
                        .committed_delivery_response_item_ids
                        .contains(id)
                {
                    self.observed.insert(id.clone(), item.clone());
                }
            }
            _ => self.pending = None,
        }
        self.previous_metadata =
            matches!(item, RolloutItem::InterAgentCommunicationMetadata { .. });
    }

    pub(super) fn finish(mut self) -> ContextProofs {
        self.observed.retain(|id, _| !self.conflicting.contains(id));
        self.observed.extend(
            self.tasks
                .finish()
                .into_iter()
                .map(|(id, proof)| (id, proof.item)),
        );
        self.observed
    }
}

pub(super) fn normalize_unproven_tasks(item: &mut RolloutItem, proofs: &ContextProofs) {
    let envelopes = match item {
        RolloutItem::ResponseItem(item) => std::slice::from_mut(item),
        RolloutItem::Compacted(item) => item.replacement_history.as_deref_mut().unwrap_or_default(),
        _ => &mut [],
    };
    for envelope in envelopes {
        if let Some(id) = envelope.id()
            && codex_protocol::protocol::is_user_agent_task_context_response_item_id(id.as_str())
            && proofs.get(id) != Some(envelope)
        {
            let id = ResponseItemId::with_suffix("msg", format!("untrusted-{}", id.as_str()));
            envelope.item.set_id(Some(id));
        }
    }
}
