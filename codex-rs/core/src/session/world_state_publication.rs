//! Publish model-visible context and its replay baselines as one accepted batch.

use super::*;
use crate::context::world_state::WorldStateSnapshot;
use tokio::sync::OwnedSemaphorePermit;

pub(super) struct ContextUpdate {
    pub(super) world_state: WorldStateSnapshot,
    pub(super) rollout: Vec<RolloutItem>,
    pub(super) reference_context: Option<TurnContextItem>,
}

impl Session {
    pub(super) async fn publish_world_state_context(
        &self,
        permit: OwnedSemaphorePermit,
        step: &StepContext,
        items: Vec<ResponseItem>,
        update: ContextUpdate,
    ) -> CodexResult<()> {
        let (items, _) = self
            .prepare_conversation_items_for_history(&step.turn, &step.settings.model_info, &items)
            .await;
        let items = items.into_owned();
        let envelopes = items
            .iter()
            .cloned()
            .map(ResponseItemEnvelope::new)
            .collect::<Vec<_>>();
        let mut rollout = envelopes
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect::<Vec<_>>();
        rollout.extend(update.rollout);
        let policy = step.settings.model_info.truncation_policy.into();
        let recorded = items.clone();
        let receiver = self.dispatch_history_publication(
            permit,
            rollout,
            Vec::new(),
            /*acknowledgement*/ None,
            move |state| {
                state.current_time_reminder.note_recorded_items(&recorded);
                state.history.record_annotated_items(&envelopes, policy);
                state.history.set_world_state_baseline(update.world_state);
                if let Some(reference_context) = update.reference_context {
                    state.set_reference_context_item(Some(reference_context));
                }
            },
        )?;
        self.publication_result(receiver).await?;
        self.send_raw_response_items(&step.turn, &items).await;
        Ok(())
    }
}
