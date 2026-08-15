use super::*;
use crate::agent::turn_queue::QueuedAgentTurn;
use crate::agent::turn_queue::QueuedAgentTurnView;
use crate::agent::turn_queue::QueuedTargetMessageWake;
use crate::session::QUEUED_INPUT_ACTIVE_ERROR_PREFIX;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;

pub(crate) struct QueuedInputObservationParams {
    pub(crate) agent_id: ThreadId,
    pub(crate) input: Vec<UserInput>,
    pub(crate) start_options: TurnStartOptions,
    pub(crate) observer: SessionPresentationId,
    pub(crate) response_observation: ResponseObservationPolicy,
    pub(crate) task_preview: Option<String>,
    pub(crate) authored_selector: Option<String>,
}

impl AgentControl {
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
        let thread = state.get_thread_including_pending(agent_id).await?;
        self.ensure_scoped_reply_route_supported(&thread, response_observation)?;
        let observer_thread = state
            .get_thread_including_pending(observer.thread_id)
            .await?;
        if observer_thread.session.presentation_id() != observer {
            return Err(CodexErr::ThreadNotFound(observer.thread_id));
        }
        let queue_id = uuid::Uuid::now_v7();
        Self::enqueue_agent_turn(
            &state,
            QueuedAgentTurn {
                id: queue_id,
                control: self.clone(),
                source: observer,
                target_thread_id: agent_id,
                input: AgentControlInput::User(input),
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
        sender: SessionPresentationId,
        sender_turn_id: &str,
        receiver_thread_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<QueuedResponseObservationSubmission> {
        if !response_observation.queue_input() {
            return Err(CodexErr::InvalidRequest(
                "queued input requires q response handling".to_string(),
            ));
        }
        let state = self.upgrade()?;
        let receiver_lifecycle_lock = state.agent_lifecycle_lock(receiver_thread_id);
        let _receiver_lifecycle_guard = receiver_lifecycle_lock.lock_owned().await;
        let receiver_thread = state
            .get_thread_including_pending(receiver_thread_id)
            .await?;
        let receiver = receiver_thread.session.presentation_id();
        let receiver_control = receiver_thread.session.services.agent_control.clone();
        receiver_control
            .require_current_agent_ownership(receiver_thread_id)
            .await?;
        receiver_control
            .ensure_scoped_reply_route_supported(&receiver_thread, response_observation)?;
        let sender_thread = state.get_thread_including_pending(sender.thread_id).await?;
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
        let sender_identity = receiver_control
            .model_visible_agent_identity(&receiver_thread, sender.thread_id)
            .await?;
        let attributed_input =
            super::scoped_messages::attributed_agent_input(sender_identity, sender_turn_id, input);
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

    fn enqueue_agent_turn(state: &Arc<ThreadManagerState>, turn: QueuedAgentTurn) {
        let target_thread_id = turn.target_thread_id;
        if state.agent_turn_queue.enqueue(turn) {
            Self::spawn_agent_turn_queue_worker(Arc::clone(state), target_thread_id);
        }
    }

    fn spawn_agent_turn_queue_worker(state: Arc<ThreadManagerState>, target_thread_id: ThreadId) {
        tokio::spawn(async move {
            loop {
                if state
                    .agent_turn_queue
                    .stop_worker_if_empty(target_thread_id)
                {
                    return;
                }
                let Ok(target_thread) = state.get_thread_including_pending(target_thread_id).await
                else {
                    state
                        .agent_turn_queue
                        .cancel_for_threads([target_thread_id]);
                    let _ = state
                        .agent_turn_queue
                        .stop_worker_if_empty(target_thread_id);
                    return;
                };
                let (snapshot, mut response_subscription) =
                    target_thread.session.subscribe_agent_responses();
                if snapshot.active_turn_id.is_some() {
                    tokio::select! {
                        _ = state.agent_turn_queue.wait_changed() => {}
                        _ = response_subscription.recv() => {}
                        () = target_thread.io.session_loop_termination.clone() => {}
                    }
                    continue;
                }

                let lifecycle_lock = state.agent_lifecycle_lock(target_thread_id);
                let lifecycle_guard = lifecycle_lock.lock_owned().await;
                let Some(entry) = state.agent_turn_queue.take_front(target_thread_id) else {
                    drop(lifecycle_guard);
                    continue;
                };
                let current_thread =
                    match state.get_thread_including_pending(target_thread_id).await {
                        Ok(current_thread) if Arc::ptr_eq(&current_thread, &target_thread) => {
                            current_thread
                        }
                        Ok(_) | Err(_) => {
                            state
                                .agent_turn_queue
                                .restore_front(target_thread_id, entry.id);
                            drop(lifecycle_guard);
                            continue;
                        }
                    };
                let _source_admission_guard = state
                    .agent_turn_queue
                    .acquire_source_admission(entry.source.thread_id)
                    .await;
                let source_is_current = state
                    .get_thread_including_pending(entry.source.thread_id)
                    .await
                    .is_ok_and(|source_thread| {
                        source_thread.session.presentation_id() == entry.source
                    });
                if !source_is_current {
                    entry.rollback_target_message_wake();
                    state
                        .agent_turn_queue
                        .finish_front(target_thread_id, entry.id);
                    drop(lifecycle_guard);
                    continue;
                }
                let task_context = entry.task_preview.clone().map_or(
                    ObservedInputTaskContext::None,
                    ObservedInputTaskContext::UserAuthored,
                );
                if !state
                    .agent_turn_queue
                    .begin_admission(target_thread_id, entry.id)
                {
                    drop(lifecycle_guard);
                    continue;
                }
                // The explicit queued admission mode prevents this input from being enqueued
                // again. Keep `q` in the bound response policy so the completed reply crosses the
                // source's next-turn boundary too.
                let response_observation = entry.response_observation;
                let agent_queue_turn = response_observation
                    .admitted_queue_turn_metadata(entry.id.to_string(), entry.source.thread_id);
                let result = entry
                    .control
                    .send_input_observing_response_to_retained_thread_locked(
                        target_thread_id,
                        &state,
                        &current_thread,
                        ObservedInputAdmission {
                            input: entry.input.clone(),
                            start_options: entry.start_options.clone(),
                            observer: entry.source,
                            response_observation,
                            admission_mode: InputTurnAdmissionMode::Queued(agent_queue_turn),
                            task_context,
                        },
                    )
                    .await;
                match result {
                    Ok(submission) => {
                        if let Some(warning) = submission.post_admission_warning.as_deref() {
                            publish_queued_turn_warning(
                                &state,
                                &entry,
                                "response",
                                format!(
                                    "Queued input was admitted to {}, but response handling \
                                     degraded: {warning}",
                                    entry.target_thread_id
                                ),
                            )
                            .await;
                        }
                        if let Some(reservation) = entry.target_message_wake.as_ref()
                            && entry.control.commit_target_message_wake(
                                reservation.observer,
                                reservation.target,
                                &reservation.target_turn_id,
                                reservation.reservation_id,
                                &submission.target_turn_id,
                            )
                        {
                            if !entry
                                .control
                                .persist_response_observation_snapshot(
                                    reservation.observer,
                                    reservation.target,
                                )
                                .await
                            {
                                let warning = format!(
                                    "Queued agent message started for {}, but its one-wake state \
                                     could not be persisted",
                                    entry.target_thread_id
                                );
                                tracing::warn!(
                                    queue_id = %entry.id,
                                    observer_thread_id = %reservation.observer.thread_id,
                                    target_thread_id = %reservation.target.thread_id,
                                    wake_turn_id = submission.target_turn_id,
                                    warning
                                );
                                publish_queued_turn_warning(&state, &entry, "wake", warning).await;
                            }
                            let (snapshot, subscription) =
                                current_thread.session.subscribe_agent_responses();
                            drop(subscription);
                            if snapshot.active_turn_id.as_deref()
                                != Some(&submission.target_turn_id)
                                && snapshot.last_terminal.as_ref().is_some_and(|(turn_id, _)| {
                                    turn_id == &submission.target_turn_id
                                })
                            {
                                entry.control.finish_target_message_wake(
                                    reservation.observer,
                                    &submission.target_turn_id,
                                );
                            }
                        }
                        state
                            .agent_turn_queue
                            .finish_front(target_thread_id, entry.id);
                    }
                    Err(err) if queued_admission_target_became_active(&err) => {
                        let active_turn_transition =
                            target_thread.session.active_turn_transition.notified();
                        tokio::pin!(active_turn_transition);
                        active_turn_transition.as_mut().enable();
                        state
                            .agent_turn_queue
                            .restore_front(target_thread_id, entry.id);
                        drop(lifecycle_guard);
                        if target_thread.session.active_turn.lock().await.is_some() {
                            tokio::select! {
                                () = active_turn_transition.as_mut() => {}
                                _ = state.agent_turn_queue.wait_changed() => {}
                                _ = response_subscription.recv() => {}
                                () = target_thread.io.session_loop_termination.clone() => {}
                            }
                        }
                        continue;
                    }
                    Err(err)
                        if matches!(err.details(), CodexErrorDetails::AgentLimitReached { .. }) =>
                    {
                        state
                            .agent_turn_queue
                            .restore_front(target_thread_id, entry.id);
                        drop(lifecycle_guard);
                        tokio::select! {
                            () = entry.control.wait_for_execution_capacity() => {}
                            _ = state.agent_turn_queue.wait_changed() => {}
                            _ = response_subscription.recv() => {}
                            () = target_thread.io.session_loop_termination.clone() => {}
                        }
                        continue;
                    }
                    Err(err) => {
                        let warning = format!(
                            "Queued input for {} was discarded before admission: {err}",
                            entry.target_thread_id
                        );
                        tracing::warn!(
                            queue_id = %entry.id,
                            source_thread_id = %entry.source.thread_id,
                            %target_thread_id,
                            %err,
                            "discarding agent turn that could not be admitted"
                        );
                        publish_queued_turn_warning(&state, &entry, "admission", warning).await;
                        entry.rollback_target_message_wake();
                        state
                            .agent_turn_queue
                            .finish_front(target_thread_id, entry.id);
                    }
                }
                drop(lifecycle_guard);
            }
        });
    }

    pub(crate) fn list_queued_agent_turns(&self) -> Vec<QueuedAgentTurnView> {
        let Ok(state) = self.upgrade() else {
            return Vec::new();
        };
        state.agent_turn_queue.list_for_root(self.session_id())
    }

    pub(crate) fn cancel_queued_agent_turn(&self, id: uuid::Uuid) -> bool {
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
    let Ok(source_thread) = state
        .get_thread_including_pending(entry.source.thread_id)
        .await
    else {
        return;
    };
    if source_thread.session.presentation_id() != entry.source {
        return;
    }
    source_thread
        .session
        .send_event_raw(Event {
            id: format!("agent-queue-{}-{warning_kind}", entry.id),
            msg: EventMsg::Warning(WarningEvent { message }),
        })
        .await;
}

fn queued_admission_target_became_active(err: &CodexErr) -> bool {
    matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message.starts_with(QUEUED_INPUT_ACTIVE_ERROR_PREFIX)
    )
}
