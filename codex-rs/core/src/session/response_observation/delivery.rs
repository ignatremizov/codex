//! Observer claims become acknowledgements only after the canonical worker commits.
//! Cancellation of a receipt never revokes accepted work or authorizes another append.

use super::*;
use crate::agent::control::CompletionPresentation;
use crate::session::AcceptedCompletionDelivery;
use codex_history::AgentResponseObservation;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::TurnStartOptions;

mod presentation;
#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;

struct DeliveryOutcome {
    session: Arc<Session>,
    finished: bool,
}

impl Drop for DeliveryOutcome {
    fn drop(&mut self) {
        if !self.finished {
            self.session.quarantine_history(
                "response observation worker lost its canonical receipt".to_string(),
            );
        }
    }
}

enum Payload {
    Context {
        communication: InterAgentCommunication,
        presentation: Option<CompletionPresentation>,
    },
    Presentation(CompletionPresentation),
}

fn copy_presentation(presentation: &CompletionPresentation) -> CompletionPresentation {
    CompletionPresentation {
        item: presentation.item.clone(),
        history_only_turn_id: presentation.history_only_turn_id.clone(),
    }
}

impl Session {
    pub(crate) async fn persist_agent_response_observations(
        self: &Arc<Self>,
        observations: &[AgentResponseObservation],
    ) -> CodexResult<()> {
        self.check_history_publication()?;
        if observations.is_empty() {
            return Ok(());
        }
        let mut outcome = DeliveryOutcome {
            session: Arc::clone(self),
            finished: false,
        };
        let permit = self.acquire_history_publication_barrier().await?;
        let receiver = self.dispatch_completion_publication(
            permit,
            observations
                .iter()
                .cloned()
                .map(RolloutItem::AgentResponseObservation)
                .collect(),
            Vec::new(),
            |_| {},
            || {},
        )?;
        self.publication_result(receiver).await?;
        outcome.finished = true;
        Ok(())
    }

    pub(crate) fn register_terminal_communication_delivery(
        &self,
        commit: ResponseObservationDeliveryCommit,
        accepted: AcceptedCompletionDelivery,
        presentation: &CompletionPresentation,
    ) -> CodexResult<CommunicationDeliveryReceipt> {
        let id = commit.response_item_id.clone();
        let receipt = self.register_communication_delivery(commit, accepted)?;
        if let Some(delivery) = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .communication_deliveries
            .get_mut(&id)
        {
            delivery.presentation = Some(copy_presentation(presentation));
        }
        Ok(receipt)
    }

    /// Only a registered exact-instance reservation can bypass ordinary shutdown admission.
    pub(crate) async fn enqueue_registered_observed_communication(
        self: &Arc<Self>,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        let id = communication.id.as_ref().ok_or_else(|| {
            CodexErr::InvalidRequest("observed communication has no identity".to_string())
        })?;
        {
            let mut state = self
                .response_observation_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let delivery = state.communication_deliveries.get_mut(id).ok_or_else(|| {
                CodexErr::InvalidRequest("observed communication is not registered".to_string())
            })?;
            if delivery.communication.is_some() {
                return Err(CodexErr::InvalidRequest(
                    "observed communication was already queued".to_string(),
                ));
            }
            delivery.communication = Some(communication.clone());
        }
        let id = id.clone();
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let result = session
                .enqueue_observed_worker(communication, start_options)
                .await;
            if let Err(error) = &result
                && let Some(delivery) = session
                    .response_observation_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .communication_deliveries
                    .remove(&id)
            {
                let _ = delivery
                    .sender
                    .send(Err(CodexErr::Fatal(error.to_string())));
            }
            result
        })
        .await
        .map_err(|error| {
            self.quarantine_history(format!("observed enqueue worker failed: {error}"));
            CodexErr::Fatal(format!("observed enqueue worker failed: {error}"))
        })?
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "accepted admission covers mailbox enqueue before shutdown can drain"
    )]
    async fn enqueue_observed_worker(
        self: &Arc<Self>,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        let id = communication.id.as_ref().ok_or_else(|| {
            CodexErr::InvalidRequest("observed communication has no identity".to_string())
        })?;
        let accepted = {
            let state = self
                .response_observation_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.consumed_deliveries.contains(id) {
                return Ok(crate::session::new_submission_id());
            }
            Arc::clone(
                &state
                    .communication_deliveries
                    .get(id)
                    .ok_or_else(|| {
                        CodexErr::InvalidRequest(
                            "observed communication is not registered".to_string(),
                        )
                    })?
                    .accepted,
            )
        };
        let order = self
            .submission_admission
            .admit_completion(&accepted)
            .await?;
        self.check_history_publication()?;
        if self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .consumed_deliveries
            .contains(id)
        {
            return Ok(crate::session::new_submission_id());
        }
        let sub_id = crate::session::new_submission_id();
        if self.submission_admission.completion_is_closing() {
            // Shutdown may already be waiting on this reservation. Do not enqueue behind it.
            let session = Arc::clone(self);
            tokio::spawn(async move {
                session.consume_observed_communication(&communication).await;
            });
        } else {
            let trigger_turn = communication.trigger_turn;
            self.input_queue
                .enqueue_mailbox_communication(communication, start_options)
                .await;
            drop(order);
            if trigger_turn || self.has_outstanding_durable_sleep() {
                let session = Arc::clone(self);
                let wake_id = sub_id.clone();
                tokio::spawn(async move {
                    session
                        .maybe_start_turn_for_pending_work_with_sub_id(wake_id)
                        .await;
                });
            }
            return Ok(sub_id);
        }
        Ok(sub_id)
    }

    pub(in crate::session) async fn consume_observed_communication(
        self: &Arc<Self>,
        communication: &InterAgentCommunication,
    ) -> bool {
        let Some(id) = communication.id.as_ref() else {
            return false;
        };
        let delivery = {
            let mut state = self
                .response_observation_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.consumed_deliveries.contains(id) {
                return true;
            }
            let matches = state
                .communication_deliveries
                .get(id)
                .and_then(|delivery| delivery.communication.as_ref())
                == Some(communication);
            if !matches {
                return false;
            }
            let delivery = state.communication_deliveries.remove(id);
            state.consumed_deliveries.insert(id.clone());
            delivery
        };
        let Some(delivery) = delivery else {
            return false;
        };
        let session = Arc::clone(self);
        let communication = communication.clone();
        // Removing the registration transfers the reservation to this independently driven worker.
        let worker = tokio::spawn(async move {
            let id = communication.id.clone();
            let result = session
                .persist_observation_payload(
                    delivery.commit,
                    delivery.accepted,
                    Payload::Context {
                        communication,
                        presentation: delivery.presentation,
                    },
                )
                .await;
            if result.is_ok()
                && let Some(id) = id.as_ref()
            {
                session
                    .input_queue
                    .acknowledge_completion_communication(id)
                    .await;
            }
            let _ = delivery.sender.send(result);
        });
        if let Err(error) = worker.await {
            self.quarantine_history(format!("observed communication worker failed: {error}"));
        }
        true
    }

    pub(in crate::session) async fn drain_observed_communications(self: &Arc<Self>) {
        let communications = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .communication_deliveries
            .values()
            .filter_map(|delivery| delivery.communication.clone())
            .collect::<Vec<_>>();
        for communication in communications {
            self.consume_observed_communication(&communication).await;
        }
    }

    pub(crate) async fn persist_observed_terminal_response(
        self: &Arc<Self>,
        communication: InterAgentCommunication,
        presentation: &CompletionPresentation,
        commit: ResponseObservationDeliveryCommit,
        accepted: AcceptedCompletionDelivery,
    ) -> CodexResult<()> {
        self.persist_observation_payload(
            commit,
            Arc::new(accepted),
            Payload::Context {
                communication,
                presentation: Some(copy_presentation(presentation)),
            },
        )
        .await
    }

    pub(crate) async fn persist_observed_presentation(
        self: &Arc<Self>,
        presentation: &CompletionPresentation,
        commit: ResponseObservationDeliveryCommit,
        accepted: AcceptedCompletionDelivery,
    ) -> CodexResult<()> {
        self.persist_observation_payload(
            commit,
            Arc::new(accepted),
            Payload::Presentation(copy_presentation(presentation)),
        )
        .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "accepted delivery orders its canonical append and immutable presentation turn"
    )]
    async fn persist_observation_payload(
        self: &Arc<Self>,
        commit: ResponseObservationDeliveryCommit,
        accepted: Arc<AcceptedCompletionDelivery>,
        payload: Payload,
    ) -> CodexResult<()> {
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let mut outcome = DeliveryOutcome {
                session: Arc::clone(&session),
                finished: false,
            };
            let result = async {
                if commit.parent != session.presentation_id() {
                    return Err(CodexErr::InvalidRequest(
                        "observed response belongs to another session instance".to_string(),
                    ));
                }
                let order = session
                    .submission_admission
                    .admit_completion(&accepted)
                    .await?;
                let control = &session.services.agent_control;
                let _transaction = control
                    .acquire_response_observation_transaction(commit.parent)
                    .await;
                let permit = session.acquire_history_publication_barrier().await?;
                let turn = session.new_history_only_turn().await;
                let policy = turn.model_info().truncation_policy.into();
                let (response, presentation, trigger_turn) = match payload {
                    Payload::Context {
                        communication,
                        presentation,
                    } => {
                        let response = communication.to_model_input_item();
                        if response.id() != Some(&commit.response_item_id) {
                            return Err(CodexErr::InvalidRequest(
                                "observed response identity changed".to_string(),
                            ));
                        }
                        (Some(response), presentation, communication.trigger_turn)
                    }
                    Payload::Presentation(presentation) => (None, Some(presentation), false),
                };
                let active = session.active_turn.lock().await;
                let (mut records, events) = match presentation {
                    Some(presentation) => presentation::records(
                        session.thread_id,
                        active
                            .as_ref()
                            .and_then(|active| active.task.as_ref())
                            .map(|task| task.turn_context.sub_id.as_str()),
                        presentation,
                    ),
                    None => (Vec::new(), Vec::new()),
                };
                if let Some(response) = &response {
                    records.push(RolloutItem::InterAgentCommunicationMetadata { trigger_turn });
                    records.push(RolloutItem::ResponseItem(response.clone().into()));
                }
                records.extend(
                    control
                        .deferred_response_observation_commit_snapshots(&commit)
                        .into_iter()
                        .map(RolloutItem::AgentResponseObservation),
                );
                let install_control = control.clone();
                let install_commit = commit.clone();
                let receiver = session.dispatch_completion_publication(
                    permit,
                    records,
                    events,
                    move |state| {
                        if let Some(response) = response {
                            if !state.history.raw_items().any(|item| item == &response) {
                                state.record_items(std::iter::once(&response), policy);
                            }
                            if !state
                                .acknowledged_completion_contexts
                                .iter()
                                .any(|retained| retained.item.item == response)
                            {
                                state.acknowledged_completion_contexts.push(
                                    crate::state::AcknowledgedCompletionContext {
                                        item: response.into(),
                                        pending: false,
                                    },
                                );
                            }
                        }
                        install_control.commit_response_observation_delivery(&install_commit);
                    },
                    || {},
                )?;
                drop(order);
                session.publication_result(receiver).await?;
                drop(active);
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                session
                    .quarantine_history(format!("observed response publication failed: {error}"));
            }
            outcome.finished = true;
            result
        })
        .await
        .map_err(|error| {
            self.quarantine_history(format!("observed response lost its receipt: {error}"));
            CodexErr::Fatal(format!("observed response lost its receipt: {error}"))
        })?
    }
}
