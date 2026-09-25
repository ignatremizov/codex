//! Single-attempt publication for explicit user control and trusted task promotion.
//!
//! The owned workers outlive their result waiters. A missing or ambiguous receipt poisons the
//! source writer; it never retracts target work or compensates with a second append.

use std::sync::Arc;
use std::sync::PoisonError;

use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserAgentControlItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use tokio::sync::OwnedMutexGuard;

use super::Session;

#[cfg(test)]
#[path = "user_agent_publication_tests.rs"]
mod tests;

struct PublicationOutcome {
    session: Arc<Session>,
    finished: bool,
}

impl Drop for PublicationOutcome {
    fn drop(&mut self) {
        if !self.finished {
            self.session.quarantine_history(
                "user-agent publication lost its canonical receipt; reload required".to_string(),
            );
        }
    }
}

impl Session {
    /// Persists source-side control audit without changing the source response lifecycle.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active-turn ownership must remain fixed from audit attribution through canonical publication and primary event enqueue"
    )]
    pub(crate) async fn record_user_agent_control(
        self: &Arc<Self>,
        item: UserAgentControlItem,
    ) -> CodexResult<()> {
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let mut outcome = PublicationOutcome {
                session: Arc::clone(&session),
                finished: false,
            };
            let result: CodexResult<()> = async {
                let permit = session.acquire_history_publication_barrier().await?;
                // Match canonical delivery lock order. Turn start/finalization cannot race the
                // choice of transcript owner while this independently driven append is pending.
                let active = session.active_turn.lock().await;
                let turn_id = active
                    .as_ref()
                    .and_then(|turn| turn.task.as_ref())
                    .map(|task| task.turn_context.sub_id.clone())
                    .or_else(|| {
                        let responses = session
                            .response_observation_state
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        responses
                            .active_turn_id
                            .clone()
                            .filter(|turn_id| responses.live_turn_id.as_ref() == Some(turn_id))
                    })
                    .unwrap_or_else(|| item.id.clone());
                let completed_at_ms = crate::turn_timing::now_unix_timestamp_ms();
                let event = Event {
                    id: item.id.clone(),
                    msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                        thread_id: session.thread_id,
                        turn_id,
                        item: TurnItem::UserAgentControl(item),
                        started_at_ms: Some(completed_at_ms),
                        completed_at_ms,
                    }),
                };
                let receiver = session.dispatch_completion_publication(
                    permit,
                    vec![RolloutItem::EventMsg(event.msg.clone())],
                    vec![event],
                    |_| {},
                    || {},
                )?;
                let receipt = session.publication_result(receiver).await?;
                if receipt.primary_event == super::sub_agent_completion::PrimaryEventEnqueue::Closed {
                    return Err(CodexErr::Fatal(
                        "user-agent audit persisted but its event stream closed; do not repeat the operation".to_string(),
                    ));
                }
                drop(active);
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                session.quarantine_history(error.to_string());
            }
            outcome.finished = true;
            result
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("user-agent audit worker failed: {error}")))?
    }

    /// Commits task linkage and policy together, retaining the observer transaction until install.
    ///
    /// `on_ack` compare-installs only the prepared runtime fields. It must not await or acquire a
    /// Session lock. The caller's accepted capability/lifecycle ownership must cover this worker,
    /// including cancellation of its result waiter.
    pub(crate) async fn commit_user_agent_task(
        self: &Arc<Self>,
        observer_txn: OwnedMutexGuard<()>,
        snapshots: Vec<AgentResponseObservation>,
        task: Option<ResponseItem>,
        on_ack: impl FnOnce() -> CodexResult<()> + Send + 'static,
    ) -> CodexResult<()> {
        if snapshots
            .iter()
            .any(|snapshot| snapshot.observer_thread_id != self.thread_id)
        {
            return Err(CodexErr::InvalidRequest(
                "task promotion belongs to another observer".to_string(),
            ));
        }
        let mut records = Vec::new();
        if let Some(task) = &task {
            records.push(RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
            });
            records.push(RolloutItem::ResponseItem(task.clone().into()));
        }
        records.extend(
            snapshots
                .iter()
                .cloned()
                .map(RolloutItem::AgentResponseObservation),
        );
        let evidence = codex_history::committed_user_agent_task_contexts(&records);
        if task.as_ref().is_some_and(|task| {
            !task
                .id()
                .and_then(|id| evidence.get(id))
                .is_some_and(|evidence| evidence.item.item == *task)
        }) {
            return Err(CodexErr::InvalidRequest(
                "task promotion requires its exact adjacent canonical snapshot".to_string(),
            ));
        }
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let _observer_txn = observer_txn;
            let mut outcome = PublicationOutcome {
                session: Arc::clone(&session),
                finished: false,
            };
            let result: CodexResult<()> = async {
                let permit = session.acquire_history_publication_barrier().await?;
                {
                    let response_state = session
                        .response_observation_state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    for (id, proof) in &evidence {
                        if response_state.task_contexts.get(id).is_some_and(|existing| existing.item != proof.item) {
                            return Err(CodexErr::InvalidRequest(
                                "task context identity already has a different canonical payload".to_string(),
                            ));
                        }
                    }
                    for snapshot in &snapshots {
                        if let Some(task) = &snapshot.promoted_task_context
                            && !evidence.contains_key(&task.response_item_id)
                            && !response_state.task_contexts.get(&task.response_item_id).is_some_and(|proof| {
                                codex_protocol::protocol::AgentResponsePromotedTaskContext::from_response_item(&proof.item.item).as_ref() == Some(task)
                            })
                        {
                            return Err(CodexErr::InvalidRequest(
                                "task snapshot has no acknowledged canonical task linkage".to_string(),
                            ));
                        }
                    }
                }
                let turn = session.new_history_only_turn().await;
                let policy = turn.model_info().truncation_policy.into();
                let response_state = Arc::clone(&session.response_observation_state);
                let installation_session = Arc::clone(&session);
                let (installed, installation) = tokio::sync::oneshot::channel();
                let receiver = session.dispatch_completion_publication(
                    permit,
                    records,
                    Vec::new(),
                    move |state| {
                        if let Err(error) = on_ack() {
                            installation_session.quarantine_history(error.to_string());
                            let _ = installed.send(Err(error));
                            return;
                        }
                        if let Some(task) = task
                            && !state.history.raw_items().any(|existing| existing == &task)
                        {
                            state.record_items(std::iter::once(&task), policy);
                        }
                        response_state
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .task_contexts
                            .extend(evidence);
                        let _ = installed.send(Ok(()));
                    },
                    || {},
                )?;
                session.publication_result(receiver).await?;
                installation.await.map_err(|_| CodexErr::InternalAgentDied)??;
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                session.quarantine_history(error.to_string());
            }
            outcome.finished = true;
            result
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("user-agent task worker failed: {error}")))?
    }
}
