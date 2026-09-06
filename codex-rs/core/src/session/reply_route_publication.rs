//! Acknowledged singleton publication, independent of runtime reply permission.

use std::sync::Arc;

use codex_history::ResponseItemEnvelope;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;

use super::Session;
use super::prepared_history_items::PreparedHistoryItems;

#[cfg(test)]
#[path = "reply_route_publication_tests.rs"]
mod tests;

struct RoutePublication {
    session: Arc<Session>,
    finished: bool,
}

impl Drop for RoutePublication {
    fn drop(&mut self) {
        if !self.finished {
            self.session.quarantine_history(
                "reply-route context lost its canonical receipt; reload required, do not retry"
                    .to_string(),
            );
        }
    }
}

impl Session {
    /// Publish the singleton before a source may acknowledge and activate its live permission.
    /// Reuse only verified retained context; a failed append is never retried.
    pub(crate) async fn publish_persistent_reply_route(
        self: &Arc<Self>,
        mut item: ResponseItem,
    ) -> CodexResult<()> {
        Self::assign_missing_response_item_id(&mut item);
        let mut envelope = ResponseItemEnvelope::new(item);
        let source =
            codex_history::persistent_agent_reply_route_source(&envelope).ok_or_else(|| {
                CodexErr::InvalidRequest("invalid persistent reply-route context".into())
            })?;
        let session = Arc::clone(self);
        // Capture abandonment ownership before the spawned future's first poll.
        let outcome = RoutePublication {
            session: Arc::clone(&session),
            finished: false,
        };
        tokio::spawn(async move {
            let mut outcome = outcome;
            let result = async {
                let permit = session.acquire_history_publication_barrier().await?;
                if session
                    .state
                    .lock()
                    .await
                    .history
                    .annotated_items()
                    .iter()
                    .any(|item| {
                        codex_history::persistent_agent_reply_route_source(item) == Some(source)
                    })
                {
                    return Ok(());
                }
                let turn = session.new_history_only_turn().await;
                let policy = turn.model_info().truncation_policy.into();
                Self::stamp_response_item_for_history(&mut envelope.item, &turn.sub_id);
                let mut mcp_revision = None;
                let prepared = {
                    let mut state = session.state.lock().await;
                    envelope.metadata.get_or_insert_default().user_input_order =
                        Some(state.history.reserve_input_order());
                    if let Some((attribution, revision)) = session
                        .services
                        .executed_tool_calls
                        .mcp_attribution_checkpoint(/*force*/ false)
                    {
                        envelope.metadata.get_or_insert_default().mcp_attribution =
                            Some(attribution);
                        mcp_revision = Some(revision);
                    }
                    PreparedHistoryItems::new(&state.history, vec![envelope.clone()], policy)
                };
                let executed_tool_calls = session.services.executed_tool_calls.clone();
                let receiver = session.dispatch_completion_publication(
                    permit,
                    prepared.rollout_items(),
                    Vec::new(),
                    move |state| {
                        state
                            .current_time_reminder
                            .note_recorded_items(std::slice::from_ref(&envelope.item));
                        prepared.install(&mut state.history);
                        if let Some(revision) = mcp_revision {
                            executed_tool_calls.mark_mcp_attribution_persisted(revision);
                        }
                    },
                    || {},
                )?;
                session.publication_result(receiver).await?;
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                session.quarantine_history(format!(
                    "reply-route context publication failed: {error}; do not retry"
                ));
            }
            outcome.finished = true;
            result
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("reply-route context worker failed: {error}")))?
    }
}
