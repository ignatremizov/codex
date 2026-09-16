//! Retains agent-tree membership and resources throughout managed startup.
//! Cleanup releases membership only after acquisition and partial session teardown finish;
//! abandoning that ownership marks tree shutdown as failed.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::sync::PoisonError;

use crate::agent::control::AgentTreeMembership;
use crate::agent::control::AgentTreeTeardownGuard;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrKind;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Op;
use codex_thread_store::LiveThreadInitGuard;
use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::sync::Mutex;

use super::SessionIo;
use super::session::Session;

#[derive(Default)]
pub(crate) struct SessionStartup {
    teardown: StdMutex<Option<AgentTreeTeardownGuard>>,
    pub(crate) persistence: Mutex<LiveThreadInitGuard>,
    pub(crate) session: OnceLock<Arc<Session>>,
    pub(crate) io: OnceLock<SessionIo>,
    cleanup: Mutex<CleanupPhase>,
}

#[derive(Default)]
enum CleanupPhase {
    #[default]
    NotStarted,
    ExecutionStopped,
    ProcessesDrained,
    GuardiansDrained,
    ServicesStopped,
    Complete,
}

impl SessionStartup {
    fn lock_teardown(&self) -> MutexGuard<'_, Option<AgentTreeTeardownGuard>> {
        self.teardown.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn hold_membership(&self, membership: AgentTreeMembership) {
        let teardown = membership.into_teardown_guard("session_startup", /*thread_id*/ None);
        let previous = self.lock_teardown().replace(teardown);
        assert!(previous.is_none(), "agent-tree membership already set");
    }

    pub(crate) fn set_session(&self, session: Arc<Session>) {
        let thread_id = session.thread_id;
        let mut teardown = self.lock_teardown();
        if self.session.set(session).is_ok()
            && let Some(teardown) = teardown.as_mut()
        {
            teardown.set_thread_id(thread_id);
        }
    }

    pub(crate) fn session_teardown(&self, thread_id: ThreadId) -> Option<AgentTreeTeardownGuard> {
        let teardown = self.lock_teardown();
        let teardown = teardown.as_ref()?;
        Some(teardown.clone_for_teardown("session_loop", Some(thread_id)))
    }

    pub(crate) fn release_membership(&self) {
        if let Some(teardown) = self.lock_teardown().take() {
            teardown.complete();
        }
    }

    pub(crate) async fn cleanup(self: &Arc<Self>) {
        let startup = Arc::clone(self);
        let (settled, first_attempt) = tokio::sync::oneshot::channel();
        // Cleanup owns the original membership and writer even if this waiter is dropped.
        // A failed attempt may return the original startup error while cleanup stays owned.
        drop(tokio::spawn(async move {
            let mut settled = Some(settled);
            loop {
                let result = std::panic::AssertUnwindSafe(startup.cleanup_durably())
                    .catch_unwind()
                    .await
                    .unwrap_or(Err(CodexErr::InternalAgentDied));
                match result {
                    Ok(()) => {
                        if let Some(settled) = settled.take() {
                            let _ = settled.send(());
                        }
                        break;
                    }
                    Err(error) => {
                        if let Some(teardown) = startup.lock_teardown().as_ref() {
                            teardown.record_shutdown_failure(
                                "durable_startup_cleanup",
                                CodexErrKind::from(&error).into(),
                            );
                        }
                        tracing::warn!(%error, "cancelled startup cleanup failed; retaining owner");
                        if let Some(settled) = settled.take() {
                            let _ = settled.send(());
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(/*secs*/ 1)).await;
                    }
                }
            }
        }));
        if let Err(error) = first_attempt.await {
            tracing::warn!(%error, "startup cleanup worker failed");
        }
    }

    /// Managed lifetimes retain this owner and retry failures until writer release is proven.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "cleanup phases must serialize through their acknowledgements, and persistence must retain the acquisition/writer across failed or cancelled durable discard for retry"
    )]
    pub(crate) async fn cleanup_durably(&self) -> CodexResult<()> {
        let mut phase = self.cleanup.lock().await;
        if matches!(*phase, CleanupPhase::Complete) {
            return Ok(());
        }
        if let Some(io) = self.io.get() {
            self.persistence.lock().await.commit();
            let _ = io.submit(Op::Interrupt).await;
            io.shutdown_durably_and_wait().await?;
            *phase = CleanupPhase::Complete;
            self.release_membership();
            return Ok(());
        }
        // Before the submission loop exists, initialization still owns the writer.
        // Drain producers without materializing a failed/lazy session's history.
        if let Some(session) = self.session.get() {
            if matches!(*phase, CleanupPhase::NotStarted) {
                session
                    .services
                    .unified_exec_manager
                    .begin_durable_shutdown();
                super::handlers::stop_session_execution(session).await;
                *phase = CleanupPhase::ExecutionStopped;
            }
            if matches!(*phase, CleanupPhase::ExecutionStopped) {
                let (code_mode, processes) = tokio::join!(
                    session.services.code_mode_service.shutdown_durably(),
                    session.services.unified_exec_manager.shutdown_durably(),
                );
                code_mode.map_err(CodexErr::Fatal)?;
                processes.map_err(|error| CodexErr::Fatal(error.to_string()))?;
                // Accepted callbacks can publish producers while the first drain runs.
                // Once code-mode acknowledges, its final inventory cannot grow again.
                session
                    .services
                    .unified_exec_manager
                    .shutdown_durably()
                    .await
                    .map_err(|error| CodexErr::Fatal(error.to_string()))?;
                *phase = CleanupPhase::ProcessesDrained;
            }
            if matches!(*phase, CleanupPhase::ProcessesDrained) {
                if let Some(guardians) = session
                    .services
                    .thread_extension_data
                    .get::<crate::guardian::GuardianReviewSessionManager>()
                {
                    guardians
                        .shutdown_durably()
                        .await
                        .map_err(|error| CodexErr::Fatal(format!("{error:#}")))?;
                }
                *phase = CleanupPhase::GuardiansDrained;
            }
            if matches!(*phase, CleanupPhase::GuardiansDrained) {
                super::handlers::shutdown_session_services(session).await;
                crate::hook_runtime::run_session_end_hooks(session).await;
                super::handlers::emit_thread_stop_lifecycle(session).await;
                *phase = CleanupPhase::ServicesStopped;
            }
        }
        self.persistence
            .lock()
            .await
            .discard_durably()
            .await
            .map_err(|error| CodexErr::Fatal(format!("discard managed startup writer: {error}")))?;
        *phase = CleanupPhase::Complete;
        self.release_membership();
        Ok(())
    }
}

/// Cleans up a cancelled session start before releasing its agent-tree membership.
pub(crate) struct SessionStartupGuard {
    startup: Option<Arc<SessionStartup>>,
}

impl SessionStartupGuard {
    pub(crate) fn new(startup: Arc<SessionStartup>) -> Self {
        Self {
            startup: Some(startup),
        }
    }

    pub(crate) fn disarm(mut self) {
        self.startup = None;
    }

    pub(crate) fn cleanup(mut self) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            if let Some(startup) = self.startup.take() {
                startup.cleanup().await;
            }
        })
    }
}

impl Drop for SessionStartupGuard {
    fn drop(&mut self) {
        let Some(startup) = self.startup.take() else {
            return;
        };
        drop(tokio::spawn(async move {
            startup.cleanup().await;
        }));
    }
}
