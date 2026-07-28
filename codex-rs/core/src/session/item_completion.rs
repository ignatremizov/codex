//! Shared lifecycle observation and timing for ordinary and receipt-bearing completions.

use super::*;

impl Session {
    pub(super) async fn prepare_turn_item_completed_event(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
    ) -> ItemCompletedEvent {
        record_turn_ttfm_metric(turn_context, &item).await;
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_item_completed(
                    &self.services.thread_extension_data,
                    turn_context.extension_data.as_ref(),
                    &item,
                )
                .await;
        }
        let completed_at_ms = now_unix_timestamp_ms();
        let item_id = item.id();
        let started_at_ms = turn_context
            .turn_timing_state
            .take_item_started(&item_id)
            .await
            .unwrap_or_else(|| {
                warn!(
                    thread_id = %self.thread_id,
                    turn_id = %turn_context.sub_id,
                    item_id = %item_id,
                    "item completed without a recorded start timestamp"
                );
                completed_at_ms
            });
        ItemCompletedEvent {
            thread_id: self.thread_id,
            turn_id: turn_context.sub_id.clone(),
            item,
            started_at_ms: Some(started_at_ms),
            completed_at_ms,
        }
    }
}
