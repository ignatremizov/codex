//! Publish model-visible context and its replay baselines as one accepted batch.

use super::prepared_history_items::PreparedHistoryItems;
use super::*;
use crate::context::world_state::WorldStateSnapshot;
use tokio::sync::OwnedSemaphorePermit;

/// Reference/history facts that must still hold after asynchronous host preparation.
#[derive(PartialEq)]
pub(super) struct ContextBaseline {
    pub(super) reference_context: Option<TurnContextItem>,
    world_state: Option<WorldStateItem>,
    history_version: u64,
    item_count: usize,
}

impl ContextBaseline {
    pub(super) fn capture(state: &crate::state::SessionState) -> Self {
        Self {
            reference_context: state.reference_context_item(),
            world_state: state.history.world_state_checkpoint(),
            history_version: state.history.history_version(),
            item_count: state.history.annotated_items().len(),
        }
    }

    pub(super) fn matches(&self, state: &crate::state::SessionState) -> bool {
        self == &Self::capture(state)
    }
}

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
        let envelopes = items
            .iter()
            .cloned()
            .map(ResponseItemEnvelope::new)
            .collect::<Vec<_>>();
        let audio_ids = file_audio::file_ids(&envelopes)
            .cloned()
            .collect::<Vec<_>>();
        let prepared = PreparedHistoryItems::new(
            &self.state.lock().await.history,
            envelopes,
            step.settings.model_info.truncation_policy.into(),
        );
        let mut rollout = prepared.rollout_items();
        rollout.extend(update.rollout);
        let recorded = items.clone();
        let receiver = self.dispatch_history_publication(
            permit,
            rollout,
            Vec::new(),
            /*acknowledgement*/ None,
            move |state| {
                state.current_time_reminder.note_recorded_items(&recorded);
                prepared.install(&mut state.history);
                state.pending_audio_duration_ids.extend(audio_ids);
                state.history.set_world_state_baseline(update.world_state);
                if let Some(reference_context) = update.reference_context {
                    state.set_reference_context_item(Some(reference_context));
                }
            },
        )?;
        self.publication_result(receiver).await?;
        // Raw observers remain caller-owned until the later transcript-publication owner.
        self.send_raw_response_items(&step.turn, &items).await;
        Ok(())
    }
}
