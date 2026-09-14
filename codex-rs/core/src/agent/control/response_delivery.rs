//! Single-attempt canonical observation delivery to a retained exact observer.

use super::presentation::CommentaryDeliveryRoute;
use super::presentation::WatcherTerminalPresentation;
use super::*;
use crate::session_prefix::format_subagent_commentary_message;
use crate::session_prefix::format_subagent_notification_message;

impl LocalAgentControl {
    pub(super) async fn persist_response_observation_snapshot(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let observer = state.get_thread(parent.thread_id).await?;
        if observer.session.presentation_id() != parent {
            return Err(CodexErr::ThreadNotFound(parent.thread_id));
        }
        observer
            .session
            .persist_agent_response_observations(
                &self.response_observation_snapshots(parent, child),
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn deliver_observed_commentary(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        generation: u64,
        turn_id: &str,
        item_id: &str,
        text: &str,
        sequence: u64,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let lifecycle = state
            .v2_spawn_resume_lock(child.thread_id)
            .lock_owned()
            .await;
        if !state.agent_lifecycle_generation_is_current(child.thread_id, generation) {
            return Ok(());
        }
        let Ok(target) = state.get_thread(child.thread_id).await else {
            return Ok(());
        };
        let Ok(observer) = state.get_thread(parent.thread_id).await else {
            return Ok(());
        };
        if target.session.presentation_id() != child || observer.session.presentation_id() != parent
        {
            return Ok(());
        }
        let submission = self.state.mailbox_submission(parent.thread_id);
        let mailbox = Arc::clone(&submission.semaphore)
            .acquire_owned()
            .await
            .map_err(|error| CodexErr::Fatal(format!("observer mailbox closed: {error}")))?;
        let transaction = self.acquire_response_observation_transaction(parent).await;
        if observer.session.submission_admission.check_ready().is_err() {
            return Ok(());
        }
        let Some(accepted) = observer
            .session
            .submission_admission
            .try_accept_completion_delivery()
        else {
            return Ok(());
        };
        let Some(delivery) = self.prepare_commentary_observation_delivery_at_sequence(
            parent, child, turn_id, item_id, text, sequence,
        ) else {
            return Ok(());
        };
        observer
            .session
            .persist_agent_response_observations(
                &self.response_observation_snapshots(parent, child),
            )
            .await?;
        let commit = ResponseObservationDeliveryCommit {
            parent,
            child,
            turn_id: turn_id.to_string(),
            response_item_id: delivery.response_item_id.clone(),
            kind: ResponseObservationDeliveryKind::Commentary,
            mailbox_final_subscription_message_id: None,
            model_visibility: codex_protocol::protocol::SubAgentCompletionModelVisibility::Visible,
        };
        let agent = self
            .model_visible_agent_identity_for_version(
                observer
                    .multi_agent_version()
                    .unwrap_or(MultiAgentVersion::V1),
                child.thread_id,
            )
            .await?;
        let mut communication = InterAgentCommunication::new(
            target
                .session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root),
            observer
                .session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root),
            Vec::new(),
            format_subagent_commentary_message(agent, turn_id, item_id, text),
            /*trigger_turn*/ true,
        );
        communication.id = Some(delivery.response_item_id.clone());
        // Admission can wait for rollback, which needs this transaction for its checkpoint.
        drop(transaction);
        drop(lifecycle);
        drop(mailbox);
        self.publish_response_observation_binding();
        loop {
            let changed = self.response_observation_changed().notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.response_observation_delivery_committed(
                parent,
                child,
                turn_id,
                &delivery.response_item_id,
            ) {
                return Ok(());
            }
            match self.route_response_observer_commentary(parent, child, turn_id) {
                CommentaryDeliveryRoute::Wait => changed.await,
                CommentaryDeliveryRoute::Mailbox | CommentaryDeliveryRoute::Undecided => break,
            }
        }
        let receipt = observer
            .session
            .register_communication_delivery(commit, accepted)?;
        observer
            .session
            .enqueue_registered_observed_communication(communication, TurnStartOptions::default())
            .await?;
        // Consumption recomputes the snapshot under a fresh transaction. Never hold either
        // lock across the receipt, or an idle target/observer pair can deadlock each other.
        receipt.recv().await
    }

    pub(super) async fn deliver_observed_terminal(
        &self,
        child: SessionPresentationId,
        terminal: WatcherTerminalPresentation,
    ) -> CodexResult<()> {
        let parent = terminal.presentation.parent();
        let Some(observer) = terminal.presentation.take_parent_thread() else {
            // A native consumer or an already committed wait can win before a late
            // observer reconciliation. Never turn consumed history into a new receipt.
            let _transaction = self.acquire_response_observation_transaction(parent).await;
            self.finish_response_observation_turn(parent, child, &terminal.turn_id);
            return Ok(());
        };
        let Some(accepted) = terminal.presentation.take_accepted_completion_delivery() else {
            return Ok(());
        };
        // The exact accepted capability already owns this obligation. Reacquiring a target
        // lifecycle gate here would deadlock mutually observing agents closing concurrently.
        let submission = self.state.mailbox_submission(parent.thread_id);
        let mailbox = Arc::clone(&submission.semaphore)
            .acquire_owned()
            .await
            .map_err(|error| CodexErr::Fatal(format!("observer mailbox closed: {error}")))?;
        let transaction = self.acquire_response_observation_transaction(parent).await;
        let context_id = terminal.presentation.completion_context_response_item_id();
        let (mut disposition, _) = self.prepare_final_response_observation_delivery(
            parent,
            child,
            &terminal.turn_id,
            &context_id,
        );
        let mut queued = self.response_observation_queue_delivery(parent, child, &terminal.turn_id);
        if disposition == FinalResponseObservation::None {
            self.finish_response_observation_turn(parent, child, &terminal.turn_id);
            self.claim_completion_context_response_item_id(parent, &context_id);
            return Ok(());
        }
        let presentation = if disposition == FinalResponseObservation::PresentationOnly {
            terminal
                .presentation
                .hidden_observation_presentation(&self.observation_reference(child.thread_id))
                .ok_or_else(|| {
                    CodexErr::Fatal("terminal has no completion presentation".to_string())
                })?
        } else {
            terminal.presentation.completion_presentation()
        };
        let snapshots = self.response_observation_snapshots(parent, child);
        observer
            .session
            .persist_agent_response_observations(&snapshots)
            .await?;
        let commit = ResponseObservationDeliveryCommit {
            parent,
            child,
            turn_id: terminal.turn_id.clone(),
            response_item_id: context_id.clone(),
            kind: ResponseObservationDeliveryKind::Final,
            mailbox_final_subscription_message_id: self.mailbox_final_subscription_for_turn(
                parent,
                child,
                &terminal.turn_id,
            ),
            model_visibility: if disposition == FinalResponseObservation::PresentationOnly {
                codex_protocol::protocol::SubAgentCompletionModelVisibility::NotVisible
            } else {
                codex_protocol::protocol::SubAgentCompletionModelVisibility::Visible
            },
        };
        // Exec is a one-shot host, not a daemon waiting beyond its primary turn.
        if (disposition == FinalResponseObservation::Wake || queued)
            && observer
                .session
                .app_server_client_metadata()
                .await
                .client_name
                .as_deref()
                == Some("codex_exec")
            && (queued
                || observer
                    .session
                    .active_turn
                    .lock()
                    .await
                    .as_ref()
                    .is_none_or(|turn| turn.task.is_none()))
        {
            disposition = FinalResponseObservation::Passive;
            queued = false;
        }
        // Wait arbitration may itself need this transaction to commit its canonical response.
        drop(transaction);
        drop(mailbox);
        if terminal.presentation.wait_owns_presentation().await {
            // Wait ownership commits only after its canonical snapshots and primary row.
            // The same accepted capability needs no second append or acknowledgement.
            drop(accepted);
        } else {
            match disposition {
                FinalResponseObservation::None => {}
                FinalResponseObservation::PresentationOnly => {
                    observer
                        .session
                        .persist_observed_presentation(presentation, commit, accepted)
                        .await?;
                }
                FinalResponseObservation::Passive | FinalResponseObservation::Wake => {
                    let agent = self
                        .model_visible_agent_identity_for_version(
                            observer
                                .multi_agent_version()
                                .unwrap_or(MultiAgentVersion::V1),
                            child.thread_id,
                        )
                        .await?;
                    let mut communication = InterAgentCommunication::new(
                        self.get_agent_metadata(child.thread_id)
                            .and_then(|metadata| metadata.agent_path)
                            .unwrap_or_else(AgentPath::root),
                        observer
                            .session_source
                            .get_agent_path()
                            .unwrap_or_else(AgentPath::root),
                        Vec::new(),
                        format_subagent_notification_message(agent, &terminal.status),
                        queued || disposition == FinalResponseObservation::Wake,
                    );
                    communication.id = Some(context_id.clone());
                    communication.defer_to_next_turn = queued;
                    if disposition == FinalResponseObservation::Passive && !queued {
                        observer
                            .session
                            .persist_observed_terminal_response(
                                communication,
                                presentation,
                                commit,
                                accepted,
                            )
                            .await?;
                    } else {
                        let receipt = observer.session.register_terminal_communication_delivery(
                            commit,
                            accepted,
                            presentation,
                        )?;
                        observer
                            .session
                            .enqueue_registered_observed_communication(
                                communication,
                                TurnStartOptions::default(),
                            )
                            .await?;
                        receipt.recv().await?;
                    }
                }
            }
        }
        let _transaction = self.acquire_response_observation_transaction(parent).await;
        self.finish_response_observation_turn(parent, child, &terminal.turn_id);
        self.claim_completion_context_response_item_id(parent, &context_id);
        Ok(())
    }

    fn observation_reference(&self, thread_id: ThreadId) -> String {
        self.get_agent_metadata(thread_id)
            .and_then(|metadata| metadata.agent_path)
            .map_or_else(|| thread_id.to_string(), |path| path.to_string())
    }
}
