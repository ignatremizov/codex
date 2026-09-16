//! Owns provisional startup and ordered graph cleanup for one exact child runtime.
//!
//! An interrupted input attempt is not a rejection. Preserve that runtime for reconciliation
//! instead of deleting history or closing a child which may already be doing accepted work.

use super::LocalAgentControl;
use super::aliases::PersistedAgentSpawn;
use crate::codex_thread::CodexThread;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use std::sync::Arc;
use tokio::sync::OwnedMutexGuard;
use tokio::task::JoinHandle;
use tracing::warn;

pub(super) struct PendingSpawn {
    control: LocalAgentControl,
    state: Arc<ThreadManagerState>,
    child: Option<Arc<CodexThread>>,
    parent_guard: Option<OwnedMutexGuard<()>>,
    child_guard: Option<OwnedMutexGuard<()>>,
    edge_write: Option<JoinHandle<CodexResult<PersistedAgentSpawn>>>,
    graph_published: bool,
    graph_uncertain: bool,
    input_pending: bool,
}

impl PendingSpawn {
    pub(super) fn new(
        control: LocalAgentControl,
        state: Arc<ThreadManagerState>,
        child: Arc<CodexThread>,
        parent_guard: Option<OwnedMutexGuard<()>>,
    ) -> Self {
        Self {
            control,
            state,
            child: Some(child),
            parent_guard,
            child_guard: None,
            edge_write: None,
            graph_published: false,
            graph_uncertain: false,
            input_pending: false,
        }
    }

    pub(super) fn set_edge_write(
        &mut self,
        edge_write: JoinHandle<CodexResult<PersistedAgentSpawn>>,
    ) {
        self.graph_uncertain = true;
        self.edge_write = Some(edge_write);
    }

    pub(super) async fn wait_for_edge(&mut self) -> CodexResult<PersistedAgentSpawn> {
        let Some(edge_write) = self.edge_write.as_mut() else {
            return Err(CodexErr::Fatal(
                "spawn has no graph publication worker".into(),
            ));
        };
        let result = edge_write.await;
        // A completed JoinHandle must never be polled again by rollback or Drop.
        self.edge_write = None;
        let persisted = result.map_err(|error| {
            CodexErr::Fatal(format!("spawn graph publication worker failed: {error}"))
        })??;
        self.graph_published = true;
        self.graph_uncertain = false;
        Ok(persisted)
    }

    pub(super) fn begin_input_attempt(&mut self) {
        self.input_pending = true;
    }

    pub(super) fn input_rejected(&mut self) {
        self.input_pending = false;
    }

    pub(super) fn input_settled(&mut self) {
        self.input_pending = false;
    }

    pub(super) fn set_child_guard(&mut self, guard: OwnedMutexGuard<()>) {
        self.child_guard = Some(guard);
    }

    pub(super) fn release_input_gate(&mut self) {
        self.child_guard.take();
    }

    pub(super) async fn reacquire_input_gate(&mut self) -> CodexResult<()> {
        let Some(child) = self.child.as_ref() else {
            return Ok(());
        };
        let id = child.session.thread_id();
        self.child_guard = Some(self.state.agent_lifecycle_lock(id).lock_owned().await);
        if !self
            .state
            .get_thread(id)
            .await
            .is_ok_and(|current| Arc::ptr_eq(&current, child))
        {
            return Err(CodexErr::InvalidRequest(
                "spawned runtime changed before handoff".into(),
            ));
        }
        child.ensure_not_unloading()?;
        if !child.is_running() {
            return Err(CodexErr::InvalidRequest(
                "spawned runtime stopped before handoff".into(),
            ));
        }
        Ok(())
    }

    /// Consuming the prepared receipt, not sending it, transfers cleanup ownership.
    pub(super) fn commit(mut self) {
        if let Some(child) = self.child.take() {
            child
                .cancelled_spawn_alias_cleanup_pending
                .store(false, std::sync::atomic::Ordering::Release);
            self.state.notify_thread_created(child.session.thread_id());
        }
    }

    /// The caller has either not attempted input or received a definite rejection.
    pub(super) async fn rollback(mut self, error: CodexErr) -> CodexErr {
        let Some(child) = self.child.take() else {
            return error;
        };
        self.cleanup(child, error).await
    }

    async fn cleanup(&mut self, child: Arc<CodexThread>, error: CodexErr) -> CodexErr {
        if self.input_pending {
            child.session.quarantine_history(format!(
                "spawn input outcome is unknown: {error}; reconcile before retrying"
            ));
            return error;
        }
        // Join the original Open write before attempting Closed. A failed acknowledgement
        // leaves graph state uncertain; do not compensate or discard its source history.
        if let Some(edge_write) = self.edge_write.take() {
            self.graph_published = matches!(edge_write.await, Ok(Ok(_)));
            self.graph_uncertain = !self.graph_published;
        }
        let id = child.session.thread_id();
        if self.child_guard.is_none() {
            self.child_guard = Some(self.state.agent_lifecycle_lock(id).lock_owned().await);
        }
        if self
            .state
            .get_thread(id)
            .await
            .is_ok_and(|current| !Arc::ptr_eq(&current, &child))
        {
            child
                .session
                .quarantine_history("provisional child was replaced during cleanup".into());
            return CodexErr::Fatal(format!(
                "{error}; provisional child was replaced; history retained"
            ));
        }
        if self.graph_published
            && !self.graph_uncertain
            && child.session_source.parent_thread_id().is_some()
            && !child.config_snapshot().await.ephemeral
        {
            child
                .cancelled_spawn_alias_cleanup_pending
                .store(true, std::sync::atomic::Ordering::Release);
        }
        child.session.submission_admission.seal_for_unload();
        let disarm = child.session.disarm_terminal_presentation();
        child
            .session
            .retire_agent_status_observers(crate::session::AgentStatusRetirement::RestoreRollback);
        // Both obligations must settle. A writer failure cannot erase the independent
        // exact alias-close obligation, and an uncertain Open write never creates one.
        let shutdown = child.shutdown_durably_and_wait().await;
        let alias_cleanup = self
            .control
            .finish_cancelled_spawn_alias_cleanup(&child)
            .await;
        disarm.commit();
        if shutdown.is_err() || alias_cleanup.is_err() {
            let retained = self.state.retain_failed_spawn_cleanup(&child).await;
            return CodexErr::Fatal(format!(
                "{error}; spawned runtime cleanup incomplete; shutdown: {shutdown:?}; alias cleanup: {alias_cleanup:?}; unload-retry retention: {retained:?}"
            ));
        }
        let mut error = error;
        // An unpublished, definitively rejected setup may discard its history. A published
        // or uncertain graph keeps the original audit source, even after its writer closes.
        if !self.graph_published
            && !self.graph_uncertain
            && let Some(live_thread) = child.session.live_thread()
            && let Err(discard) = live_thread.discard().await
        {
            error = CodexErr::Fatal(format!(
                "{error}; rejected spawn history cleanup failed: {discard}"
            ));
        }
        let removed = self
            .state
            .remove_thread_with_authority(
                &id,
                &child,
                crate::thread_manager::ThreadRemovalAuthority::DurableUnload,
                || {
                    self.control.forget_v2_residency(id);
                    self.control.runtime.registry.release_spawned_thread(id);
                },
            )
            .await;
        if removed.is_none() {
            self.state
                .run_if_thread_absent(id, || {
                    self.control.forget_v2_residency(id);
                    self.control.runtime.registry.release_spawned_thread(id);
                })
                .await;
        }
        error
    }
}

impl Drop for PendingSpawn {
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        if self.input_pending {
            child.session.quarantine_history(
                "spawn worker ended during input admission; input outcome unknown; do not resubmit"
                    .into(),
            );
            return;
        }
        let mut cleanup = Self {
            control: self.control.clone(),
            state: Arc::clone(&self.state),
            child: None,
            parent_guard: self.parent_guard.take(),
            child_guard: self.child_guard.take(),
            edge_write: self.edge_write.take(),
            graph_published: self.graph_published,
            graph_uncertain: self.graph_uncertain,
            input_pending: false,
        };
        drop(tokio::spawn(async move {
            let error = cleanup
                .cleanup(child, CodexErr::Fatal("spawn setup was abandoned".into()))
                .await;
            warn!("{error}");
        }));
    }
}
