use super::user_dispatch::ObservedInputResult;
use super::user_dispatch::ObservedUserInputRequest;
use super::user_dispatch::UserDispatch;
use super::user_dispatch::UserObservation;
use super::*;
use crate::agent::turn_queue::QueuedAgentTurn;
use crate::agent::turn_queue::QueuedAgentTurnView;
use crate::agent::turn_queue::QueuedTargetMessageWake;
use codex_protocol::turn_input::NotSubmittedReason;

pub(crate) struct QueuedResponseObservationSubmission {
    pub(crate) queue_id: Uuid,
}
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;

pub(crate) struct QueuedInputObservationParams {
    pub(crate) agent_id: ThreadId,
    pub(crate) input: AgentControlInput,
    pub(crate) start_options: TurnStartOptions,
    pub(crate) observer: SessionPresentationId,
    pub(crate) response_observation: ResponseObservationPolicy,
    pub(crate) task_preview: Option<String>,
    pub(crate) authored_selector: Option<String>,
}

impl LocalAgentControl {
    pub(crate) async fn queue_input_observing_response(
        &self,
        params: QueuedInputObservationParams,
    ) -> CodexResult<QueuedResponseObservationSubmission> {
        let QueuedInputObservationParams {
            agent_id,
            input,
            start_options,
            observer,
            response_observation,
            task_preview,
            authored_selector,
        } = params;
        if !response_observation.queue_input() {
            return Err(CodexErr::InvalidRequest(
                "queued input requires q response handling".to_string(),
            ));
        }
        let state = self.upgrade()?;
        let lifecycle_lock = state.agent_lifecycle_lock(agent_id);
        let _lifecycle_guard = lifecycle_lock.lock_owned().await;
        self.require_current_agent_ownership(agent_id).await?;
        let thread = state.get_thread(agent_id).await?;
        self.ensure_target_message_route_allowed(&thread, observer, response_observation)?;
        let observer_thread = state.get_thread(observer.thread_id).await?;
        if observer_thread.session.presentation_id() != observer {
            return Err(CodexErr::ThreadNotFound(observer.thread_id));
        }
        let _source_guard = state
            .agent_turn_queue
            .acquire_source_admission(observer.thread_id)
            .await;
        observer_thread.session.submission_admission.check_ready()?;
        thread.session.submission_admission.check_ready()?;
        let queue_id = uuid::Uuid::now_v7();
        Self::enqueue_agent_turn(
            &state,
            QueuedAgentTurn {
                id: queue_id,
                control: self.clone(),
                source: observer,
                target_thread_id: agent_id,
                input,
                start_options,
                response_observation,
                task_preview,
                authored_selector,
                target_message_wake: None,
            },
        );
        Ok(QueuedResponseObservationSubmission { queue_id })
    }

    pub(crate) async fn queue_scoped_agent_input_observing_response(
        &self,
        origin: AgentModelInputOrigin,
        batch_id: Option<&str>,
        receiver_thread_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<QueuedResponseObservationSubmission> {
        let AgentModelInputOrigin {
            sender,
            sender_turn_id,
        } = origin;
        let sender_turn_id = sender_turn_id.as_str();
        if !response_observation.queue_input() {
            return Err(CodexErr::InvalidRequest(
                "queued input requires q response handling".to_string(),
            ));
        }
        let state = self.upgrade()?;
        let receiver_lifecycle_lock = state.agent_lifecycle_lock(receiver_thread_id);
        let _receiver_lifecycle_guard = receiver_lifecycle_lock.lock_owned().await;
        let receiver_thread = state.get_thread(receiver_thread_id).await?;
        let receiver = receiver_thread.session.presentation_id();
        let receiver_control = receiver_thread.session.services.agent_control.clone();
        receiver_control
            .require_current_agent_ownership(receiver_thread_id)
            .await?;
        receiver_control
            .ensure_scoped_reply_route_supported(&receiver_thread, response_observation)?;
        let sender_thread = state.get_thread(sender.thread_id).await?;
        let _source_guard = state
            .agent_turn_queue
            .acquire_source_admission(sender.thread_id)
            .await;
        sender_thread.session.submission_admission.check_ready()?;
        receiver_thread.session.submission_admission.check_ready()?;
        receiver_control
            .refresh_subtree_messaging(receiver_thread_id)
            .await?;
        let attributed_input = receiver_control
            .attribute_model_input(sender, receiver_thread_id, sender_turn_id, batch_id, input)
            .await?;
        let admission = receiver_control
            .acquire_target_message_admission_after_binding(
                &receiver_thread,
                receiver,
                &sender_thread,
                sender,
                sender_turn_id,
                TargetMessageAdmissionMode::SeparateTurn,
            )
            .await?;
        let TargetMessageAdmission::Wake(reservation_id) = admission else {
            return Err(CodexErr::InvalidRequest(
                "agent message route already reserved or consumed its idle wake".to_string(),
            ));
        };
        let queue_id = uuid::Uuid::now_v7();
        Self::enqueue_agent_turn(
            &state,
            QueuedAgentTurn {
                id: queue_id,
                control: receiver_control,
                source: sender,
                target_thread_id: receiver_thread_id,
                input: attributed_input,
                start_options,
                response_observation,
                task_preview: None,
                authored_selector: None,
                target_message_wake: Some(QueuedTargetMessageWake {
                    observer: receiver,
                    target: sender,
                    target_turn_id: sender_turn_id.to_string(),
                    reservation_id,
                }),
            },
        );
        Ok(QueuedResponseObservationSubmission { queue_id })
    }

    pub(in crate::agent::control) fn enqueue_agent_turn(
        state: &Arc<ThreadManagerState>,
        turn: QueuedAgentTurn,
    ) {
        let target_thread_id = turn.target_thread_id;
        if state.agent_turn_queue.enqueue(turn) {
            Self::spawn_agent_turn_queue_worker(Arc::clone(state), target_thread_id);
        }
    }

    fn spawn_agent_turn_queue_worker(state: Arc<ThreadManagerState>, target_thread_id: ThreadId) {
        tokio::spawn(async move {
            loop {
                let changed = state.agent_turn_queue.changed();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if state
                    .agent_turn_queue
                    .stop_worker_if_empty(target_thread_id)
                {
                    return;
                }
                let Ok(target) = state.get_thread(target_thread_id).await else {
                    state
                        .agent_turn_queue
                        .cancel_for_threads([target_thread_id]);
                    continue;
                };
                let (snapshot, mut responses) = target.session.subscribe_agent_responses();
                if snapshot.active_turn_id.is_some() {
                    tokio::select! {
                        _ = changed => {}
                        _ = responses.recv() => {}
                        () = target.io.session_loop_termination.clone() => {}
                    }
                    continue;
                }
                let lifecycle = state.acquire_live_agent_lifecycle(target_thread_id).await;
                let Ok(lifecycle) = lifecycle else {
                    state
                        .agent_turn_queue
                        .cancel_for_threads([target_thread_id]);
                    continue;
                };
                let Some(entry) = state.agent_turn_queue.take_front(target_thread_id) else {
                    continue;
                };
                if !state
                    .get_thread(target_thread_id)
                    .await
                    .is_ok_and(|current| Arc::ptr_eq(&current, &target))
                {
                    entry.rollback_target_message_wake();
                    state
                        .agent_turn_queue
                        .finish_front(target_thread_id, entry.id);
                    continue;
                }
                let source_guard = state
                    .agent_turn_queue
                    .acquire_source_admission(entry.source.thread_id)
                    .await;
                let source_is_current =
                    state
                        .get_thread(entry.source.thread_id)
                        .await
                        .is_ok_and(|source| {
                            source.session.presentation_id() == entry.source
                                && source.session.submission_admission.check_ready().is_ok()
                        });
                if !source_is_current
                    || entry
                        .control
                        .require_current_agent_ownership(target_thread_id)
                        .await
                        .is_err()
                {
                    entry.rollback_target_message_wake();
                    state
                        .agent_turn_queue
                        .finish_front(target_thread_id, entry.id);
                    continue;
                }
                // Capacity is checked before the irreversible claim. This is a positive
                // no-submission result, unlike any failure after handing input to SessionIo.
                if let Err(error) = entry
                    .control
                    .ensure_execution_capacity_for_turn_start(&target)
                    .await
                {
                    if matches!(error.details(), CodexErrorDetails::AgentLimitReached { .. }) {
                        state
                            .agent_turn_queue
                            .restore_front_after_not_submitted(target_thread_id, entry.id);
                        drop(source_guard);
                        drop(lifecycle);
                        let cancelled = state.agent_turn_queue.changed();
                        tokio::pin!(cancelled);
                        cancelled.as_mut().enable();
                        if !state.agent_turn_queue.has_pending(target_thread_id) {
                            continue;
                        }
                        tokio::select! {
                            () = entry.control.wait_for_execution_capacity() => {}
                            _ = cancelled => {}
                            _ = responses.recv() => {}
                            () = target.io.session_loop_termination.clone() => {}
                        }
                    } else {
                        entry.rollback_target_message_wake();
                        state
                            .agent_turn_queue
                            .finish_front(target_thread_id, entry.id);
                    }
                    continue;
                }
                if !state
                    .agent_turn_queue
                    .begin_admission(target_thread_id, entry.id)
                {
                    continue;
                }
                let result = entry
                    .control
                    .dispatch_user_input_locked(
                        &target,
                        ObservedUserInputRequest {
                            target_message_wake: entry.target_message_wake.clone(),
                            input: entry.input.clone(),
                            start_options: entry.start_options.clone(),
                            observer: entry.source,
                            observation: UserObservation::Install(entry.response_observation),
                            dispatch: UserDispatch::Queued(
                                entry.response_observation.admitted_queue_turn_metadata(
                                    entry.id.to_string(),
                                    entry.source.thread_id,
                                ),
                            ),
                            task_preview: entry.task_preview.clone(),
                        },
                    )
                    .await;
                match result {
                    Ok(ObservedInputResult::PermissionRejected(reason)) => {
                        entry.rollback_target_message_wake();
                        state
                            .agent_turn_queue
                            .finish_front(target_thread_id, entry.id);
                        drop(source_guard);
                        drop(lifecycle);
                        publish_queued_turn_warning(&state, &entry, "permission", reason).await;
                    }
                    Ok(ObservedInputResult::NotSubmitted(NotSubmittedReason::NotIdle)) => {
                        state
                            .agent_turn_queue
                            .restore_front_after_not_submitted(target_thread_id, entry.id);
                        drop(source_guard);
                        drop(lifecycle);
                        let transition = target.session.active_turn_transition.notified();
                        tokio::pin!(transition);
                        transition.as_mut().enable();
                        if target.session.active_turn.lock().await.is_some() {
                            tokio::select! {
                                _ = transition => {}
                                _ = responses.recv() => {}
                                () = target.io.session_loop_termination.clone() => {}
                            }
                        }
                        continue;
                    }
                    Ok(ObservedInputResult::NotSubmitted(reason)) => {
                        entry.rollback_target_message_wake();
                        state
                            .agent_turn_queue
                            .finish_front(target_thread_id, entry.id);
                        drop(source_guard);
                        drop(lifecycle);
                        publish_queued_turn_warning(
                            &state,
                            &entry,
                            "admission",
                            format!("Queued input was not submitted: {reason:?}"),
                        )
                        .await;
                    }
                    Ok(ObservedInputResult::Submitted {
                        mut submission,
                        input_persisted,
                    }) => {
                        if let Some(reservation) = &entry.target_message_wake {
                            match submission.target_turn_id.as_deref() {
                                Some(turn_id) => {
                                    entry.control.commit_target_message_wake(
                                        reservation.observer,
                                        reservation.target,
                                        &reservation.target_turn_id,
                                        reservation.reservation_id,
                                        turn_id,
                                    );
                                    let (snapshot, _) = target.session.subscribe_agent_responses();
                                    if snapshot.active_turn_id.as_deref() != Some(turn_id)
                                        && snapshot
                                            .last_terminal
                                            .as_ref()
                                            .is_some_and(|(completed, _)| completed == turn_id)
                                    {
                                        entry.control.finish_target_message_wake(
                                            reservation.observer,
                                            turn_id,
                                        );
                                    }
                                    if let Err(error) = entry
                                        .control
                                        .persist_response_observation_snapshot(
                                            reservation.observer,
                                            reservation.target,
                                        )
                                        .await
                                    {
                                        entry.control.abandon_response_observer(
                                            reservation.observer,
                                            reservation.target,
                                            &error.to_string(),
                                        );
                                        submission.post_admission_warning = Some(format!(
                                            "input was admitted but one-wake publication failed: {error}; do not resend"
                                        ));
                                    }
                                }
                                None => entry.control.abandon_response_observer(
                                    reservation.observer,
                                    reservation.target,
                                    "queued reverse-message admission is unknown",
                                ),
                            }
                        }
                        state
                            .agent_turn_queue
                            .finish_front(target_thread_id, entry.id);
                        drop(source_guard);
                        drop(lifecycle);
                        submission.await_input_persistence(input_persisted).await;
                        if let Some(warning) = submission.post_admission_warning {
                            publish_queued_turn_warning(&state, &entry, "response", warning).await;
                        }
                    }
                    Err(error) => {
                        // Do not restore the entry based on error text or an absent receipt.
                        if let Some(reservation) = &entry.target_message_wake {
                            entry.control.abandon_response_observer(
                                reservation.observer,
                                reservation.target,
                                &error.to_string(),
                            );
                        }
                        state
                            .agent_turn_queue
                            .finish_front(target_thread_id, entry.id);
                        drop(source_guard);
                        drop(lifecycle);
                        publish_queued_turn_warning(
                            &state,
                            &entry,
                            "admission",
                            format!("Queued submission stopped: {error}; reconcile before retry"),
                        )
                        .await;
                    }
                }
            }
        });
    }

    pub(crate) fn list_queued_agent_turns(&self) -> Vec<QueuedAgentTurnView> {
        self.upgrade().map_or_else(
            |_| Vec::new(),
            |state| state.agent_turn_queue.list_for_root(self.session_id()),
        )
    }

    pub(crate) fn cancel_queued_agent_turn(&self, id: Uuid) -> bool {
        self.upgrade()
            .is_ok_and(|state| state.agent_turn_queue.cancel(self.session_id(), id))
    }
}

async fn publish_queued_turn_warning(
    state: &ThreadManagerState,
    entry: &QueuedAgentTurn,
    warning_kind: &str,
    message: String,
) {
    let Ok(source) = state.get_thread(entry.source.thread_id).await else {
        return;
    };
    if source.session.presentation_id() == entry.source {
        source
            .session
            .send_event_raw(Event {
                id: format!("agent-queue-{}-{warning_kind}", entry.id),
                msg: EventMsg::Warning(WarningEvent { message }),
            })
            .await;
    }
}
