//! Acknowledged singleton publication, independent of runtime reply permission.

use std::sync::Arc;

use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;

use super::Session;

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
        let envelope = ResponseItemEnvelope::new(item);
        let source =
            codex_history::persistent_agent_reply_route_source(&envelope).ok_or_else(|| {
                CodexErr::InvalidRequest("invalid persistent reply-route context".into())
            })?;
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let mut outcome = RoutePublication {
                session: Arc::clone(&session),
                finished: false,
            };
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
                let receiver = session.dispatch_completion_publication(
                    permit,
                    vec![RolloutItem::ResponseItem(envelope.clone())],
                    Vec::new(),
                    move |state| {
                        state.record_items(std::iter::once(&envelope.item), policy);
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
