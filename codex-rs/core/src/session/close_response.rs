//! Close replay has its own accepted capability, not an invented response-observer commit.

use super::AcceptedCompletionDelivery;
use super::Session;
use super::new_submission_id;
use super::sub_agent_completion::CompletionContextDelivery;
use super::sub_agent_completion::CompletionContextPublication;
use crate::agent::control::CompletionPresentation;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::TurnStartOptions;
use std::sync::Arc;

struct ClosePublication {
    session: Arc<Session>,
    finished: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionContextState {
    /// A positively acknowledged payload is installed or retained for consumption.
    Present,
    /// A positive receipt exists, but replacement history removed its live context.
    SettledRemoved,
    /// No positive receipt exists; absence must not authorize duplicate delivery.
    Unacknowledged,
}

impl Drop for ClosePublication {
    fn drop(&mut self) {
        if !self.finished {
            self.session.quarantine_history(
                "accepted close replay lost its receipt; do not retry".to_string(),
            );
        }
    }
}

impl Session {
    pub(crate) async fn completion_context_state(
        &self,
        id: &codex_protocol::ResponseItemId,
    ) -> CodexResult<CompletionContextState> {
        let _permit = self.acquire_history_publication_barrier().await?;
        let canonical = self
            .services
            .thread_store
            .load_sub_agent_completion_context_item(
                codex_thread_store::LoadSubAgentCompletionContextItemParams {
                    thread_id: self.thread_id,
                    include_archived: false,
                    response_item_id: id.clone(),
                },
            )
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?;
        let received = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .settled_completion_contexts
            .get(id)
            .cloned();
        if canonical
            .as_ref()
            .zip(received.as_ref())
            .is_some_and(|(canonical, received)| canonical != received)
        {
            self.quarantine_history("completion receipt payload changed".to_string());
            return Err(CodexErr::InvalidRequest(
                "completion receipt payload changed".to_string(),
            ));
        }
        let Some(proven) = canonical.or(received) else {
            return Ok(CompletionContextState::Unacknowledged);
        };
        let state = self.state.lock().await;
        let present = state.history.raw_items().any(|item| item == &proven)
            || state
                .acknowledged_completion_contexts
                .iter()
                .any(|context| context.pending && context.item.item == proven);
        Ok(if present {
            CompletionContextState::Present
        } else {
            CompletionContextState::SettledRemoved
        })
    }

    pub(crate) async fn publish_close_response(
        self: &Arc<Self>,
        communication: InterAgentCommunication,
        presentation: CompletionPresentation,
        accepted: AcceptedCompletionDelivery,
    ) -> CodexResult<()> {
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let mut outcome = ClosePublication {
                session: Arc::clone(&session),
                finished: false,
            };
            let queued = communication.trigger_turn || communication.defer_to_next_turn;
            let publication = session
                .persist_completion_context(
                    communication.to_model_input_item(),
                    &accepted,
                    if queued {
                        CompletionContextDelivery::QueueOnly
                    } else {
                        CompletionContextDelivery::InstallNow
                    },
                )
                .await?;
            if publication == CompletionContextPublication::AlreadyPublished {
                outcome.finished = true;
                return Ok(());
            }
            session
                .publish_completion_item(&presentation, &accepted)
                .await?;
            if queued {
                // Admission covers enqueue, but no lifecycle or publication gate spans wakeup.
                let order = session
                    .submission_admission
                    .admit_completion(&accepted)
                    .await?;
                session.check_history_publication()?;
                let trigger_turn = communication.trigger_turn;
                let start_options = TurnStartOptions {
                    turn_trigger: trigger_turn.then(|| "agent_wake".to_string()),
                    ..Default::default()
                };
                if communication.defer_to_next_turn {
                    session
                        .input_queue
                        .queue_turn_inputs_for_next_turn(
                            vec![super::TurnInput::InterAgentCommunication(communication)],
                            start_options,
                        )
                        .await;
                } else {
                    session
                        .input_queue
                        .enqueue_mailbox_communication(communication, start_options)
                        .await;
                }
                let closing = session.submission_admission.completion_is_closing();
                drop(order);
                if !closing && (trigger_turn || session.has_outstanding_durable_sleep()) {
                    session
                        .maybe_start_turn_for_pending_work_with_sub_id(new_submission_id())
                        .await;
                }
            }
            outcome.finished = true;
            Ok(())
        })
        .await
        .map_err(|error| {
            self.quarantine_history(format!(
                "accepted close replay failed: {error}; do not retry"
            ));
            CodexErr::Fatal(format!("accepted close replay failed: {error}"))
        })?
    }
}
