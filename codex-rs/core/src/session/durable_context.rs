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
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::oneshot;
use tokio_util::task::TaskTracker;

use super::Session;
use super::completion_provenance::CompletionPublicationReceipt;
use super::thread_settings;
use crate::state::SessionState;

pub(super) struct PublicationBatch {
    pub(super) rollout: Vec<RolloutItem>,
    pub(super) events: Vec<codex_protocol::protocol::Event>,
    /// A settings caller may hold a lock needed by the bounded event consumer.
    /// Reply after publication, but before delivering the corresponding live event.
    pub(super) reply: Option<oneshot::Sender<CodexResult<()>>>,
    pub(super) raw_event_guardian_thread_id: Option<codex_protocol::ThreadId>,
}

#[derive(Clone, Default)]
pub(super) struct HistoryPublication {
    tasks: TaskTracker,
    failure: Arc<Mutex<Option<String>>>,
    closing: Arc<AtomicBool>,
}

/// Storage, state, and notification owners needed by accepted work, without Session.
pub(super) struct HistoryPublicationHandle {
    publication: HistoryPublication,
    submission_admission: Arc<super::SubmissionAdmission>,
    live_thread: Option<codex_thread_store::LiveThread>,
    state: Arc<tokio::sync::Mutex<SessionState>>,
    event_sender: async_channel::Sender<codex_protocol::protocol::Event>,
    trace: codex_rollout_trace::ThreadTraceContext,
    mcp_runtime: Arc<codex_mcp::McpRuntime>,
    analytics: codex_analytics::AnalyticsEventsClient,
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
    reply: Option<oneshot::Sender<CodexResult<()>>>,
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
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err(CodexErr::Fatal(message.clone())));
        }
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
    /// Persistent completions use single-attempt writer barriers, never the retry queue.
    /// Ephemeral completions share the owned installation/event boundary without a disk receipt.
    pub(super) fn dispatch_completion_publication(
        &self,
        permit: OwnedSemaphorePermit,
        items: Vec<RolloutItem>,
        events: Vec<codex_protocol::protocol::Event>,
        install: impl FnOnce(&mut SessionState) + Send + 'static,
        on_primary_delivery: impl FnOnce() + Send + 'static,
    ) -> CodexResult<oneshot::Receiver<CodexResult<CompletionPublicationReceipt>>> {
        self.history_publication.check()?;
        let live = self.live_thread().cloned();
        let state = Arc::clone(&self.state);
        let failure = Arc::clone(&self.history_publication.failure);
        let admission = Arc::clone(&self.submission_admission);
        let tx_event = self.tx_event.clone();
        let trace = self.services.rollout_thread_trace.clone();
        let mcp_runtime = Arc::clone(&self.services.mcp_runtime);
        let analytics = self.services.analytics_events_client.clone();
        let thread_id = self.thread_id;
        let show_raw_agent_reasoning = self.show_raw_agent_reasoning();
        let (sender, receiver) = oneshot::channel();
        // Retain abandonment evidence even when the owned worker is never polled.
        let outcome = PublicationOutcome {
            failure,
            reply: None,
            stage: "completion append",
            finished: false,
            reload: Some(Arc::clone(&admission)),
        };
        self.history_publication.tasks.spawn(async move {
            let _permit = permit;
            let mut outcome = outcome;
            if let Some(live) = &live
                && let Err(error) = live
                    .append_completion_items_and_flush_canonical(&items)
                    .await
            {
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
            let trusted_guardian = {
                let state = state.lock().await;
                let configuration = &state.session_configuration;
                configuration.trusted_guardian_reviewer
                    && configuration.parent_thread_id.is_some()
                    && analytics.is_enabled()
                    && matches!(&configuration.session_source,
                        SessionSource::SubAgent(SubAgentSource::Other(name))
                            if name == crate::guardian::GUARDIAN_REVIEWER_NAME)
            };
            let mut legacy_events = Vec::new();
            for event in events {
                // These are item receipts, not new parent turns or final answers. Keep
                // observers and legacy wait notifications without a second canonical append.
                trace.record_codex_turn_event(&event.id, &event.msg);
                trace.record_tool_call_event(event.id.clone(), &event.msg);
                trace.record_protocol_event(&event.msg);
                mcp_runtime.observe_event(&event.msg);
                if trusted_guardian {
                    analytics.track_guardian_session_event(thread_id, &event);
                }
                if matches!(&event.msg, EventMsg::ItemCompleted(completed)
                    if matches!(&completed.item, codex_protocol::items::TurnItem::CollabAgentToolCall(_)))
                {
                    legacy_events.extend(event.msg.as_legacy_events(show_raw_agent_reasoning)
                        .into_iter().map(|msg| Event { id: event.id.clone(), msg }));
                }
                if tx_event.send(event).await.is_err() {
                    primary_event = super::sub_agent_completion::PrimaryEventEnqueue::Closed;
                }
            }
            if primary_event == super::sub_agent_completion::PrimaryEventEnqueue::Enqueued {
                on_primary_delivery();
            }
            for event in legacy_events {
                trace.record_tool_call_event(event.id.clone(), &event.msg);
                trace.record_protocol_event(&event.msg);
                mcp_runtime.observe_event(&event.msg);
                let _ = tx_event.send(event).await;
            }
            outcome.finished = true;
            let receipt = if live.is_some() {
                CompletionPublicationReceipt::Canonical { primary_event }
            } else {
                CompletionPublicationReceipt::RuntimeOnly { primary_event }
            };
            let _ = sender.send(Ok(receipt));
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
                reply: None,
                raw_event_guardian_thread_id: None,
            },
            leases,
            acknowledgement,
            install,
        )
    }

    pub(super) fn history_publication_handle(&self) -> HistoryPublicationHandle {
        HistoryPublicationHandle {
            publication: self.history_publication.clone(),
            submission_admission: Arc::clone(&self.submission_admission),
            live_thread: self.live_thread().cloned(),
            state: Arc::clone(&self.state),
            event_sender: self.tx_event.clone(),
            trace: self.services.rollout_thread_trace.clone(),
            mcp_runtime: Arc::clone(&self.services.mcp_runtime),
            analytics: self.services.analytics_events_client.clone(),
        }
    }

    pub(super) fn dispatch_history_publication_with_events<T: Send + 'static>(
        &self,
        permit: OwnedSemaphorePermit,
        batch: PublicationBatch,
        leases: Vec<PostCompactionContextContribution>,
        acknowledgement: Option<TurnInputContributionAcknowledgement>,
        install: impl FnOnce(&mut SessionState) -> T + Send + 'static,
    ) -> CodexResult<oneshot::Receiver<CodexResult<T>>> {
        self.history_publication_handle()
            .dispatch_history_publication_with_events(
                permit,
                batch,
                leases,
                acknowledgement,
                install,
            )
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

impl HistoryPublicationHandle {
    pub(super) fn dispatch_history_publication_with_events<T: Send + 'static>(
        &self,
        permit: OwnedSemaphorePermit,
        mut batch: PublicationBatch,
        leases: Vec<PostCompactionContextContribution>,
        acknowledgement: Option<TurnInputContributionAcknowledgement>,
        install: impl FnOnce(&mut SessionState) -> T + Send + 'static,
    ) -> CodexResult<oneshot::Receiver<CodexResult<T>>> {
        let admission = self.publication.check().and_then(|()| {
            if leases.iter().any(|contribution| !contribution.is_current()) {
                Err(CodexErr::Fatal(
                    "checkpoint contribution was invalidated before publication".to_string(),
                ))
            } else {
                Ok(())
            }
        });
        if let Err(error) = admission {
            if let Some(reply) = batch.reply.take() {
                let _ = reply.send(Err(CodexErr::Fatal(error.to_string())));
            }
            return Err(error);
        }
        let live_thread = self.live_thread.clone();
        let state = Arc::clone(&self.state);
        let failure = Arc::clone(&self.publication.failure);
        let event_sender = self.event_sender.clone();
        let trace = self.trace.clone();
        let mcp_runtime = Arc::clone(&self.mcp_runtime);
        let analytics = self.analytics.clone();
        let (sender, receiver) = oneshot::channel();
        // Capture abandonment before scheduling, including a worker never polled.
        let outcome = PublicationOutcome {
            failure,
            reply: batch.reply.take(),
            stage: "append",
            finished: false,
            // The accepted worker owns quarantine even if its caller drops the receipt.
            reload: Some(Arc::clone(&self.submission_admission)),
        };
        self.publication.tasks.spawn(async move {
            let PublicationBatch {
                rollout: items,
                events,
                reply: _,
                raw_event_guardian_thread_id,
            } = batch;
            let _permit = permit;
            let _leases = leases;
            // Once polled, publish panic/abandonment before releasing the permit.
            // Keeping the guard captured above also covers never-polled workers.
            let mut outcome = outcome;
            if !items.is_empty()
                && let Some(live_thread) = live_thread
                && let Err(error) = live_thread.append_items_and_flush_canonical(&items).await
            {
                let _ = sender.send(Err(outcome.fail(error)));
                return;
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
            if let Some(reply) = outcome.reply.take() {
                let _ = reply.send(Ok(()));
            }
            for event in events {
                if matches!(
                    &event.msg,
                    codex_protocol::protocol::EventMsg::RawResponseItem(_)
                ) {
                    // These response items are already canonical. Preserve observer effects
                    // without re-appending or synthesizing a runtime turn lifecycle.
                    trace.record_codex_turn_event(&event.id, &event.msg);
                    trace.record_tool_call_event(event.id.clone(), &event.msg);
                    if let Some(thread_id) = raw_event_guardian_thread_id {
                        analytics.track_guardian_session_event(thread_id, &event);
                    }
                    mcp_runtime.observe_event(&event.msg);
                }
                trace.record_protocol_event(&event.msg);
                if let Err(error) = event_sender.send(event).await {
                    tracing::debug!("dropping event because channel is closed: {error}");
                }
            }
            outcome.finished = true;
            let _ = sender.send(Ok(result));
        });
        Ok(receiver)
    }
}
