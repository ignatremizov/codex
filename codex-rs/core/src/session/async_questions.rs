//! Thread-local short question references, independent of the retained model window.

use super::Session;
use crate::function_tool::FunctionCallError;
use codex_context_fragments::async_question_number;
use codex_history::RolloutItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_thread_store::LoadThreadHistoryParams;
use std::ops::Range;

impl Session {
    /// Reserve once per question batch. Gaps are harmless; reusing a visible number is not.
    /// Raw artifacts include pre-compaction and legacy rollback records. A paginated
    /// history revert/fork reserves from its retained lineage; presentation IDs also
    /// retain the source call ID, independently of their short model-visible number.
    pub(crate) async fn reserve_async_question_refs(
        &self,
        count: usize,
    ) -> Result<Range<u64>, FunctionCallError> {
        let initialized = self.state.lock().await.async_question_high_water.is_some();
        let mut restored = 0;
        if !initialized && self.live_thread().is_some() {
            let history = self
                .services
                .thread_store
                .load_canonical_artifact_segments(LoadThreadHistoryParams {
                    thread_id: self.thread_id,
                    include_archived: false,
                })
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "Cannot restore question references: {err}"
                    ))
                })?;
            // No rollback masking here: this is an allocation high-water mark, not evidence
            // that an old question is still pending or that a write was acknowledged.
            for item in history.segments.iter().flatten() {
                let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = item else {
                    continue;
                };
                let TurnItem::AgentMessage(message) = &event.item else {
                    continue;
                };
                let Some(first) = async_question_number(&message.id) else {
                    continue;
                };
                let Some(questions) = &message.questions else {
                    continue;
                };
                if let Some(last) = first.checked_add(questions.len().saturating_sub(1) as u64) {
                    restored = restored.max(last);
                }
            }
        }
        let mut state = self.state.lock().await;
        let high_water = state.async_question_high_water.get_or_insert(restored);
        let start = high_water.checked_add(1).ok_or_else(|| {
            FunctionCallError::RespondToModel("Question references exhausted".into())
        })?;
        let end = start.checked_add(count as u64).ok_or_else(|| {
            FunctionCallError::RespondToModel("Question references exhausted".into())
        })?;
        *high_water = end - 1;
        Ok(start..end)
    }
}

#[cfg(test)]
#[path = "async_question_tests.rs"]
mod tests;
