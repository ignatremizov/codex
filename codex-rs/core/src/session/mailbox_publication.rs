//! Quarantines an abandoned or failed mailbox publication before its writer barrier is released.

use super::Session;
use super::turn_context::TurnContext;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_thread_store::ThreadStoreError;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::with_serialization_allowance;
use std::sync::Arc;

#[cfg(test)]
#[path = "mailbox_publication_tests.rs"]
mod tests;

pub(super) struct MailboxPublicationOutcome {
    pub(super) session: Arc<Session>,
    pub(super) finished: bool,
}

impl Drop for MailboxPublicationOutcome {
    fn drop(&mut self) {
        if !self.finished {
            self.session.quarantine_history(
                "mailbox publication outcome unknown; recover canonical evidence before continuing"
                    .to_string(),
            );
        }
    }
}

impl Session {
    /// Capture source revisions before the canonical append. The caller owns the
    /// history/settings barrier, and acknowledges the MCP revision only after flush.
    /// A recovered envelope must be replayed verbatim, not prepared a second time.
    pub(super) async fn prepare_mailbox_context(
        &self,
        model_info: &ModelInfo,
        mut envelope: ResponseItemEnvelope,
    ) -> (ResponseItemEnvelope, Option<u64>) {
        let policy: TruncationPolicy = model_info.truncation_policy.into();
        if matches!(
            &envelope.item,
            ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. }
        ) {
            envelope
                .metadata
                .get_or_insert_default()
                .history_truncation_token_limit
                .get_or_insert_with(|| with_serialization_allowance(policy).token_budget());
        }
        let force_checkpoint = crate::context_manager::is_user_turn_boundary(&envelope.item);
        let mcp_revision = self
            .services
            .executed_tool_calls
            .mcp_attribution_checkpoint(force_checkpoint)
            .map(|(attribution, revision)| {
                envelope.metadata.get_or_insert_default().mcp_attribution = Some(attribution);
                revision
            });
        let mut state = self.state.lock().await;
        if crate::context::is_user_authorization_message(&envelope.item) {
            envelope
                .metadata
                .get_or_insert_default()
                .user_input_order
                .get_or_insert_with(|| state.history.reserve_input_order());
        }
        let mut projected = state.history.clone();
        let mut envelopes = [envelope];
        projected.record_annotated_items(&mut envelopes, policy);
        let [envelope] = envelopes;
        (envelope, mcp_revision)
    }

    /// Install a persisted original without inventing another source revision or
    /// replacing input-order reservations made while canonical I/O was pending.
    pub(super) async fn insert_mailbox_context(
        &self,
        turn: &TurnContext,
        model_info: &ModelInfo,
        envelope: ResponseItemEnvelope,
    ) -> Result<bool, ThreadStoreError> {
        {
            let mut state = self.state.lock().await;
            if state
                .history
                .raw_items()
                .any(|item| item.id() == envelope.id())
            {
                return Ok(false);
            }
            state
                .current_time_reminder
                .note_recorded_items(std::slice::from_ref(&envelope.item));
            state
                .history
                .replay_annotated_item(&envelope, model_info.truncation_policy.into());
        }
        self.tx_event
            .send(Event {
                id: turn.sub_id.clone(),
                msg: EventMsg::RawResponseItem(codex_protocol::protocol::RawResponseItemEvent {
                    item: envelope.item,
                }),
            })
            .await
            .map_err(|_| ThreadStoreError::Conflict {
                message: "mailbox raw event stream closed after canonical commit".to_string(),
            })?;
        Ok(true)
    }
}
