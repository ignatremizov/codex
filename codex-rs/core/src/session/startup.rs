//! Retains persistence acquisition and resources throughout managed startup.
//! The thread manager drops initialization, then joins acquisition and resource cleanup here.

use std::sync::Arc;
use std::sync::OnceLock;

use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Op;
use codex_thread_store::LiveThreadInitGuard;
use tokio::sync::Mutex;

use super::SessionIo;
use super::session::Session;

#[derive(Default)]
pub(crate) struct SessionStartup {
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
    pub(crate) async fn cleanup(&self) {
        if let Some(io) = self.io.get() {
            // The session loop owns persistence now. Preserve its shutdown semantics even
            // if registration or the caller's handoff was interrupted after the loop started.
            self.persistence.lock().await.commit();
            let _ = io.submit(Op::Interrupt).await;
            let _ = io.shutdown_and_wait().await;
        } else {
            if let Some(session) = self.session.get() {
                super::handlers::shutdown_session_runtime(session).await;
            }
            let mut persistence = std::mem::take(&mut *self.persistence.lock().await);
            persistence.discard().await;
        }
    }

    /// Managed lifetimes retain this owner and retry failures until writer release is proven.
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
        Ok(())
    }
}
