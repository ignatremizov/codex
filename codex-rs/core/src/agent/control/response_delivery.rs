use super::presentation::ResponseObservationDeliveryKind;
use super::presentation::ResponseObservationEventMatch;
use super::presentation::WatcherTerminalPresentation;
use super::*;
use crate::session::AgentResponseEvent;
use crate::session::AgentResponseSubscription;
use crate::session_prefix::format_subagent_commentary_message;
use crate::session_prefix::format_subagent_notification_message;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use std::collections::HashSet;

pub(super) enum WatcherTerminalPoll {
    Terminal(WatcherTerminalPresentation),
    Retry,
    Closed,
}

impl AgentControl {
    pub(super) async fn persist_response_observation_snapshot_transactionally(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> bool {
        let _transaction_permit = self.acquire_response_observation_transaction(parent).await;
        self.persist_response_observation_snapshot(parent, child)
            .await
    }

    pub(super) async fn next_watcher_terminal(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        child_reference: &str,
        response_rx: &mut AgentResponseSubscription,
        child_multi_agent_version: MultiAgentVersion,
        child_lifecycle_generation: u64,
    ) -> WatcherTerminalPoll {
        loop {
            // Register before checking the generation so an explicit close cannot notify between
            // the check and the blocking response receive. This is a global lifecycle signal;
            // unrelated thread changes simply cause another generation check. Keep only the
            // notification handle across the receive so the watcher cannot retain the manager.
            let lifecycle_changed = match self.upgrade() {
                Ok(state) => state.wait_for_agent_lifecycle_change(),
                Err(_) => return WatcherTerminalPoll::Closed,
            };
            let watcher_terminal_changed = self
                .wait_agent_presentations
                .watcher_terminal_changed
                .notified();
            tokio::pin!(lifecycle_changed);
            tokio::pin!(watcher_terminal_changed);
            lifecycle_changed.as_mut().enable();
            watcher_terminal_changed.as_mut().enable();
            if !self
                .agent_lifecycle_generation_is_current(child.thread_id, child_lifecycle_generation)
            {
                return WatcherTerminalPoll::Closed;
            }
            if child_multi_agent_version == MultiAgentVersion::V1
                && let Some((turn_id, delivery)) =
                    self.pending_response_observation_commentary_delivery(parent, child)
            {
                let Some(_lifecycle_guard) = self
                    .acquire_current_agent_lifecycle(child.thread_id, child_lifecycle_generation)
                    .await
                else {
                    return WatcherTerminalPoll::Closed;
                };
                if !self
                    .deliver_v1_commentary_observation(
                        parent,
                        child,
                        child_reference,
                        &turn_id,
                        &delivery,
                    )
                    .await
                {
                    return WatcherTerminalPoll::Retry;
                }
            }
            if let Some(terminal) = self.take_watcher_terminal_presentation(parent, child) {
                let Some(_lifecycle_guard) = self
                    .acquire_current_agent_lifecycle(child.thread_id, child_lifecycle_generation)
                    .await
                else {
                    return WatcherTerminalPoll::Closed;
                };
                match self
                    .observe_queued_watcher_terminal(
                        parent,
                        child,
                        terminal,
                        child_multi_agent_version,
                    )
                    .await
                {
                    Ok(Some(terminal)) => return WatcherTerminalPoll::Terminal(terminal),
                    Ok(None) => continue,
                    Err(terminal) => {
                        self.requeue_watcher_terminal_presentation(parent, child, terminal);
                        return WatcherTerminalPoll::Retry;
                    }
                }
            }
            let response = match tokio::select! {
                biased;
                () = &mut lifecycle_changed => continue,
                () = &mut watcher_terminal_changed => continue,
                response = response_rx.recv() => response,
            } {
                Some(response) => response,
                None => {
                    let Some(terminal) = self.take_watcher_terminal_presentation(parent, child)
                    else {
                        return WatcherTerminalPoll::Closed;
                    };
                    let Some(_lifecycle_guard) = self
                        .acquire_current_agent_lifecycle(
                            child.thread_id,
                            child_lifecycle_generation,
                        )
                        .await
                    else {
                        return WatcherTerminalPoll::Closed;
                    };
                    return match self
                        .observe_queued_watcher_terminal(
                            parent,
                            child,
                            terminal,
                            child_multi_agent_version,
                        )
                        .await
                    {
                        Ok(Some(terminal)) => WatcherTerminalPoll::Terminal(terminal),
                        Ok(None) => WatcherTerminalPoll::Closed,
                        Err(terminal) => {
                            self.requeue_watcher_terminal_presentation(parent, child, terminal);
                            WatcherTerminalPoll::Retry
                        }
                    };
                }
            };
            match response {
                AgentResponseEvent::Commentary {
                    turn_id,
                    item_id,
                    text,
                    sequence,
                } => {
                    if self
                        .await_response_observation_event_match(parent, child, &turn_id)
                        .await
                    {
                        let Some(_lifecycle_guard) = self
                            .acquire_current_agent_lifecycle(
                                child.thread_id,
                                child_lifecycle_generation,
                            )
                            .await
                        else {
                            return WatcherTerminalPoll::Closed;
                        };
                        if !self
                            .deliver_v1_commentary(
                                parent,
                                child,
                                child_reference,
                                &turn_id,
                                &item_id,
                                &text,
                                sequence,
                            )
                            .await
                        {
                            return WatcherTerminalPoll::Retry;
                        }
                    }
                }
                AgentResponseEvent::Terminal {
                    turn_id, status, ..
                } => {
                    if self
                        .await_response_observation_event_match(parent, child, &turn_id)
                        .await
                    {
                        let Some(_lifecycle_guard) = self
                            .acquire_current_agent_lifecycle(
                                child.thread_id,
                                child_lifecycle_generation,
                            )
                            .await
                        else {
                            return WatcherTerminalPoll::Closed;
                        };
                        let _ = self.record_agent_terminal_presentation(
                            parent,
                            child,
                            &turn_id,
                            status,
                            TerminalPresentationDelivery::Watcher,
                            || {},
                        );
                    }
                }
                AgentResponseEvent::TurnAborted { turn_id, .. } => {
                    if child_multi_agent_version == MultiAgentVersion::V1
                        && self
                            .await_response_observation_event_match(parent, child, &turn_id)
                            .await
                    {
                        let Some(_lifecycle_guard) = self
                            .acquire_current_agent_lifecycle(
                                child.thread_id,
                                child_lifecycle_generation,
                            )
                            .await
                        else {
                            return WatcherTerminalPoll::Closed;
                        };
                        // TurnAborted is the selected V1 turn's final outcome. The normal session
                        // publication path records this presentation before publishing the response
                        // event, but synthesize it here as well so abort setup races cannot clear
                        // one-shot observation state before Interrupted is delivered.
                        let _ = self.record_agent_terminal_presentation(
                            parent,
                            child,
                            &turn_id,
                            AgentStatus::Interrupted,
                            TerminalPresentationDelivery::Watcher,
                            || {},
                        );
                    }
                }
                AgentResponseEvent::TurnStarted { turn_id, sequence } => {
                    let Some(_lifecycle_guard) = self
                        .acquire_current_agent_lifecycle(
                            child.thread_id,
                            child_lifecycle_generation,
                        )
                        .await
                    else {
                        return WatcherTerminalPoll::Closed;
                    };
                    let _transaction_permit =
                        self.acquire_response_observation_transaction(parent).await;
                    if self.bind_response_observation_started_turn_at_sequence(
                        parent, child, &turn_id, sequence,
                    ) && child_multi_agent_version == MultiAgentVersion::V1
                        && !self
                            .persist_response_observation_snapshot(parent, child)
                            .await
                    {
                        return WatcherTerminalPoll::Retry;
                    }
                }
            }
        }
    }

    async fn observe_queued_watcher_terminal(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        terminal: WatcherTerminalPresentation,
        child_multi_agent_version: MultiAgentVersion,
    ) -> Result<Option<WatcherTerminalPresentation>, WatcherTerminalPresentation> {
        let _transaction_permit = self.acquire_response_observation_transaction(parent).await;
        if self.response_observation_event_match(parent, child, &terminal.turn_id)
            != ResponseObservationEventMatch::Observe
            && self.bind_response_observation_started_turn_at_sequence(
                parent,
                child,
                &terminal.turn_id,
                /*sequence*/ 0,
            )
            && child_multi_agent_version == MultiAgentVersion::V1
            && !self
                .persist_response_observation_snapshot(parent, child)
                .await
        {
            return Err(terminal);
        }
        if self
            .await_response_observation_event_match(parent, child, &terminal.turn_id)
            .await
        {
            Ok(Some(terminal))
        } else {
            self.finish_watcher_terminal_presentation(parent, child, &terminal.turn_id);
            Ok(None)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn deliver_v1_commentary(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        child_reference: &str,
        turn_id: &str,
        item_id: &str,
        text: &str,
        sequence: u64,
    ) -> bool {
        let Ok(submission_permit) = self
            .acquire_mailbox_submission_permit(parent.thread_id)
            .await
        else {
            return false;
        };
        let _transaction_permit = self.acquire_response_observation_transaction(parent).await;
        let Some(delivery) = self.prepare_commentary_observation_delivery_at_sequence(
            parent, child, turn_id, item_id, text, sequence,
        ) else {
            return true;
        };
        if !self
            .persist_response_observation_snapshot(parent, child)
            .await
        {
            return false;
        }
        self.deliver_v1_commentary_observation_locked(
            parent,
            child,
            child_reference,
            turn_id,
            &delivery,
            submission_permit,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn deliver_recovered_v1_commentary(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        child_reference: &str,
        turn_id: &str,
        item_id: &str,
        text: &str,
        sequence: u64,
        prior_item_ids: &HashSet<String>,
    ) -> bool {
        let Ok(submission_permit) = self
            .acquire_mailbox_submission_permit(parent.thread_id)
            .await
        else {
            return false;
        };
        let _transaction_permit = self.acquire_response_observation_transaction(parent).await;
        let Some(delivery) = self.prepare_recovered_commentary_observation_delivery(
            parent,
            child,
            turn_id,
            item_id,
            text,
            sequence,
            prior_item_ids,
        ) else {
            return true;
        };
        if !self
            .persist_response_observation_snapshot(parent, child)
            .await
        {
            return false;
        }
        self.deliver_v1_commentary_observation_locked(
            parent,
            child,
            child_reference,
            turn_id,
            &delivery,
            submission_permit,
        )
        .await
    }

    pub(super) async fn deliver_v1_commentary_observation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        child_reference: &str,
        turn_id: &str,
        delivery: &codex_protocol::protocol::AgentResponseCommentaryDelivery,
    ) -> bool {
        let Ok(submission_permit) = self
            .acquire_mailbox_submission_permit(parent.thread_id)
            .await
        else {
            return false;
        };
        let _transaction_permit = self.acquire_response_observation_transaction(parent).await;
        if !self
            .persist_response_observation_snapshot(parent, child)
            .await
        {
            return false;
        }
        self.deliver_v1_commentary_observation_locked(
            parent,
            child,
            child_reference,
            turn_id,
            delivery,
            submission_permit,
        )
        .await
    }

    async fn deliver_v1_commentary_observation_locked(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        child_reference: &str,
        turn_id: &str,
        delivery: &codex_protocol::protocol::AgentResponseCommentaryDelivery,
        submission_permit: tokio::sync::OwnedSemaphorePermit,
    ) -> bool {
        let Some(child_agent_path) = self.observation_agent_path(child.thread_id) else {
            return false;
        };
        let Some(parent_agent_path) = self.observation_agent_path(parent.thread_id) else {
            return false;
        };
        let mut communication = InterAgentCommunication::new(
            child_agent_path,
            parent_agent_path,
            Vec::new(),
            format_subagent_commentary_message(
                child_reference,
                child.thread_id,
                turn_id,
                &delivery.source_item_id,
                &delivery.text,
            ),
            /*trigger_turn*/ true,
        );
        let committed_observations = self.response_observation_committed_snapshots(
            parent,
            child,
            turn_id,
            &delivery.response_item_id,
            ResponseObservationDeliveryKind::Commentary,
        );
        if committed_observations.is_empty() {
            self.commit_commentary_observation_delivery(parent, child, turn_id);
            return self
                .persist_response_observation_snapshot(parent, child)
                .await;
        }
        communication.id = Some(delivery.response_item_id.clone());
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Result, child.thread_id);
        if self
            .send_inter_agent_communication_durably(
                parent,
                communication,
                context,
                /*parent_turn_id*/ None,
                committed_observations,
                submission_permit,
            )
            .await
            .is_err()
        {
            return false;
        }
        self.commit_commentary_observation_delivery(parent, child, turn_id);
        self.persist_response_observation_snapshot(parent, child)
            .await
    }

    pub(super) async fn deliver_v1_watcher_terminal(
        &self,
        parent_thread_id: ThreadId,
        child_reference: &str,
        terminal: &WatcherTerminalPresentation,
    ) -> bool {
        // Every lifecycle path takes the destination mailbox before the observer transaction.
        // Keeping that global order prevents two mutually observing agents from forming a
        // mailbox/observation cycle.
        let Ok(submission_permit) = self
            .acquire_mailbox_submission_permit(parent_thread_id)
            .await
        else {
            return false;
        };
        let mut submission_permit = Some(submission_permit);
        let _transaction_permit = self
            .acquire_response_observation_transaction(terminal.presentation.parent())
            .await;
        let terminal_response_item_id = terminal.presentation.completion_context_response_item_id();
        let (final_response_observation, response_item_id) = self
            .prepare_final_response_observation_delivery(
                terminal.presentation.parent(),
                terminal.presentation.child(),
                &terminal.turn_id,
                &terminal_response_item_id,
            );
        if !self
            .persist_response_observation_snapshot(
                terminal.presentation.parent(),
                terminal.presentation.child(),
            )
            .await
        {
            return false;
        }
        if final_response_observation != FinalResponseObservation::None
            && terminal.presentation.wait_owns_presentation().await
        {
            if final_response_observation != FinalResponseObservation::PresentationOnly {
                self.commit_final_response_observation_delivery(
                    terminal.presentation.parent(),
                    terminal.presentation.child(),
                    &terminal.turn_id,
                );
                return self
                    .persist_response_observation_snapshot(
                        terminal.presentation.parent(),
                        terminal.presentation.child(),
                    )
                    .await;
            }
            return true;
        }
        let final_response_observation =
            if final_response_observation == FinalResponseObservation::Wake {
                let Ok(state) = self.upgrade() else {
                    return false;
                };
                let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
                    return false;
                };
                let is_idle_codex_exec = parent_thread
                    .session
                    .app_server_client_metadata()
                    .await
                    .client_name
                    .as_deref()
                    == Some("codex_exec")
                    && parent_thread
                        .session
                        .active_turn
                        .lock()
                        .await
                        .as_ref()
                        .is_none_or(|active_turn| active_turn.task.is_none());
                if is_idle_codex_exec {
                    // A one-shot exec host exits after its primary turn and cannot consume a
                    // synthetic wake. Persist the subscribed result through the passive path so
                    // the transcript remains complete without leaving a durable receipt blocked
                    // on a turn that this host deliberately cannot start.
                    FinalResponseObservation::Passive
                } else {
                    FinalResponseObservation::Wake
                }
            } else {
                final_response_observation
            };
        match final_response_observation {
            FinalResponseObservation::None => return true,
            FinalResponseObservation::PresentationOnly => {
                // `x` still owns the canonical client-visible completion item. That item is
                // persisted as an ItemCompleted event rather than conversation context, so TUI
                // replay remains complete without exposing the payload to a later model request.
                // For a terminal admitted before shutdown, the emission worker takes the
                // presentation's accepted-delivery token before returning from its first attempt.
                // Graceful shutdown waits for that token, so a failed append keeps retrying with
                // stable item and turn IDs before termination even though the observation
                // relationship can now retire.
                let Ok(state) = self.upgrade() else {
                    return false;
                };
                let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
                    return false;
                };
                if parent_thread.session.presentation_id() != terminal.presentation.parent() {
                    return false;
                }
                match terminal.presentation.take_accepted_completion_delivery() {
                    Some(completion_delivery) => {
                        parent_thread
                            .emit_accepted_sub_agent_completion_without_turn(
                                child_reference,
                                &terminal.status,
                                SubAgentCompletionModelVisibility::NotVisible,
                                completion_delivery,
                            )
                            .await;
                    }
                    None => {
                        parent_thread
                            .emit_sub_agent_completion_without_turn(
                                child_reference,
                                &terminal.status,
                                SubAgentCompletionModelVisibility::NotVisible,
                            )
                            .await;
                    }
                }
                return true;
            }
            FinalResponseObservation::Wake => {
                let Some(response_item_id) = response_item_id else {
                    return false;
                };
                let Some(submission_permit) = submission_permit.take() else {
                    return false;
                };
                let delivered = self
                    .deliver_v1_waking_terminal(
                        parent_thread_id,
                        child_reference,
                        terminal,
                        response_item_id,
                        submission_permit,
                    )
                    .await;
                if delivered {
                    self.commit_final_response_observation_delivery(
                        terminal.presentation.parent(),
                        terminal.presentation.child(),
                        &terminal.turn_id,
                    );
                    return self
                        .persist_response_observation_snapshot(
                            terminal.presentation.parent(),
                            terminal.presentation.child(),
                        )
                        .await;
                }
                return false;
            }
            FinalResponseObservation::Passive => {}
        }
        let Some(response_item_id) = response_item_id else {
            return false;
        };
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let message = format_subagent_notification_message(
            child_reference,
            terminal.presentation.child().thread_id,
            &terminal.status,
        );
        let Some(child_agent_path) =
            self.observation_agent_path(terminal.presentation.child().thread_id)
        else {
            return false;
        };
        let Some(parent_agent_path) = self.observation_agent_path(parent_thread_id) else {
            return false;
        };
        let mut communication = InterAgentCommunication::new(
            child_agent_path,
            parent_agent_path,
            Vec::new(),
            message,
            /*trigger_turn*/ false,
        );
        communication.id = Some(response_item_id.clone());
        let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
            return false;
        };
        if parent_thread.session.presentation_id() != terminal.presentation.parent() {
            return false;
        }
        let committed_observations = self.response_observation_committed_snapshots(
            terminal.presentation.parent(),
            terminal.presentation.child(),
            &terminal.turn_id,
            &response_item_id,
            ResponseObservationDeliveryKind::Final,
        );
        if committed_observations.is_empty() {
            return false;
        }
        let accepted_completion_delivery =
            terminal.presentation.take_accepted_completion_delivery();
        let admission = if accepted_completion_delivery.is_some() {
            CompletionSubmissionAdmission::Accepted
        } else {
            CompletionSubmissionAdmission::Ordinary
        };
        if !parent_thread
            .persist_sub_agent_notification_without_turn(
                communication,
                admission,
                committed_observations,
            )
            .await
        {
            if let Some(accepted_completion_delivery) = accepted_completion_delivery {
                terminal
                    .presentation
                    .restore_accepted_completion_delivery(accepted_completion_delivery);
            }
            return false;
        }
        if !terminal.presentation.wait_owns_presentation().await {
            match accepted_completion_delivery {
                Some(completion_delivery) => {
                    parent_thread
                        .emit_accepted_sub_agent_completion_without_turn(
                            child_reference,
                            &terminal.status,
                            SubAgentCompletionModelVisibility::Visible,
                            completion_delivery,
                        )
                        .await;
                }
                None => {
                    parent_thread
                        .emit_sub_agent_completion_without_turn(
                            child_reference,
                            &terminal.status,
                            SubAgentCompletionModelVisibility::Visible,
                        )
                        .await;
                }
            }
        }
        self.commit_final_response_observation_delivery(
            terminal.presentation.parent(),
            terminal.presentation.child(),
            &terminal.turn_id,
        );
        self.persist_response_observation_snapshot(
            terminal.presentation.parent(),
            terminal.presentation.child(),
        )
        .await
    }

    pub(super) async fn finish_and_persist_response_observation_turn(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> bool {
        let _transaction_permit = self.acquire_response_observation_transaction(parent).await;
        let previous_relationship = self.response_observation_relationship_snapshot(parent, child);
        let removed_bound_wake =
            self.response_observation_turn_has_bound_final_wake(parent, child, turn_id);
        let observation_updates = self.finish_response_observation_turn(parent, child, turn_id);
        if self
            .persist_response_observation_updates(parent, observation_updates)
            .await
        {
            drop(_transaction_permit);
            if removed_bound_wake {
                // Persistence can finish after the wake turn already returned to idle. Re-check
                // now that the durable wake record no longer suppresses idle contributors.
                self.recheck_thread_idle_lifecycle(parent).await;
            }
            true
        } else {
            self.restore_response_observation_relationship_snapshot(
                parent,
                child,
                previous_relationship,
            );
            false
        }
    }

    async fn deliver_v1_waking_terminal(
        &self,
        parent_thread_id: ThreadId,
        child_reference: &str,
        terminal: &WatcherTerminalPresentation,
        response_item_id: ResponseItemId,
        submission_permit: tokio::sync::OwnedSemaphorePermit,
    ) -> bool {
        let message = format_subagent_notification_message(
            child_reference,
            terminal.presentation.child().thread_id,
            &terminal.status,
        );
        let Some(child_agent_path) =
            self.observation_agent_path(terminal.presentation.child().thread_id)
        else {
            return false;
        };
        let Some(parent_agent_path) = self.observation_agent_path(parent_thread_id) else {
            return false;
        };
        let mut communication = InterAgentCommunication::new(
            child_agent_path,
            parent_agent_path,
            Vec::new(),
            message,
            /*trigger_turn*/ true,
        );
        let committed_observations = self.response_observation_committed_snapshots(
            terminal.presentation.parent(),
            terminal.presentation.child(),
            &terminal.turn_id,
            &response_item_id,
            ResponseObservationDeliveryKind::Final,
        );
        communication.id = Some(response_item_id);
        let context = AgentCommunicationContext::new(
            AgentCommunicationKind::Result,
            terminal.presentation.child().thread_id,
        );
        let accepted_completion_delivery =
            terminal.presentation.take_accepted_completion_delivery();
        let admission = if accepted_completion_delivery.is_some() {
            CompletionSubmissionAdmission::Accepted
        } else {
            CompletionSubmissionAdmission::Ordinary
        };
        let Ok((_submission_id, parent_thread)) = self
            .send_inter_agent_completion_communication_durably(
                parent_thread_id,
                communication,
                context,
                &terminal.presentation,
                admission,
                committed_observations,
                submission_permit,
            )
            .await
        else {
            if let Some(accepted_completion_delivery) = accepted_completion_delivery {
                terminal
                    .presentation
                    .restore_accepted_completion_delivery(accepted_completion_delivery);
            }
            return false;
        };
        match accepted_completion_delivery {
            Some(completion_delivery) => {
                parent_thread
                    .emit_accepted_sub_agent_completion_without_turn(
                        child_reference,
                        &terminal.status,
                        SubAgentCompletionModelVisibility::Visible,
                        completion_delivery,
                    )
                    .await;
            }
            None => {
                parent_thread
                    .emit_sub_agent_completion_without_turn(
                        child_reference,
                        &terminal.status,
                        SubAgentCompletionModelVisibility::Visible,
                    )
                    .await;
            }
        }
        true
    }

    fn observation_agent_path(&self, thread_id: ThreadId) -> Option<AgentPath> {
        if let Some(agent_path) = self
            .get_agent_metadata(thread_id)
            .and_then(|metadata| metadata.agent_path)
        {
            return Some(agent_path);
        }
        let thread_name = format!("thread_{}", thread_id.to_string().replace('-', "_"));
        match AgentPath::root().join(&thread_name) {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!(%thread_id, "failed to build synthetic agent path: {err}");
                None
            }
        }
    }

    pub(super) async fn persist_response_observation_snapshot(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> bool {
        let observations = self.response_observation_snapshots(parent, child);
        self.persist_response_observation_updates(parent, observations)
            .await
    }

    pub(super) async fn persist_response_observation_updates(
        &self,
        parent: SessionPresentationId,
        observations: Vec<codex_protocol::protocol::AgentResponseObservation>,
    ) -> bool {
        if observations.is_empty() {
            return true;
        }
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(parent_thread) = state.get_thread(parent.thread_id).await else {
            return false;
        };
        if parent_thread.session.presentation_id() != parent {
            return false;
        }
        parent_thread
            .session
            .persist_agent_response_observations(&observations)
            .await
    }

    pub(super) async fn rollback_response_observation_relationship_locked(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        previous_relationship: Option<super::presentation::ResponseObserverRelationship>,
        target_turn_id: Option<String>,
        failed_operation: &str,
    ) -> CodexResult<()> {
        self.restore_response_observation_relationship_snapshot(
            parent,
            child,
            previous_relationship,
        );
        if self
            .persist_response_observation_updates(
                parent,
                self.response_observation_audit_snapshots(parent, child, target_turn_id),
            )
            .await
        {
            return Ok(());
        }

        if let Ok(state) = self.upgrade()
            && let Ok(parent_thread) = state.get_thread(parent.thread_id).await
            && parent_thread.session.presentation_id() == parent
        {
            // Neither failed durability barrier is authoritative. Quarantine the live parent so a
            // cold reload, rather than the optimistically restored in-memory relationship, decides
            // which append actually survived.
            parent_thread
                .session
                .submission_admission
                .rollback_requires_reload();
        }
        Err(CodexErr::Fatal(format!(
            "{failed_operation}; its compensating response-observation update also failed to persist, so the durable subscription outcome is unknown; refresh the thread before continuing"
        )))
    }
}
