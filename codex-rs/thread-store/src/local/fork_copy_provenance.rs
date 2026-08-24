//! Copying history cannot create new canonical response/snapshot evidence by closing source gaps.

use std::collections::HashMap;

use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::committed_user_agent_task_contexts;
use codex_history::is_committed_observed_response;
use codex_protocol::ResponseItemId;
use codex_protocol::protocol::AgentResponseObservation;

use crate::ThreadStoreError;
use crate::ThreadStoreResult;

struct SourceContextProof {
    item: ResponseItemEnvelope,
    observations: Vec<AgentResponseObservation>,
}

/// Original, segment-local evidence for a copied source, not a live writer acknowledgment.
#[derive(Default)]
pub(super) struct SourceProvenance {
    contexts: HashMap<ResponseItemId, Vec<SourceContextProof>>,
}

impl SourceProvenance {
    pub(super) fn record_segment(&mut self, items: &[RolloutItem]) {
        for (id, proof) in committed_user_agent_task_contexts(items) {
            self.contexts
                .entry(id)
                .or_default()
                .push(SourceContextProof {
                    item: proof.item,
                    observations: vec![proof.observation],
                });
        }
        for (index, item) in items.iter().enumerate() {
            let RolloutItem::ResponseItem(item) = item else {
                continue;
            };
            let Some(id) = item.id() else { continue };
            if !is_committed_observed_response(items, index) {
                continue;
            }
            let observations = items[index + 1..]
                .iter()
                .take_while(|item| matches!(item, RolloutItem::AgentResponseObservation(_)))
                .filter_map(|item| match item {
                    RolloutItem::AgentResponseObservation(observation)
                        if observation
                            .committed_delivery_response_item_ids
                            .contains(id) =>
                    {
                        Some(observation.clone())
                    }
                    _ => None,
                })
                .collect();
            self.contexts
                .entry(id.clone())
                .or_default()
                .push(SourceContextProof {
                    item: item.clone(),
                    observations,
                });
        }
    }

    pub(super) fn validate_copy(&self, items: &[RolloutItem]) -> ThreadStoreResult<()> {
        for proof in committed_user_agent_task_contexts(items).into_values() {
            self.require_source_proof(&proof.item, &proof.observation)?;
        }
        for (index, item) in items.iter().enumerate() {
            let RolloutItem::ResponseItem(item) = item else {
                continue;
            };
            let Some(id) = item.id() else { continue };
            if !is_committed_observed_response(items, index) {
                continue;
            }
            for following in items[index + 1..]
                .iter()
                .take_while(|item| matches!(item, RolloutItem::AgentResponseObservation(_)))
            {
                if let RolloutItem::AgentResponseObservation(observation) = following
                    && observation
                        .committed_delivery_response_item_ids
                        .contains(id)
                {
                    self.require_source_proof(item, observation)?;
                }
            }
        }
        Ok(())
    }

    fn require_source_proof(
        &self,
        item: &ResponseItemEnvelope,
        observation: &AgentResponseObservation,
    ) -> ThreadStoreResult<()> {
        if item
            .id()
            .and_then(|id| self.contexts.get(id))
            .is_some_and(|proofs| {
                proofs
                    .iter()
                    .any(|proof| proof.item == *item && proof.observations.contains(observation))
            })
        {
            return Ok(());
        }
        // Reject an unsafe copy instead of rewriting source identities, inserting an artificial
        // boundary, or treating an audit snapshot as acknowledgment of a formerly uncertain write.
        Err(ThreadStoreError::InvalidRequest {
            message: "cannot copy fork source: flattening would create response observation evidence absent from the original lineage".to_string(),
        })
    }
}
