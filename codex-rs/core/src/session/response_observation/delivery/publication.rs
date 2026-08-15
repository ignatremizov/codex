//! Single-attempt observed delivery through the current canonical history and event owners.

use super::*;
use crate::session::prepared_history_items::PreparedHistoryItems;
use codex_history::ResponseItemEnvelope;

impl Session {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "accepted delivery orders its canonical append and immutable presentation turn"
    )]
    pub(super) async fn persist_observation_payload(
        self: &Arc<Self>,
        commit: ResponseObservationDeliveryCommit,
        accepted: Arc<AcceptedCompletionDelivery>,
        payload: Payload,
    ) -> CodexResult<()> {
        let session = Arc::clone(self);
        let outcome = DeliveryOutcome {
            session: Arc::clone(&session),
            finished: false,
        };
        tokio::spawn(async move {
            let mut outcome = outcome;
            let result = async {
                if commit.parent != session.presentation_id() {
                    return Err(CodexErr::InvalidRequest(
                        "observed response belongs to another session instance".to_string(),
                    ));
                }
                let (response, presentation, trigger_turn, recording_turn_id, wait_turn) = match payload {
                    Payload::Context { communication, presentation, recording_turn_id } => {
                        let response = communication.to_model_input_item();
                        if response.id() != Some(&commit.response_item_id) {
                            return Err(CodexErr::InvalidRequest(
                                "observed response identity changed".to_string(),
                            ));
                        }
                        (Some(response), presentation.map(|presentation| *presentation), communication.trigger_turn, recording_turn_id, None)
                    }
                    Payload::Presentation(presentation) => (None, Some(*presentation), false, None, None),
                    Payload::WaitCommentary { mut communication, turn_context } => {
                        communication.set_turn_id_if_missing(&turn_context.sub_id);
                        let response = communication.to_model_input_item();
                        if response.id() != Some(&commit.response_item_id)
                            || response.turn_id() != Some(turn_context.sub_id.as_str())
                        {
                            return Err(CodexErr::InvalidRequest("wait commentary identity changed".to_string()));
                        }
                        (Some(response), None, communication.trigger_turn, Some(turn_context.sub_id.clone()), Some(turn_context))
                    }
                };
                let history_turn = session.new_history_only_turn().await;
                loop {
                    // Match ordinary incoming communications: confirmed Code Mode sends that
                    // precede this input must finish before acquiring the publication barrier.
                    let boundary = if response.is_some() {
                        Some(Arc::clone(&session.code_mode_message_tasks.communication_boundary)
                            .acquire_owned().await.map_err(|error| CodexErr::Fatal(error.to_string()))?)
                    } else { None };
                    let (input_order, pending) = {
                        let mut state = session.state.lock().await;
                        if response.is_some() {
                            (Some(state.history.reserve_input_order()), session.pending_code_mode_message_recordings())
                        } else { (None, Vec::new()) }
                    };
                    for mut recording in pending {
                        let _ = recording.changed().await;
                    }
                    let changed = session.active_turn_transition.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    let order = session.submission_admission.admit_completion(&accepted).await?;
                    let control = session.services.local_agent_runtime.control(session.session_id());
                    let transaction = control.acquire_response_observation_transaction(commit.parent).await;
                    let permit = session.acquire_history_publication_barrier().await?;
                    let active = session.active_turn.lock().await;
                    if let Some(wait_turn) = &wait_turn
                        && active.as_ref().and_then(|active| active.task.as_ref())
                            .is_none_or(|task| task.turn_context.sub_id != wait_turn.sub_id)
                    {
                        return Err(CodexErr::InvalidRequest("wait commentary turn is no longer active".to_string()));
                    }
                    let finishing_recording_turn = if active.as_ref().is_some_and(|active| {
                        active.task.is_none() && active.terminal_pending
                    }) {
                        let state = session.state.lock().await;
                        recording_turn_id.as_ref().filter(|turn_id| {
                            state.last_started_turn_id.as_ref() == Some(*turn_id)
                        }).cloned()
                    } else { None };
                    if active.as_ref().is_some_and(|active| active.task.is_none())
                        && finishing_recording_turn.is_none()
                    {
                        // Unrelated background delivery waits for a taskless reservation.
                        // The finishing input recorder must not wait for its own finalization.
                        drop(active);
                        drop(permit);
                        drop(transaction);
                        drop(order);
                        drop(boundary);
                        changed.await;
                        continue;
                    }
                    let active_context = active.as_ref().and_then(|active| active.task.as_ref())
                        .map(|task| Arc::clone(&task.turn_context));
                    if let (Some(recording), Some(active)) = (&recording_turn_id, &active_context)
                        && recording != &active.sub_id
                    {
                        return Err(CodexErr::InvalidRequest(
                            "observed input recorder belongs to another active turn".to_string(),
                        ));
                    }
                    let turn = wait_turn.as_ref().or(active_context.as_ref()).unwrap_or(&history_turn);
                    let receiving_turn_id = active_context.as_ref().map(|turn| turn.sub_id.clone())
                        .or(finishing_recording_turn).or_else(|| recording_turn_id.clone());
                    let turn_id = receiving_turn_id.clone().unwrap_or_else(|| {
                        presentation.as_ref().map(|presentation| presentation.history_only_turn_id.clone())
                            .unwrap_or_else(|| turn.sub_id.clone())
                    });
                    let (mut records, mut events) = match presentation {
                        Some(presentation) => presentation::records(
                            session.thread_id,
                            receiving_turn_id.as_deref(),
                            presentation,
                        ),
                        None => (Vec::new(), Vec::new()),
                    };
                    let mut mcp_revision = None;
                    let prepared = if let Some(mut response) = response {
                        // This is the independently authenticated delivery path, not ordinary
                        // input preparation, which deliberately strips reserved completion IDs.
                        Self::stamp_response_item_for_history(&mut response, &turn_id);
                        let mut envelope = ResponseItemEnvelope::new(response);
                        envelope.metadata.get_or_insert_default().user_input_order = input_order;
                        if let Some((attribution, revision)) = session.services.executed_tool_calls
                            .mcp_attribution_checkpoint(/*force*/ false)
                        {
                            envelope.metadata.get_or_insert_default().mcp_attribution = Some(attribution);
                            mcp_revision = Some(revision);
                        }
                        let prepared = PreparedHistoryItems::new(
                            &session.state.lock().await.history,
                            vec![envelope],
                            turn.model_info().truncation_policy.into(),
                        );
                        let prepared_items = prepared.rollout_items();
                        let [RolloutItem::ResponseItem(envelope)] = prepared_items.as_slice() else {
                            return Err(CodexErr::Fatal("observed response preparation lost its source envelope".to_string()));
                        };
                        let envelope = envelope.clone();
                        records.push(RolloutItem::InterAgentCommunicationMetadata { trigger_turn });
                        records.extend(prepared_items);
                        let mut batch = session.conversation_publication_batch(
                            turn, Vec::new(), std::slice::from_ref(&envelope.item),
                        ).await;
                        for event in &mut batch.events { event.id = turn_id.clone(); }
                        events.extend(batch.events);
                        Some((prepared, envelope))
                    } else { None };
                    // Keep metadata / response / observation records adjacent for audit readers.
                    records.extend(control.deferred_response_observation_commit_snapshots(&commit)
                        .into_iter().map(RolloutItem::AgentResponseObservation));
                    let runtime_only = session.live_thread().is_none();
                    let install_commit = commit.clone();
                    let executed_tool_calls = session.services.executed_tool_calls.clone();
                    let receiver = session.dispatch_completion_publication(
                        permit, records, events,
                        move |state| {
                            let _boundary = boundary;
                            if let Some((prepared, envelope)) = prepared {
                                if !state.history.raw_items().any(|item| item == &envelope.item) {
                                    state.current_time_reminder.note_recorded_items(std::slice::from_ref(&envelope.item));
                                    prepared.install(&mut state.history);
                                }
                                if runtime_only || codex_protocol::protocol::is_sub_agent_completion_context_response_item_id(
                                    install_commit.response_item_id.as_str(),
                                ) {
                                    state.completion_publication_receipts.contexts.insert(
                                        install_commit.response_item_id.clone(), envelope.clone(),
                                    );
                                }
                                if !state.acknowledged_completion_contexts.iter()
                                    .any(|retained| retained.item == envelope)
                                {
                                    state.acknowledged_completion_contexts.push(crate::state::AcknowledgedCompletionContext {
                                        item: envelope, pending: false,
                                    });
                                }
                            }
                            if let Some(revision) = mcp_revision {
                                executed_tool_calls.mark_mcp_attribution_persisted(revision);
                            }
                            control.commit_response_observation_delivery(&install_commit);
                        },
                        || {},
                    )?;
                    drop(order);
                    let result = session.publication_result(receiver).await;
                    drop(active);
                    result?;
                    return Ok(());
                }
            }.await;
            if let Err(error) = &result {
                session.quarantine_history(format!("observed response publication failed: {error}"));
                session.input_queue.completion_commit_changed.notify_waiters();
            }
            outcome.finished = true;
            result
        }).await.map_err(|error| {
            self.quarantine_history(format!("observed response lost its receipt: {error}"));
            CodexErr::Fatal(format!("observed response lost its receipt: {error}"))
        })?
    }
}
