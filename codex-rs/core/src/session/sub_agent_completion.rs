use super::CompletionSubmissionAdmission;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::agent::agent_status_from_event;
use crate::agent::control::AgentTerminalPresentation;
use crate::agent::control::TerminalPresentationDelivery;
use crate::agent::status::is_final;
use crate::turn_timing::now_unix_timestamp_ms;
use codex_protocol::ResponseItemId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_history::RolloutItem;
use codex_thread_store::LoadSubAgentCompletionContextItemParams;
use codex_thread_store::LoadSubAgentCompletionPresentationParams;
use codex_thread_store::StoredSubAgentCompletionPresentation;
use codex_thread_store::ThreadStoreError;
use tokio::sync::mpsc;

pub(crate) struct TerminalStatusSubscription {
    id: u64,
    subscribers: std::sync::Weak<
        std::sync::Mutex<std::collections::HashMap<u64, mpsc::UnboundedSender<AgentStatus>>>,
    >,
    receiver: mpsc::UnboundedReceiver<AgentStatus>,
}

impl TerminalStatusSubscription {
    pub(crate) fn try_recv(&mut self) -> Result<AgentStatus, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    pub(crate) async fn recv(&mut self) -> Option<AgentStatus> {
        self.receiver.recv().await
    }
}

impl Drop for TerminalStatusSubscription {
    fn drop(&mut self) {
        if let Some(subscribers) = self.subscribers.upgrade() {
            subscribers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.id);
        }
    }
}

impl Session {
    pub(crate) async fn wait_for_completion_submission_admission(
        &self,
        admission: CompletionSubmissionAdmission,
    ) -> bool {
        match admission {
            CompletionSubmissionAdmission::Ordinary => {
                self.submission_admission
                    .wait_for_completion_submission()
                    .await
            }
            CompletionSubmissionAdmission::Accepted => {
                self.submission_admission
                    .wait_for_accepted_completion_submission()
                    .await
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "completion context persistence must remain ordered before shutdown admission"
    )]
    pub(crate) async fn record_sub_agent_notification_with_admission(
        self: &std::sync::Arc<Self>,
        message: String,
        response_item_id: ResponseItemId,
        admission: CompletionSubmissionAdmission,
    ) -> Result<(), ThreadStoreError> {
        let _admission_guard = loop {
            let admitted = self
                .wait_for_completion_submission_admission(admission)
                .await;
            if !admitted {
                return Err(codex_thread_store::ThreadStoreError::Internal {
                    message: "parent session is no longer accepting completion context".to_string(),
                });
            }
            let admission_guard = self.submission_admission.send_lock.lock().await;
            if matches!(admission, CompletionSubmissionAdmission::Ordinary)
                && (self
                    .submission_admission
                    .shutdown_pending
                    .load(std::sync::atomic::Ordering::Acquire)
                    || self
                        .submission_admission
                        .completion_delivery_admission_closed
                        .load(std::sync::atomic::Ordering::Acquire))
            {
                return Err(codex_thread_store::ThreadStoreError::Internal {
                    message: "parent session shutdown is already in progress".to_string(),
                });
            }
            let admission_state = *self
                .submission_admission
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match admission_state {
                super::SubmissionAdmissionState::Ready => break admission_guard,
                super::SubmissionAdmissionState::RollbackPending
                | super::SubmissionAdmissionState::RollbackEventPending => drop(admission_guard),
                super::SubmissionAdmissionState::ReloadRequired => {
                    return Err(codex_thread_store::ThreadStoreError::Internal {
                        message: "parent session requires reload before completion context"
                            .to_string(),
                    });
                }
            }
        };
        let turn_context = self.new_default_turn().await;
        self.record_durable_context_items(
            turn_context,
            vec![ResponseItem::Message {
                id: Some(response_item_id),
                role: "user".to_string(),
                content: vec![ContentItem::InputText { text: message }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
            /*acknowledgement*/ None,
        )
        .await
    }

    pub(super) async fn persisted_sub_agent_completion_context_item(
        &self,
        response_item_id: &ResponseItemId,
    ) -> Result<Option<ResponseItem>, ThreadStoreError> {
        if !is_sub_agent_completion_context_response_item_id(response_item_id.as_str()) {
            return Ok(None);
        }
        if self.live_thread().is_none() {
            return Ok(None);
        }
        self.services
            .thread_store
            .load_sub_agent_completion_context_item(LoadSubAgentCompletionContextItemParams {
                thread_id: self.thread_id,
                include_archived: false,
                response_item_id: response_item_id.clone(),
            })
            .await
    }

    pub(super) async fn persisted_sub_agent_completion_presentation(
        &self,
        item_id: &str,
        turn_id: &str,
    ) -> Result<StoredSubAgentCompletionPresentation, ThreadStoreError> {
        if self.live_thread().is_none() {
            return Ok(StoredSubAgentCompletionPresentation::default());
        }
        self.services
            .thread_store
            .load_sub_agent_completion_presentation(LoadSubAgentCompletionPresentationParams {
                thread_id: self.thread_id,
                include_archived: false,
                item_id: item_id.to_string(),
                turn_id: turn_id.to_string(),
            })
            .await
    }

    pub(crate) fn subscribe_terminal_status(&self) -> (AgentStatus, TerminalStatusSubscription) {
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (terminal_status_tx, terminal_status_rx) = mpsc::unbounded_channel();
        let id = self
            .next_terminal_status_subscriber_id
            .fetch_add(/*val*/ 1, std::sync::atomic::Ordering::Relaxed);
        self.terminal_status_subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, terminal_status_tx);
        (
            self.agent_status.borrow().clone(),
            TerminalStatusSubscription {
                id,
                subscribers: std::sync::Arc::downgrade(&self.terminal_status_subscribers),
                receiver: terminal_status_rx,
            },
        )
    }

    pub(super) fn replace_agent_status_locked(&self, status: AgentStatus) {
        if is_final(&status)
            && !self
                .terminal_status_suppressed
                .load(std::sync::atomic::Ordering::Acquire)
        {
            self.terminal_status_subscribers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|_, subscriber| subscriber.send(status.clone()).is_ok());
        }
        self.agent_status.send_replace(status);
    }

    pub(super) fn publish_agent_status_from_event(&self, event: &EventMsg) {
        let Some(status) = agent_status_from_event(event) else {
            return;
        };
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_status = self.agent_status.borrow().clone();
        if matches!(&status, AgentStatus::Running) || !is_final(&current_status) {
            self.replace_agent_status_locked(status);
        }
    }

    fn record_sub_agent_terminal_presentation(
        &self,
        parent_thread_id: codex_protocol::ThreadId,
        turn_id: &str,
        status: AgentStatus,
        delivery: TerminalPresentationDelivery,
    ) -> Option<AgentTerminalPresentation> {
        if !self
            .terminal_presentation_armed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_status = self.agent_status.borrow().clone();
        if is_final(&current_status) {
            return None;
        }
        let child = self.presentation_id();
        let parent = self
            .services
            .agent_control
            .completion_parent_for_child(child, parent_thread_id);
        let Some(parent) = parent else {
            self.replace_agent_status_locked(status);
            return None;
        };
        self.services
            .agent_control
            .record_agent_terminal_presentation(
                parent,
                child,
                turn_id,
                status.clone(),
                delivery,
                || {
                    self.replace_agent_status_locked(status);
                },
            )
    }

    pub(super) async fn prepare_sub_agent_terminal_presentation(
        &self,
        turn_context: &TurnContext,
        event: &EventMsg,
    ) -> Option<AgentTerminalPresentation> {
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path,
            ..
        }) = &turn_context.session_source
        else {
            return None;
        };
        let delivery = match (turn_context.multi_agent_version, event) {
            (
                codex_protocol::protocol::MultiAgentVersion::V2,
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_),
            ) if agent_path.is_some() => TerminalPresentationDelivery::Direct,
            _ => TerminalPresentationDelivery::Watcher,
        };
        let status = if delivery == TerminalPresentationDelivery::Direct {
            turn_context
                .terminal_error
                .lock()
                .await
                .as_ref()
                .map(|error| AgentStatus::Errored(error.message.clone()))
                .or_else(|| agent_status_from_event(event))?
        } else {
            agent_status_from_event(event)?
        };
        if !is_final(&status) {
            return None;
        }
        self.record_sub_agent_terminal_presentation(
            *parent_thread_id,
            &turn_context.sub_id,
            status,
            delivery,
        )
    }

    pub(super) fn prepare_raw_sub_agent_terminal_presentation(&self, event: &Event) {
        let Some(status) = agent_status_from_event(&event.msg).filter(is_final) else {
            return;
        };
        let Some(parent_thread_id) = self.spawn_parent_thread_id else {
            return;
        };
        let generated_turn_id;
        let turn_id = if event.id.is_empty() {
            generated_turn_id = uuid::Uuid::now_v7().to_string();
            generated_turn_id.as_str()
        } else {
            event.id.as_str()
        };
        let _ = self.record_sub_agent_terminal_presentation(
            parent_thread_id,
            turn_id,
            status,
            TerminalPresentationDelivery::Watcher,
        );
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "completion persistence must stay atomic with parent turn transitions"
    )]
    pub(crate) async fn emit_turn_item_completed_without_turn_with_history_id(
        &self,
        item: TurnItem,
        history_only_turn_id: &str,
    ) -> Result<(), ThreadStoreError> {
        let item_id = item.id();
        loop {
            let transition = self.active_turn_transition.notified();
            tokio::pin!(transition);
            transition.as_mut().enable();
            let durable_context_permit =
                self.durable_context_lock.acquire().await.map_err(|err| {
                    codex_thread_store::ThreadStoreError::Internal {
                        message: format!("failed to lock subagent completion recording: {err}"),
                    }
                })?;
            let active_turn_guard = self.active_turn.lock().await;
            let active_turn_context = active_turn_guard.as_ref().map(|active_turn| {
                active_turn
                    .task
                    .as_ref()
                    .map(|task| task.turn_context.clone())
            });

            let persisted = self
                .persisted_sub_agent_completion_presentation(item_id.as_str(), history_only_turn_id)
                .await?;
            let history_only_turn_started = persisted.turn_started;
            let history_only_turn_completed = persisted.turn_completed;
            let persisted_completion = persisted.item_completed;
            let persisted_completion_is_complete = persisted_completion
                .as_ref()
                .filter(|event| {
                    event.turn_id != history_only_turn_id
                        || (history_only_turn_started && history_only_turn_completed)
                })
                .cloned();
            if let Some(item_completed) = persisted_completion_is_complete {
                let item_started = EventMsg::ItemStarted(ItemStartedEvent {
                    thread_id: self.thread_id,
                    turn_id: item_completed.turn_id.clone(),
                    item: item_completed.item.clone(),
                    started_at_ms: item_completed.completed_at_ms,
                });
                let turn_id = item_completed.turn_id.clone();
                drop(durable_context_permit);
                self.send_event_raw_with_persistence(
                    Event {
                        id: turn_id.clone(),
                        msg: item_started,
                    },
                    /*persist*/ false,
                )
                .await;
                self.send_event_raw_with_persistence(
                    Event {
                        id: turn_id,
                        msg: EventMsg::ItemCompleted(item_completed),
                    },
                    /*persist*/ false,
                )
                .await;
                drop(active_turn_guard);
                return Ok(());
            }

            let partial_history_only_turn = history_only_turn_started
                || history_only_turn_completed
                || persisted_completion
                    .as_ref()
                    .is_some_and(|event| event.turn_id == history_only_turn_id);
            let history_only_completion_persisted = persisted_completion
                .as_ref()
                .is_some_and(|event| event.turn_id == history_only_turn_id);
            let (turn_context, history_only) =
                match (partial_history_only_turn, active_turn_context) {
                    (true, _) => (
                        self.new_default_turn_with_sub_id(history_only_turn_id.to_string())
                            .await,
                        true,
                    ),
                    (false, Some(Some(turn_context))) => (turn_context, false),
                    (false, Some(None)) => {
                        drop(active_turn_guard);
                        drop(durable_context_permit);
                        transition.as_mut().await;
                        continue;
                    }
                    (false, None) => (
                        self.new_default_turn_with_sub_id(history_only_turn_id.to_string())
                            .await,
                        true,
                    ),
                };
            let completed_at_ms = now_unix_timestamp_ms();
            let item_completed = persisted_completion.unwrap_or_else(|| ItemCompletedEvent {
                thread_id: self.thread_id,
                turn_id: turn_context.sub_id.clone(),
                item,
                started_at_ms: None,
                completed_at_ms,
            });
            let item_started = EventMsg::ItemStarted(ItemStartedEvent {
                thread_id: self.thread_id,
                turn_id: item_completed.turn_id.clone(),
                item: item_completed.item.clone(),
                started_at_ms: item_completed.completed_at_ms,
            });
            let item_completed = EventMsg::ItemCompleted(item_completed);
            let rollout_items = if history_only {
                let mut rollout_items = Vec::new();
                if !history_only_turn_started {
                    rollout_items.push(RolloutItem::EventMsg(EventMsg::TurnStarted(
                        TurnStartedEvent {
                            turn_id: turn_context.sub_id.clone(),
                            trace_id: None,
                            started_at: None,
                            model_context_window: None,
                            collaboration_mode_kind: Default::default(),
                        },
                    )));
                }
                if !history_only_completion_persisted {
                    rollout_items.push(RolloutItem::EventMsg(item_completed.clone()));
                }
                if !history_only_turn_completed {
                    rollout_items.push(RolloutItem::EventMsg(EventMsg::TurnComplete(
                        TurnCompleteEvent {
                            turn_id: turn_context.sub_id.clone(),
                            last_agent_message: None,
                            error: None,
                            started_at: None,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        },
                    )));
                }
                rollout_items
            } else {
                vec![RolloutItem::EventMsg(item_completed.clone())]
            };
            if let Some(live_thread) = self.live_thread() {
                live_thread
                    .append_items_and_flush_canonical(&rollout_items)
                    .await?;
            }
            drop(durable_context_permit);
            self.send_event_raw_with_persistence(
                Event {
                    id: turn_context.sub_id.clone(),
                    msg: item_started,
                },
                /*persist*/ false,
            )
            .await;
            self.send_event_raw_with_persistence(
                Event {
                    id: turn_context.sub_id.clone(),
                    msg: item_completed,
                },
                /*persist*/ false,
            )
            .await;
            drop(active_turn_guard);
            return Ok(());
        }
    }
}

#[cfg(test)]
#[path = "sub_agent_completion_tests.rs"]
mod tests;
