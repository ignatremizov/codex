//! Independently driven publication at the existing session history/settings boundary.
//!
//! Accepted workers own their permit until completion. They never retain Session.
//! An append error can follow canonical commit: failure requires canonical reload,
//! never another append of the same batch.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_extension_api::PostCompactionContextContribution;
use codex_extension_api::TurnInputContributionAcknowledgement;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::oneshot;
use tokio_util::task::TaskTracker;

use super::Session;
use super::thread_settings;
use crate::state::SessionState;

pub(super) struct PublicationBatch {
    pub(super) rollout: Vec<RolloutItem>,
    pub(super) events: Vec<codex_protocol::protocol::Event>,
    pub(super) raw_event_guardian_thread_id: Option<codex_protocol::ThreadId>,
}

#[derive(Default)]
pub(super) struct HistoryPublication {
    tasks: TaskTracker,
    failure: Arc<Mutex<Option<String>>>,
    closing: AtomicBool,
}

impl HistoryPublication {
    pub(super) fn check(&self) -> CodexResult<()> {
        if let Some(failure) = self
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            return Err(CodexErr::Fatal(failure.clone()));
        }
        if self.closing.load(Ordering::Acquire) {
            return Err(CodexErr::Fatal(
                "history publication is closing".to_string(),
            ));
        }
        Ok(())
    }
}

/// Records abandonment/panic as failure even if the result receiver disappeared.
struct PublicationOutcome {
    failure: Arc<Mutex<Option<String>>>,
    stage: &'static str,
    finished: bool,
    reload: Option<Arc<super::SubmissionAdmission>>,
}

impl PublicationOutcome {
    fn fail(&mut self, error: impl std::fmt::Display) -> CodexErr {
        if let Some(admission) = &self.reload {
            admission.rollback_requires_reload();
        }
        let message = format!(
            "history publication failed during {}: {error}; canonical reload required",
            self.stage
        );
        *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(message.clone());
        self.finished = true;
        CodexErr::Fatal(message)
    }
}

impl Drop for PublicationOutcome {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.fail("publication worker abandoned");
        }
    }
}

impl Session {
    /// Completion records use single-attempt writer barriers, never the ordinary retry queue.
    pub(super) fn dispatch_completion_publication(
        &self,
        permit: OwnedSemaphorePermit,
        items: Vec<RolloutItem>,
        events: Vec<codex_protocol::protocol::Event>,
        install: impl FnOnce(&mut SessionState) + Send + 'static,
        on_primary_delivery: impl FnOnce() + Send + 'static,
    ) -> CodexResult<
        oneshot::Receiver<CodexResult<super::sub_agent_completion::CanonicalCompletionReceipt>>,
    > {
        self.history_publication.check()?;
        let live = self.live_thread().cloned().ok_or_else(|| {
            CodexErr::InvalidRequest(
                "completion publication requires canonical persistence".to_string(),
            )
        })?;
        let state = Arc::clone(&self.state);
        let failure = Arc::clone(&self.history_publication.failure);
        let admission = Arc::clone(&self.submission_admission);
        let tx_event = self.tx_event.clone();
        let (sender, receiver) = oneshot::channel();
        self.history_publication.tasks.spawn(async move {
            let _permit = permit;
            let mut outcome = PublicationOutcome {
                failure,
                stage: "completion append",
                finished: false,
                reload: Some(Arc::clone(&admission)),
            };
            let result = live
                .append_completion_items_and_flush_canonical(&items)
                .await;
            if let Err(error) = result {
                admission.rollback_requires_reload();
                let _ = sender.send(Err(outcome.fail(error)));
                return;
            }
            outcome.stage = "completion live installation";
            {
                let mut state = state.lock().await;
                install(&mut state);
            }
            let mut primary_event = super::sub_agent_completion::PrimaryEventEnqueue::Enqueued;
            for event in events {
                if tx_event.send(event).await.is_err() {
                    primary_event = super::sub_agent_completion::PrimaryEventEnqueue::Closed;
                }
            }
            if primary_event == super::sub_agent_completion::PrimaryEventEnqueue::Enqueued {
                on_primary_delivery();
            }
            outcome.finished = true;
            let _ = sender.send(Ok(
                super::sub_agent_completion::CanonicalCompletionReceipt { primary_event },
            ));
        });
        Ok(receiver)
    }

    pub(crate) fn quarantine_history(&self, reason: String) {
        self.submission_admission.rollback_requires_reload();
        self.history_publication
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| format!("{reason}; canonical reload required"));
    }

    pub(crate) fn check_history_publication(&self) -> CodexResult<()> {
        self.history_publication.check()
    }

    /// Waits for accepted workers without closing admission. Abort the active task first.
    pub(crate) async fn await_history_publication(&self) {
        let _permit = self.reserve_history_publication().await;
    }

    pub(crate) async fn reserve_history_publication(&self) -> OwnedSemaphorePermit {
        thread_settings::acquire_persistence_lock(self).await
    }

    pub(crate) async fn acquire_history_publication_barrier(
        &self,
    ) -> CodexResult<OwnedSemaphorePermit> {
        let permit = self.reserve_history_publication().await;
        self.check_history_publication()?;
        Ok(permit)
    }

    pub(super) async fn close_history_publication(&self) {
        {
            let _permit = thread_settings::acquire_persistence_lock(self).await;
            self.history_publication
                .closing
                .store(true, Ordering::Release);
            self.history_publication.tasks.close();
        }
        self.history_publication.tasks.wait().await;
    }

    /// Dispatch is synchronous while retaining the validated publication permit.
    /// Only the receiver is cancellable; accepted work is driven by the task tracker.
    /// Its receipt confirms local publication, never observation by a client.
    pub(super) fn dispatch_history_publication<T: Send + 'static>(
        &self,
        permit: OwnedSemaphorePermit,
        items: Vec<RolloutItem>,
        leases: Vec<PostCompactionContextContribution>,
        acknowledgement: Option<TurnInputContributionAcknowledgement>,
        install: impl FnOnce(&mut SessionState) -> T + Send + 'static,
    ) -> CodexResult<oneshot::Receiver<CodexResult<T>>> {
        self.dispatch_history_publication_with_events(
            permit,
            PublicationBatch {
                rollout: items,
                events: Vec::new(),
                raw_event_guardian_thread_id: None,
            },
            leases,
            acknowledgement,
            install,
        )
    }

    pub(super) fn dispatch_history_publication_with_events<T: Send + 'static>(
        &self,
        permit: OwnedSemaphorePermit,
        batch: PublicationBatch,
        leases: Vec<PostCompactionContextContribution>,
        acknowledgement: Option<TurnInputContributionAcknowledgement>,
        install: impl FnOnce(&mut SessionState) -> T + Send + 'static,
    ) -> CodexResult<oneshot::Receiver<CodexResult<T>>> {
        self.history_publication.check()?;
        if leases.iter().any(|contribution| !contribution.is_current()) {
            return Err(CodexErr::Fatal(
                "checkpoint contribution was invalidated before publication".to_string(),
            ));
        }
        let live_thread = self.live_thread().cloned();
        let state = Arc::clone(&self.state);
        let failure = Arc::clone(&self.history_publication.failure);
        let event_sender = self.tx_event.clone();
        let trace = self.services.rollout_thread_trace.clone();
        let mcp_runtime = Arc::clone(&self.services.mcp_runtime);
        let analytics = self.services.analytics_events_client.clone();
        let (sender, receiver) = oneshot::channel();
        self.history_publication.tasks.spawn(async move {
            let PublicationBatch {
                rollout: items,
                events,
                raw_event_guardian_thread_id,
            } = batch;
            let _permit = permit;
            let _leases = leases;
            let mut outcome = PublicationOutcome {
                failure,
                stage: "append",
                finished: false,
                reload: None,
            };
            if !items.is_empty()
                && let Some(live_thread) = live_thread
            {
                if let Err(error) = live_thread.append_items_and_flush_canonical(&items).await {
                    let _ = sender.send(Err(outcome.fail(error)));
                    return;
                }
            }
            outcome.stage = "live installation";
            let result = {
                let mut state = state.lock().await;
                install(&mut state)
            };
            outcome.stage = "acknowledgment";
            if let Some(acknowledgement) = acknowledgement {
                acknowledgement.acknowledge();
            }
            for event in events {
                if matches!(
                    &event.msg,
                    codex_protocol::protocol::EventMsg::RawResponseItem(_)
                ) {
                    // Raw response events have no lifecycle, realtime or legacy-event effects.
                    // Their response item was committed above; never append it a second time.
                    trace.record_codex_turn_event(&event.id, &event.msg);
                    trace.record_tool_call_event(event.id.clone(), &event.msg);
                    if let Some(thread_id) = raw_event_guardian_thread_id {
                        analytics.track_guardian_session_event(thread_id, &event);
                    }
                    mcp_runtime.observe_event(&event.msg);
                }
                trace.record_protocol_event(&event.msg);
                if let Err(error) = event_sender.send(event).await {
                    // Persistence succeeds even when its former event consumer has gone away.
                    tracing::debug!("dropping event because channel is closed: {error}");
                }
            }
            outcome.finished = true;
            let _ = sender.send(Ok(result));
        });
        Ok(receiver)
    }

    pub(super) async fn publication_result<T>(
        &self,
        receiver: oneshot::Receiver<CodexResult<T>>,
    ) -> CodexResult<T> {
        match receiver.await {
            Ok(result) => result,
            Err(_) => {
                self.check_history_publication()?;
                Err(CodexErr::Fatal(
                    "history publication lost its completion result".to_string(),
                ))
            }
        }
    }
}
