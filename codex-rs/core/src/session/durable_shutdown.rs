//! Acknowledged shutdown preserves retry ownership until persistence releases the writer.

use super::SessionIo;
use super::SubmissionAdmission;
use super::handlers;
use super::session::Session;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;

#[derive(Default)]
enum Phase {
    #[default]
    NotStarted,
    ExecutionStopped,
    ProcessesDrained,
    GuardiansDrained,
    RuntimeStopped,
    CompletionsPersisted,
    StopHooksEmitted,
    WriterClosed,
}

/// Progress belongs to the submission loop, never to a request's response receiver.
#[derive(Default)]
pub(super) struct DurableShutdown {
    phase: Phase,
}

impl DurableShutdown {
    pub(super) fn has_started(&self) -> bool {
        !matches!(self.phase, Phase::NotStarted)
    }

    pub(super) async fn run(
        &mut self,
        session: &Arc<Session>,
        submission_id: String,
    ) -> CodexResult<()> {
        if matches!(self.phase, Phase::NotStarted) {
            session
                .services
                .unified_exec_manager
                .begin_durable_shutdown();
            handlers::stop_session_execution(session).await;
            self.phase = Phase::ExecutionStopped;
        }
        if matches!(self.phase, Phase::ExecutionStopped) {
            // Nested dispatch can be waiting for one of the processes being terminated.
            // Settle both owned cleanups; an error in one must not cancel the other.
            let (code_mode, processes) = tokio::join!(
                session.services.code_mode_service.shutdown_durably(),
                session.services.unified_exec_manager.shutdown_durably(),
            );
            code_mode.map_err(|error| {
                CodexErr::Fatal(format!(
                    "drain code-mode execution before shutdown: {error}"
                ))
            })?;
            processes.map_err(|error| {
                CodexErr::Fatal(format!("drain owned execution before shutdown: {error}"))
            })?;
            // No accepted code-mode callback can publish another producer after its
            // acknowledgement. Recheck the manager's final inventory at that boundary.
            session
                .services
                .unified_exec_manager
                .shutdown_durably()
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!(
                        "drain final execution events before shutdown: {error}"
                    ))
                })?;
            self.phase = Phase::ProcessesDrained;
        }
        if matches!(self.phase, Phase::ProcessesDrained) {
            if let Some(guardians) = session
                .services
                .thread_extension_data
                .get::<crate::guardian::GuardianReviewSessionManager>()
            {
                guardians.shutdown_durably().await.map_err(|error| {
                    CodexErr::Fatal(format!(
                        "drain owned guardian sessions before shutdown: {error:#}"
                    ))
                })?;
            }
            self.phase = Phase::GuardiansDrained;
        }
        if matches!(self.phase, Phase::GuardiansDrained) {
            handlers::shutdown_session_services(session).await;
            crate::hook_runtime::run_session_end_hooks(session).await;
            self.phase = Phase::RuntimeStopped;
        }
        if matches!(self.phase, Phase::RuntimeStopped) {
            handlers::persist_completion_mailbox_before_shutdown(session)
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!(
                        "persist accepted completion before shutdown: {error}"
                    ))
                })?;
            self.phase = Phase::CompletionsPersisted;
        }
        if matches!(self.phase, Phase::CompletionsPersisted) {
            handlers::emit_thread_stop_lifecycle(session.as_ref()).await;
            self.phase = Phase::StopHooksEmitted;
        }
        if matches!(self.phase, Phase::StopHooksEmitted) {
            if let Some(live_thread) = session.live_thread() {
                live_thread.shutdown_durably().await.map_err(|error| {
                    CodexErr::Fatal(format!("close durable thread writer: {error}"))
                })?;
            }
            self.phase = Phase::WriterClosed;
            session.submission_admission.acknowledge_writer_closed();
            // The store has removed its recorder and released its writer lease. Merely
            // acknowledging the recorder's I/O task shutdown would be too early.
            session
                .submission_admission
                .durable_shutdown_complete
                .store(true, Ordering::Release);
            let event = Event {
                id: submission_id,
                msg: EventMsg::ShutdownComplete,
            };
            session
                .services
                .rollout_thread_trace
                .record_protocol_event(&event.msg);
            session.deliver_event_raw(event).await;
            session
                .services
                .rollout_thread_trace
                .record_ended(codex_rollout_trace::RolloutStatus::Completed);
        }
        Ok(())
    }
}

impl SessionIo {
    pub(crate) fn durable_shutdown_succeeded(&self) -> bool {
        let mut endpoint = self;
        while let Some(target) = &endpoint.submission_admission.durable_shutdown_target {
            endpoint = target;
        }
        endpoint
            .submission_admission
            .durable_shutdown_complete
            .load(Ordering::Acquire)
    }

    pub(crate) async fn shutdown_durably_and_wait(&self) -> CodexResult<()> {
        // A delegate's public operation forwarder may already have been canceled. Keep
        // lifecycle ownership routed to the actual actor, including its admission/latch.
        let mut endpoint = self;
        while let Some(target) = &endpoint.submission_admission.durable_shutdown_target {
            endpoint = target;
        }
        if !endpoint
            .submission_admission
            .durable_shutdown_complete
            .load(Ordering::Acquire)
        {
            let (reply, result) = oneshot::channel();
            let outcome = match endpoint.submit(Op::ShutdownDurably { reply }).await {
                Ok(_) => tokio::select! {
                    result = result => result.unwrap_or(Err(CodexErr::InternalAgentDied)),
                    () = endpoint.session_loop_termination.clone() => {
                        // Another queued shutdown may have stopped the actor. Its unread
                        // reply sender can remain in the channel while tx_sub is retained.
                        // Actor exit alone is not proof that the writer was closed.
                        Err(CodexErr::Fatal(
                            "session terminated without acknowledging durable shutdown".to_string(),
                        ))
                    }
                },
                Err(error) => Err(error),
            };
            if let Err(error) = outcome
                && !endpoint
                    .submission_admission
                    .durable_shutdown_complete
                    .load(Ordering::Acquire)
            {
                return Err(error);
            }
        }
        endpoint.session_loop_termination.clone().await;
        if endpoint
            .submission_admission
            .durable_shutdown_complete
            .load(Ordering::Acquire)
        {
            Ok(())
        } else {
            Err(CodexErr::Fatal(
                "session terminated without acknowledging durable shutdown".to_string(),
            ))
        }
    }
}

impl SubmissionAdmission {
    /// Fence ordinary work and ownership mutations without excluding accepted completions.
    pub(crate) fn seal_for_unload(&self) {
        self.subtree_unload_pending
            .store(/*val*/ true, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub(crate) fn is_sealed_for_unload(&self) -> bool {
        self.subtree_unload_pending.load(Ordering::Acquire)
    }

    pub(crate) fn forwarding_to(target: Arc<SessionIo>) -> Self {
        Self {
            durable_shutdown_target: Some(target),
            ..Self::default()
        }
    }
}

#[cfg(test)]
#[path = "durable_shutdown_tests.rs"]
mod tests;
