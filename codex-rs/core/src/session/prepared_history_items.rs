//! Capture canonical source revisions without publishing live history before storage.
//! Installation replays the prepared originals, preserving reservations made during I/O.

use crate::context_manager::ContextManager;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_utils_output_truncation::TruncationPolicy;

pub(super) struct PreparedHistoryItems {
    envelopes: Vec<ResponseItemEnvelope>,
    policy: TruncationPolicy,
}

impl PreparedHistoryItems {
    pub(super) fn new(
        history: &ContextManager,
        mut envelopes: Vec<ResponseItemEnvelope>,
        policy: TruncationPolicy,
    ) -> Self {
        let mut projected = history.clone();
        projected.record_annotated_items(&mut envelopes, policy);
        Self { envelopes, policy }
    }

    pub(super) fn rollout_items(&self) -> Vec<RolloutItem> {
        self.envelopes
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect()
    }

    pub(super) fn install(self, history: &mut ContextManager) {
        // Do not replace the captured history wholesale: input and stream-start order
        // reservations can advance while the independent writer is appending/flushing.
        for envelope in &self.envelopes {
            history.replay_annotated_item(envelope, self.policy);
        }
    }
}
