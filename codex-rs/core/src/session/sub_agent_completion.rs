//! Canonical completion publication. Ambiguous writes require a new runtime.

use super::*;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_thread_store::LoadSubAgentCompletionContextItemParams;
use codex_thread_store::LoadSubAgentCompletionPresentationParams;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrimaryEventEnqueue {
    Enqueued,
    Closed,
}

pub(super) struct CanonicalCompletionReceipt {
    pub(super) primary_event: PrimaryEventEnqueue,
}

pub(crate) enum CompletionContextDelivery {
    InstallNow,
    QueueOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionContextPublication {
    Published,
    AlreadyPublished,
}

struct CompletionConsumption {
    session: Arc<Session>,
    finished: bool,
}

impl Drop for CompletionConsumption {
    fn drop(&mut self) {
        if !self.finished {
            self.session
                .quarantine_history("completion consumption lost its worker receipt".to_string());
            self.session
                .input_queue
                .completion_commit_changed
                .notify_waiters();
        }
    }
}

impl Session {
    pub(super) async fn consume_completion_context(
        self: &Arc<Self>,
        communication: &InterAgentCommunication,
        model_info: &ModelInfo,
    ) -> CodexResult<()> {
        let id = match self
            .input_queue
            .begin_completion_communication_commit(communication)
            .await
        {
            input_queue::CompletionCommunicationCommit::Started(id) => id,
            input_queue::CompletionCommunicationCommit::AlreadyStarted => return Ok(()),
            input_queue::CompletionCommunicationCommit::Ordinary => {
                return Err(CodexErr::InvalidRequest(
                    "expected completion context".to_string(),
                ));
            }
        };
        let session = Arc::clone(self);
        let communication = communication.clone();
        let model_info = model_info.clone();
        let result = tokio::spawn(async move {
            let mut receipt = CompletionConsumption {
                session: Arc::clone(&session),
                finished: false,
            };
            let result = async {
                let response = communication.to_model_input_item();
                if response.id() != Some(&id) {
                    return Err(CodexErr::InvalidRequest(
                        "completion lease identity changed".to_string(),
                    ));
                }
                let permit = session.acquire_history_publication_barrier().await?;
                let stored = session
                    .services
                    .thread_store
                    .load_sub_agent_completion_context_item(
                        LoadSubAgentCompletionContextItemParams {
                            thread_id: session.thread_id,
                            include_archived: false,
                            response_item_id: id.clone(),
                        },
                    )
                    .await
                    .map_err(|error| CodexErr::Fatal(error.to_string()))?;
                if stored.as_ref() != Some(&response) {
                    return Err(CodexErr::InvalidRequest(
                        "completion mailbox item has no matching canonical provenance".to_string(),
                    ));
                }
                let policy = model_info.truncation_policy.into();
                let passive_final_activity = session.passive_final_delivery_activity.clone();
                let is_passive = !communication.trigger_turn && !communication.defer_to_next_turn;
                let receiver = session.dispatch_completion_publication(
                    permit,
                    Vec::new(),
                    Vec::new(),
                    move |state| {
                        if !state.history.raw_items().any(|item| item == &response) {
                            state.record_items(std::iter::once(&response), policy);
                            if is_passive {
                                passive_final_activity.send_replace(());
                            }
                        }
                        if let Some(retained) = state
                            .acknowledged_completion_contexts
                            .iter_mut()
                            .find(|retained| retained.item.item == response)
                        {
                            retained.pending = false;
                        } else {
                            state.acknowledged_completion_contexts.push(
                                crate::state::AcknowledgedCompletionContext {
                                    item: response.into(),
                                    pending: false,
                                },
                            );
                        }
                    },
                    || {},
                )?;
                session.publication_result(receiver).await?;
                session
                    .input_queue
                    .acknowledge_completion_communication(&id)
                    .await;
                session
                    .services
                    .agent_control
                    .claim_completion_context_response_item_id(session.presentation_id(), &id);
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                session.quarantine_history(format!("completion consumption failed: {error}"));
                session
                    .input_queue
                    .completion_commit_changed
                    .notify_waiters();
            }
            receipt.finished = true;
            result
        })
        .await
        .map_err(|error| {
            self.quarantine_history(format!("completion consumption lost its receipt: {error}"));
            self.input_queue.completion_commit_changed.notify_waiters();
            CodexErr::Fatal(format!("completion consumption lost its receipt: {error}"))
        })?;
        result
    }

    pub(super) async fn drain_completion_mailbox(self: &Arc<Self>) -> CodexResult<()> {
        let turn = self.new_history_only_turn().await;
        loop {
            let changed = self.input_queue.completion_commit_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            self.check_history_publication()?;
            let drain = self
                .input_queue
                .drain_completion_communications_for_shutdown()
                .await;
            for communication in drain.communications {
                self.consume_completion_context(&communication, turn.model_info())
                    .await?;
            }
            if !drain.has_committing {
                return Ok(());
            }
            changed.await;
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "accepted completion ordering spans canonical lookup and writer dispatch"
    )]
    pub(crate) async fn persist_completion_context(
        &self,
        response: ResponseItem,
        reservation: &AcceptedCompletionDelivery,
        delivery: CompletionContextDelivery,
    ) -> CodexResult<CompletionContextPublication> {
        let order = self
            .submission_admission
            .admit_completion(reservation)
            .await?;
        let permit = self.acquire_history_publication_barrier().await?;
        let id = response
            .id()
            .filter(|id| is_sub_agent_completion_context_response_item_id(id.as_str()))
            .ok_or_else(|| {
                CodexErr::InvalidRequest("completion has no trusted identity".to_string())
            })?;
        let previous = self
            .services
            .thread_store
            .load_sub_agent_completion_context_item(LoadSubAgentCompletionContextItemParams {
                thread_id: self.thread_id,
                include_archived: false,
                response_item_id: id.clone(),
            })
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?;
        if previous
            .as_ref()
            .is_some_and(|previous| previous != &response)
        {
            return Err(CodexErr::InvalidRequest(
                "completion identity payload changed".to_string(),
            ));
        }
        if previous.is_some() {
            // The canonical worker retains this barrier through live installation.
            // Its publisher owns any remaining presentation/enqueue work.
            return Ok(CompletionContextPublication::AlreadyPublished);
        }
        let items = vec![
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
            },
            RolloutItem::ResponseItem(response.clone().into()),
        ];
        let turn = self.new_history_only_turn().await;
        let policy = turn.model_info().truncation_policy.into();
        let observations = Arc::clone(&self.response_observation_state);
        let settled = response.clone();
        let passive_final_activity = self.passive_final_delivery_activity.clone();
        let receiver = self.dispatch_completion_publication(
            permit,
            items,
            Vec::new(),
            move |state| {
                if let Some(id) = settled.id().cloned() {
                    observations
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .settled_completion_contexts
                        .insert(id, settled);
                }
                if matches!(delivery, CompletionContextDelivery::InstallNow)
                    && !state.history.raw_items().any(|item| item == &response)
                {
                    state.record_items(std::iter::once(&response), policy);
                    // This callback runs after canonical ACK, before any presentation awaits.
                    // Queue-only publication is not live model-context delivery.
                    passive_final_activity.send_replace(());
                }
                if let Some(retained) = state
                    .acknowledged_completion_contexts
                    .iter_mut()
                    .find(|retained| retained.item.item == response)
                {
                    retained.pending &= matches!(delivery, CompletionContextDelivery::QueueOnly);
                } else {
                    state.acknowledged_completion_contexts.push(
                        crate::state::AcknowledgedCompletionContext {
                            item: response.into(),
                            pending: matches!(delivery, CompletionContextDelivery::QueueOnly),
                        },
                    );
                }
            },
            || {},
        )?;
        drop(order);
        self.publication_result(receiver)
            .await
            .inspect_err(|error| self.quarantine_history(error.to_string()))?;
        Ok(CompletionContextPublication::Published)
    }

    /// Selects the real active turn once, or a stable UUIDv7 history-only turn.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "completion publication binds the active turn until its canonical receipt"
    )]
    pub(crate) async fn publish_completion_item(
        &self,
        presentation: &crate::agent::control::CompletionPresentation,
        reservation: &AcceptedCompletionDelivery,
    ) -> CodexResult<()> {
        let item = presentation.item.clone();
        loop {
            let changed = self.active_turn_transition.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let order = self
                .submission_admission
                .admit_completion(reservation)
                .await?;
            let permit = self.acquire_history_publication_barrier().await?;
            let active = self.active_turn.lock().await;
            let (turn_id, history_only) = match active.as_ref() {
                Some(active_turn) => match &active_turn.task {
                    Some(task) => (task.turn_context.sub_id.clone(), false),
                    None => {
                        drop(active);
                        drop(permit);
                        drop(order);
                        changed.await;
                        continue;
                    }
                },
                None => (presentation.history_only_turn_id.clone(), true),
            };
            let stored = self
                .services
                .thread_store
                .load_sub_agent_completion_presentation(LoadSubAgentCompletionPresentationParams {
                    thread_id: self.thread_id,
                    include_archived: false,
                    item_id: item.id(),
                    turn_id: turn_id.clone(),
                })
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
            if let Some(event) = &stored.item_completed
                && (event.turn_id != turn_id
                    || serde_json::to_value(&event.item)
                        .map_err(|error| CodexErr::Fatal(error.to_string()))?
                        != serde_json::to_value(&item)
                            .map_err(|error| CodexErr::Fatal(error.to_string()))?)
            {
                return Err(CodexErr::InvalidRequest(
                    "completion presentation identity changed".to_string(),
                ));
            }
            let completed = stored
                .item_completed
                .clone()
                .unwrap_or_else(|| ItemCompletedEvent {
                    thread_id: self.thread_id,
                    turn_id: turn_id.clone(),
                    item: item.clone(),
                    started_at_ms: None,
                    completed_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
                });
            let mut records = Vec::new();
            if history_only && !stored.turn_started {
                records.push(RolloutItem::EventMsg(EventMsg::TurnStarted(
                    TurnStartedEvent {
                        turn_id: turn_id.clone(),
                        root_turn_id: None,
                        trace_id: None,
                        started_at: None,
                        model_context_window: None,
                        collaboration_mode_kind: Default::default(),
                        agent_queue: None,
                    },
                )));
            }
            if stored.item_completed.is_none() {
                records.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(
                    completed.clone(),
                )));
            }
            if history_only && !stored.turn_completed {
                records.push(RolloutItem::EventMsg(EventMsg::TurnComplete(
                    TurnCompleteEvent {
                        turn_id: turn_id.clone(),
                        last_agent_message: None,
                        error: None,
                        started_at: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    },
                )));
            }
            let events = vec![
                Event {
                    id: turn_id.clone(),
                    msg: EventMsg::ItemStarted(ItemStartedEvent {
                        thread_id: self.thread_id,
                        turn_id: turn_id.clone(),
                        item,
                        started_at_ms: completed.completed_at_ms,
                    }),
                },
                Event {
                    id: turn_id,
                    msg: EventMsg::ItemCompleted(completed),
                },
            ];
            let receiver =
                self.dispatch_completion_publication(permit, records, events, |_| {}, || {})?;
            drop(order);
            let result = self.publication_result(receiver).await;
            drop(active);
            result.inspect_err(|error| self.quarantine_history(error.to_string()))?;
            return Ok(());
        }
    }

    /// Wait ownership transfers only after canonical commit and primary event enqueue.
    pub(crate) async fn emit_turn_item_completed_with_primary_delivery(
        &self,
        turn: &TurnContext,
        item: TurnItem,
        on_primary_delivery: impl FnOnce() + Send + 'static,
    ) {
        let result = async {
            let permit = self.acquire_history_publication_barrier().await?;
            let event = Event {
                id: turn.sub_id.clone(),
                msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id: self.thread_id,
                    turn_id: turn.sub_id.clone(),
                    item,
                    started_at_ms: None,
                    completed_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
                }),
            };
            let receiver = self.dispatch_completion_publication(
                permit,
                vec![RolloutItem::EventMsg(event.msg.clone())],
                vec![event],
                |_| {},
                on_primary_delivery,
            )?;
            self.publication_result(receiver).await
        }
        .await;
        match result {
            Ok(CanonicalCompletionReceipt {
                primary_event: PrimaryEventEnqueue::Enqueued,
            }) => {}
            Ok(CanonicalCompletionReceipt {
                primary_event: PrimaryEventEnqueue::Closed,
            }) => {}
            Err(error) => {
                self.quarantine_history(format!("wait completion publication failed: {error}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "sub_agent_completion_tests.rs"]
mod tests;
