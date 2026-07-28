//! Receipt scope and accepted completion evidence for persistent and ephemeral sessions.

use super::Session;
use super::sub_agent_completion::PrimaryEventEnqueue;
use codex_protocol::ResponseItemId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_thread_store::LoadSubAgentCompletionContextItemParams;

/// A volatile installation is never evidence that a canonical writer acknowledged a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CompletionPublicationReceipt {
    Canonical { primary_event: PrimaryEventEnqueue },
    RuntimeOnly { primary_event: PrimaryEventEnqueue },
}

impl Session {
    /// Persistent sessions require store evidence; ephemeral sessions use only this
    /// exact runtime's positive publication receipts, never arbitrary model history.
    pub(super) async fn load_completion_context_provenance(
        &self,
        id: &ResponseItemId,
    ) -> CodexResult<Option<ResponseItem>> {
        if self.live_thread().is_some() {
            return self
                .services
                .thread_store
                .load_sub_agent_completion_context_item(LoadSubAgentCompletionContextItemParams {
                    thread_id: self.thread_id,
                    include_archived: false,
                    response_item_id: id.clone(),
                })
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()));
        }
        Ok(self
            .state
            .lock()
            .await
            .completion_runtime_provenance
            .contexts
            .get(id)
            .map(|envelope| envelope.item.clone()))
    }
}

#[cfg(test)]
#[path = "completion_provenance_tests.rs"]
mod tests;
