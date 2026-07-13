//! Orders retained inputs at acceptance, assistant messages at stream start,
//! and Code Mode messages at confirmed delivery.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::task::Context;
use std::task::Poll;

use crate::context::ContextualUserFragment;
use crate::context::UserGoalUpdate;
use codex_history::RetainedContextEvent;
use codex_history::RetainedUserMessage;
use codex_history::RolloutItem;
use codex_protocol::models::ResponseItem;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

use super::Session;
use super::TurnContext;
use super::thread_settings;

/// Only unrecorded starts need a reservation; abandoned entries expire with the turn.
#[derive(Default)]
pub(super) struct PendingAssistantMessageOrders(pub(super) Mutex<HashMap<String, u64>>);

pub(super) struct CodeModeMessageTasks {
    tasks: Mutex<TaskTracker>,
    pending_persistence: Arc<Mutex<Vec<watch::Receiver<()>>>>,
    pub(super) communication_boundary: Arc<Semaphore>,
}

impl Default for CodeModeMessageTasks {
    fn default() -> Self {
        Self {
            tasks: Mutex::default(),
            pending_persistence: Arc::default(),
            communication_boundary: Arc::new(Semaphore::new(1)),
        }
    }
}

impl Session {
    /// Reserve before deriving display items, including plans, from the source message.
    /// Completion-only responses use the same path before publishing their text.
    pub(super) async fn reserve_assistant_message_order(
        &self,
        turn_context: &TurnContext,
        item: &ResponseItem,
    ) {
        if let ResponseItem::Message {
            id: Some(id), role, ..
        } = item
            && role == "assistant"
        {
            let mut state = self.state.lock().await;
            if !state
                .history
                .raw_items()
                .any(|item| item.id().is_some_and(|recorded_id| recorded_id == id))
            {
                turn_context
                    .extension_data
                    .get_or_init(PendingAssistantMessageOrders::default)
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(id.to_string())
                    .or_insert_with(|| state.history.reserve_input_order());
            }
        }
    }

    /// Records authorization without adding pending input or reopening an active turn.
    pub(crate) async fn record_user_goal_update(&self, update: UserGoalUpdate) {
        // Goal metadata must not initialize a model step, even on a goal-first thread.
        // Keep context construction off callers' stacks, including the TUI RPC dispatcher.
        let context = Box::pin(self.new_inject_items_context()).await;
        self.record_conversation_items(
            &context,
            context.model_info(),
            &[ContextualUserFragment::into(update)],
        )
        .await;
    }

    pub(crate) async fn reserve_user_input_order(&self) -> u64 {
        let _boundary = self
            .code_mode_message_tasks
            .communication_boundary
            .acquire()
            .await
            .unwrap_or_else(|_| unreachable!("communication boundary remains open"));
        let (order, pending) = {
            let mut state = self.state.lock().await;
            let order = state.history.reserve_input_order();
            (order, self.pending_code_mode_message_recordings())
        };
        // Older readers assign assistant records to physical instruction boundaries.
        // Keep a later user steer from reaching the rollout ahead of an earlier send.
        for mut recording in pending {
            let _ = recording.changed().await;
        }
        order
    }

    /// Snapshot while holding `state` so earlier confirmed delivery reservations are included.
    pub(super) fn pending_code_mode_message_recordings(&self) -> Vec<watch::Receiver<()>> {
        let mut pending = self
            .code_mode_message_tasks
            .pending_persistence
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        pending.retain(|recording| recording.has_changed().is_ok());
        pending.clone()
    }

    pub(crate) async fn record_retained_context(&self, mut event: RetainedContextEvent) {
        event.bound();
        // Share the checkpoint persistence lock so a fact cannot land on the wrong side
        // of the checkpoint/suffix boundary. Ephemeral threads use the same live state.
        let permit = thread_settings::acquire_persistence_lock(self).await;
        let mut projected = self.state.lock().await.history.clone();
        if projected.record_retained_context(&event) {
            let result = match self.dispatch_history_publication(
                permit,
                vec![RolloutItem::RetainedContext(event.clone())],
                Vec::new(),
                /*acknowledgement*/ None,
                move |state| {
                    state.history.record_retained_context(&event);
                },
            ) {
                Ok(receiver) => self.publication_result(receiver).await,
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                tracing::error!("failed to publish retained context: {error}");
            }
        }
    }

    pub(crate) fn track_code_mode_message(&self) -> Option<TaskTrackerToken> {
        let tasks = self
            .code_mode_message_tasks
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (!tasks.is_closed()).then(|| tasks.token())
    }

    pub(super) async fn drain_code_mode_messages(&self) {
        let tasks = {
            let tasks = self
                .code_mode_message_tasks
                .tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            tasks.close();
            tasks.clone()
        };
        tasks.wait().await;
    }

    /// Starts recording confirmed delivery synchronously while the tool's admission is held.
    pub(crate) fn record_delivered_assistant_message(
        self: &Arc<Self>,
        message: RetainedUserMessage,
    ) -> (JoinHandle<()>, watch::Receiver<()>) {
        let state = Arc::clone(&self.state);
        let publication = self.history_publication_handle();
        let persistence = Arc::clone(&self.thread_settings_persistence);
        let communication_boundary =
            Arc::clone(&self.code_mode_message_tasks.communication_boundary);
        let pending_persistence = Arc::clone(&self.code_mode_message_tasks.pending_persistence);
        let (recorded, recording) = watch::channel(());
        let pending_recording = recording.clone();
        let mut reserve = Box::pin(async move {
            let boundary = communication_boundary
                .acquire_owned()
                .await
                .unwrap_or_else(|_| unreachable!("communication boundary remains open"));
            let mut state = state.lock().await;
            let mut event = RetainedContextEvent::DeliveredAssistantMessage {
                message,
                acceptance_order: state.history.reserve_input_order(),
            };
            event.bound();
            let mut pending = pending_persistence
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            pending.retain(|recording| recording.has_changed().is_ok());
            pending.push(pending_recording);
            (event, boundary)
        });
        // Reserve at the confirmed MCP response boundary, ahead of later user input.
        // A pending state lock queues the same reservation without retaining Session.
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let reservation = reserve.as_mut().poll(&mut context);
        let task = self
            .code_mode_message_tasks
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spawn(async move {
                let (event, boundary) = match reservation {
                    Poll::Ready(reservation) => reservation,
                    Poll::Pending => reserve.await,
                };
                let permit = persistence
                    .acquire_owned()
                    .await
                    .unwrap_or_else(|_| unreachable!("history publication remains open"));
                // Keep distinct delivery receipts canonical even when bounded live evidence
                // is full. Closing the receipt follows installation, or sticky failure.
                let result = publication.dispatch_history_publication_with_events(
                    permit,
                    super::durable_context::PublicationBatch {
                        rollout: vec![RolloutItem::RetainedContext(event.clone())],
                        events: Vec::new(),
                        reply: None,
                    },
                    Vec::new(),
                    /*acknowledgement*/ None,
                    move |state| {
                        let _boundary = boundary;
                        state.history.record_retained_context(&event);
                        drop(recorded);
                    },
                );
                match result {
                    Ok(receiver) => match receiver.await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::error!("failed to publish assistant delivery: {error}")
                        }
                        Err(error) => {
                            tracing::error!("assistant delivery publisher was lost: {error}")
                        }
                    },
                    Err(error) => {
                        tracing::error!("assistant delivery publication rejected: {error}")
                    }
                }
            });
        (task, recording)
    }
}

#[cfg(test)]
#[path = "retained_context_tests.rs"]
mod tests;
