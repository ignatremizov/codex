use super::presentation::ResponseObservationDeliveryKind;
use super::presentation::ResponseObservationEventMatch;
use super::presentation::WatcherResponseEventStream;
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
                let Some(lifecycle_guard) = self
                    .acquire_current_agent_lifecycle(child.thread_id, child_lifecycle_generation)
                    .await
                else {
                    return WatcherTerminalPoll::Closed;
                };
                if !self
                    .deliver_v1_commentary_observation(
                        parent,
                        child,
                        &turn_id,
                        &delivery,
                        lifecycle_guard,
                    )
                    .await
                {
                    return WatcherTerminalPoll::Retry;
                }
            }
            if let Some(terminal) = self.take_response_ordered_watcher_terminal_presentation(
                parent,
                child,
                WatcherResponseEventStream::Open,
            ) {
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
                    // A closed response stream has no earlier commentary left to process.
                    let Some(terminal) = self.take_response_ordered_watcher_terminal_presentation(
                        parent,
                        child,
                        WatcherResponseEventStream::Closed,
                    ) else {
                        return WatcherTerminalPoll::Closed;
                    };
                    self.mark_response_observer_terminal_processed(
                        parent,
                        child,
                        &terminal.turn_id,
                    );
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
                        let Some(lifecycle_guard) = self
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
                                &turn_id,
                                &item_id,
                                &text,
                                sequence,
                                lifecycle_guard,
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
                        self.mark_response_observer_terminal_processed(parent, child, &turn_id);
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
                        self.mark_response_observer_terminal_processed(parent, child, &turn_id);
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
        turn_id: &str,
        item_id: &str,
        text: &str,
        sequence: u64,
        target_lifecycle_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> bool {
        let transaction_permit = self.acquire_response_observation_transaction(parent).await;
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
        drop(transaction_permit);
        if self.route_response_observer_commentary(parent, child, turn_id)
            == CommentaryDeliveryRoute::Wait
        {
            return true;
        }
        let Ok(submission_permit) = self
            .acquire_mailbox_submission_permit(parent.thread_id)
            .await
        else {
            return false;
        };
        if self.route_response_observer_commentary(parent, child, turn_id)
            != CommentaryDeliveryRoute::Mailbox
        {
            return true;
        }
        if self
            .response_observation_commentary_delivery(parent, child, turn_id)
            .as_ref()
            != Some(&delivery)
        {
            return true;
        }
        self.deliver_v1_commentary_observation_after_claim(
            parent,
            child,
            turn_id,
            &delivery,
            DurableResponseDelivery {
                commit: ResponseObservationDeliveryCommit {
                    parent,
                    child,
                    turn_id: turn_id.to_string(),
                    response_item_id: delivery.response_item_id.clone(),
                    kind: ResponseObservationDeliveryKind::Commentary,
                },
                submission_permit,
                target_lifecycle_guard,
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn deliver_recovered_v1_commentary(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        item_id: &str,
        text: &str,
        sequence: u64,
        prior_item_ids: &HashSet<String>,
        target_lifecycle_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> bool {
        let transaction_permit = self.acquire_response_observation_transaction(parent).await;
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
        drop(transaction_permit);
        if self.route_response_observer_commentary(parent, child, turn_id)
            == CommentaryDeliveryRoute::Wait
        {
            return true;
        }
        let Ok(submission_permit) = self
            .acquire_mailbox_submission_permit(parent.thread_id)
            .await
        else {
            return false;
        };
        if self.route_response_observer_commentary(parent, child, turn_id)
            != CommentaryDeliveryRoute::Mailbox
        {
            return true;
        }
        if self
            .response_observation_commentary_delivery(parent, child, turn_id)
            .as_ref()
            != Some(&delivery)
        {
            return true;
        }
        self.deliver_v1_commentary_observation_after_claim(
            parent,
            child,
            turn_id,
            &delivery,
            DurableResponseDelivery {
                commit: ResponseObservationDeliveryCommit {
                    parent,
                    child,
                    turn_id: turn_id.to_string(),
                    response_item_id: delivery.response_item_id.clone(),
                    kind: ResponseObservationDeliveryKind::Commentary,
                },
                submission_permit,
                target_lifecycle_guard,
            },
        )
        .await
    }

    pub(super) async fn deliver_v1_commentary_observation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        delivery: &codex_protocol::protocol::AgentResponseCommentaryDelivery,
        target_lifecycle_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> bool {
        if self.route_response_observer_commentary(parent, child, turn_id)
            == CommentaryDeliveryRoute::Wait
        {
            return true;
        }
        let Ok(submission_permit) = self
            .acquire_mailbox_submission_permit(parent.thread_id)
            .await
        else {
            return false;
        };
        if self.route_response_observer_commentary(parent, child, turn_id)
            != CommentaryDeliveryRoute::Mailbox
        {
            return true;
        }
        if self
            .response_observation_commentary_delivery(parent, child, turn_id)
            .as_ref()
            != Some(delivery)
        {
            return true;
        }
        let transaction_permit = self.acquire_response_observation_transaction(parent).await;
        if !self
            .persist_response_observation_snapshot(parent, child)
            .await
        {
            return false;
        }
        drop(transaction_permit);
        self.deliver_v1_commentary_observation_after_claim(
            parent,
            child,
            turn_id,
            delivery,
            DurableResponseDelivery {
                commit: ResponseObservationDeliveryCommit {
                    parent,
                    child,
                    turn_id: turn_id.to_string(),
                    response_item_id: delivery.response_item_id.clone(),
                    kind: ResponseObservationDeliveryKind::Commentary,
                },
                submission_permit,
                target_lifecycle_guard,
            },
        )
        .await
    }

    pub(crate) async fn deliver_v1_wait_commentary(
        &self,
        parent: SessionPresentationId,
        turn_context: &Arc<crate::TurnContext>,
        commentary: &WaitCommentaryDelivery,
    ) -> bool {
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(parent_thread) = state.get_thread_including_pending(parent.thread_id).await else {
            return false;
        };
        if parent_thread.session.presentation_id() != parent {
            return false;
        }
        let Ok(agent) = self
            .model_visible_agent_identity(&parent_thread, commentary.child.thread_id)
            .await
        else {
            return false;
        };
        let Some(child_agent_path) = self.observation_agent_path(commentary.child.thread_id) else {
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
                agent,
                &commentary.turn_id,
                &commentary.delivery.source_item_id,
                &commentary.delivery.text,
            ),
            /*trigger_turn*/ true,
        );
        communication.id = Some(commentary.delivery.response_item_id.clone());
        let commit = ResponseObservationDeliveryCommit {
            parent,
            child: commentary.child,
            turn_id: commentary.turn_id.clone(),
            response_item_id: commentary.delivery.response_item_id.clone(),
            kind: ResponseObservationDeliveryKind::Commentary,
        };
        let rollout_suffix = self
            .response_observation_committed_snapshots(
                parent,
                commentary.child,
                &commentary.turn_id,
                &commentary.delivery.response_item_id,
                ResponseObservationDeliveryKind::Commentary,
            )
            .into_iter()
            .map(RolloutItem::AgentResponseObservation)
            .collect();
        if parent_thread
            .session
            .record_wait_commentary(Arc::clone(turn_context), communication, rollout_suffix)
            .await
            .is_err()
        {
            return false;
        }
        self.commit_response_observation_delivery(&commit);
        true
    }

    // The pending claim is durable before this method starts, but the observer transaction is not
    // held: the receipt resolves only when the observer reaches a mailbox-consumption boundary.
    async fn deliver_v1_commentary_observation_after_claim(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        delivery: &codex_protocol::protocol::AgentResponseCommentaryDelivery,
        durable_delivery: DurableResponseDelivery,
    ) -> bool {
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(parent_thread) = state.get_thread_including_pending(parent.thread_id).await else {
            return false;
        };
        if parent_thread.session.presentation_id() != parent {
            return false;
        }
        let Ok(agent) = self
            .model_visible_agent_identity(&parent_thread, child.thread_id)
            .await
        else {
            return false;
        };
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
                agent,
                turn_id,
                &delivery.source_item_id,
                &delivery.text,
            ),
            /*trigger_turn*/ true,
        );
        communication.id = Some(delivery.response_item_id.clone());
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Result, child.thread_id);
        self.send_inter_agent_communication_durably(
            parent,
            communication,
            context,
            /*parent_turn_id*/ None,
            durable_delivery,
        )
        .await
        .is_ok()
    }

    pub(super) async fn deliver_v1_watcher_terminal(
        &self,
        parent_thread_id: ThreadId,
        child_reference: &str,
        terminal: &WatcherTerminalPresentation,
        target_lifecycle_guard: tokio::sync::OwnedMutexGuard<()>,
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
        let mut target_lifecycle_guard = Some(target_lifecycle_guard);
        let transaction_permit = self
            .acquire_response_observation_transaction(terminal.presentation.parent())
            .await;
        let terminal_response_item_id = terminal.presentation.completion_context_response_item_id();
        let (final_response_observation, response_item_id, queue_delivery) = self
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
        let (final_response_observation, queue_delivery) =
            if matches!(
                final_response_observation,
                FinalResponseObservation::Passive | FinalResponseObservation::Wake
            ) && (final_response_observation == FinalResponseObservation::Wake || queue_delivery)
            {
                let Ok(state) = self.upgrade() else {
                    return false;
                };
                let Ok(parent_thread) = state.get_thread_including_pending(parent_thread_id).await
                else {
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
                    (FinalResponseObservation::Passive, false)
                } else {
                    (final_response_observation, queue_delivery)
                }
            } else {
                (final_response_observation, queue_delivery)
            };
        match (final_response_observation, queue_delivery) {
            (FinalResponseObservation::None, _) => return true,
            (FinalResponseObservation::PresentationOnly, _) => {
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
                let Ok(parent_thread) = state.get_thread_including_pending(parent_thread_id).await
                else {
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
            (FinalResponseObservation::Wake, _) | (FinalResponseObservation::Passive, true) => {
                let Some(response_item_id) = response_item_id else {
                    return false;
                };
                let Some(submission_permit) = submission_permit.take() else {
                    return false;
                };
                let Some(target_lifecycle_guard) = target_lifecycle_guard.take() else {
                    return false;
                };
                // Triggered delivery resolves at the observer's next mailbox-consumption
                // boundary. Holding its observation transaction across that wait would block any
                // lifecycle tool the observer invokes before it can reach that boundary.
                drop(transaction_permit);
                return self
                    .deliver_v1_triggered_terminal(
                        parent_thread_id,
                        child_reference,
                        terminal,
                        queue_delivery,
                        DurableResponseDelivery {
                            commit: ResponseObservationDeliveryCommit {
                                parent: terminal.presentation.parent(),
                                child: terminal.presentation.child(),
                                turn_id: terminal.turn_id.clone(),
                                response_item_id,
                                kind: ResponseObservationDeliveryKind::Final,
                            },
                            submission_permit,
                            target_lifecycle_guard,
                        },
                    )
                    .await;
            }
            (FinalResponseObservation::Passive, false) => {}
        }
        let Some(response_item_id) = response_item_id else {
            return false;
        };
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(parent_thread) = state.get_thread_including_pending(parent_thread_id).await else {
            return false;
        };
        if parent_thread.session.presentation_id() != terminal.presentation.parent() {
            return false;
        }
        let Ok(agent) = self
            .model_visible_agent_identity(&parent_thread, terminal.presentation.child().thread_id)
            .await
        else {
            return false;
        };
        let message = format_subagent_notification_message(agent, &terminal.status);
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

    async fn deliver_v1_triggered_terminal(
        &self,
        parent_thread_id: ThreadId,
        child_reference: &str,
        terminal: &WatcherTerminalPresentation,
        queue_delivery: bool,
        durable_delivery: DurableResponseDelivery,
    ) -> bool {
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(parent_thread) = state.get_thread_including_pending(parent_thread_id).await else {
            return false;
        };
        if parent_thread.session.presentation_id() != terminal.presentation.parent() {
            return false;
        }
        let Ok(agent) = self
            .model_visible_agent_identity(&parent_thread, terminal.presentation.child().thread_id)
            .await
        else {
            return false;
        };
        let message = format_subagent_notification_message(agent, &terminal.status);
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
        communication.defer_to_next_turn = queue_delivery;
        communication.id = Some(durable_delivery.commit.response_item_id.clone());
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
                durable_delivery,
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
        let Ok(parent_thread) = state.get_thread_including_pending(parent.thread_id).await else {
            return false;
        };
        if parent_thread.session.presentation_id() != parent {
            return false;
        }
        if parent_thread
            .session
            .thread_config_snapshot()
            .await
            .ephemeral
        {
            // Ephemeral sessions retain observation state only for the live runtime. They have no
            // rollout against which a durable response-observation barrier could be satisfied.
            return true;
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
            && let Ok(parent_thread) = state.get_thread_including_pending(parent.thread_id).await
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

    pub(super) async fn rollback_installed_response_observer_if_current(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        registration_id: uuid::Uuid,
        mut target_turn_ids: Vec<Option<String>>,
    ) -> CodexResult<()> {
        let _transaction = self.acquire_response_observation_transaction(parent).await;
        let removed_bound_wake = match self.revoke_response_observation_if_registration_is_current(
            parent,
            child,
            registration_id,
        ) {
            super::presentation::ConditionalResponseObservationRevocation::Replaced => {
                return Ok(());
            }
            super::presentation::ConditionalResponseObservationRevocation::Missing => false,
            super::presentation::ConditionalResponseObservationRevocation::Revoked {
                removed_bound_wake,
            } => removed_bound_wake,
        };
        target_turn_ids.sort();
        target_turn_ids.dedup();
        let observations = target_turn_ids
            .into_iter()
            .flat_map(|target_turn_id| {
                self.response_observation_audit_snapshots(parent, child, target_turn_id)
            })
            .collect::<Vec<_>>();
        if !self
            .persist_response_observation_updates(parent, observations)
            .await
        {
            if let Ok(state) = self.upgrade()
                && let Ok(parent_thread) =
                    state.get_thread_including_pending(parent.thread_id).await
                && parent_thread.session.presentation_id() == parent
            {
                parent_thread
                    .session
                    .submission_admission
                    .rollback_requires_reload();
            }
            return Err(CodexErr::Fatal(
                "failed to persist cancellation of resumed agent response observation; refresh \
                 the observer thread before continuing"
                    .to_string(),
            ));
        }
        drop(_transaction);
        if removed_bound_wake {
            self.recheck_thread_idle_lifecycle(parent).await;
        }
        Ok(())
    }
}
