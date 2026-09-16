//! Owns provisional startup and ordered graph cleanup for one exact child runtime.
//!
//! The native graph write precedes child storage. An interrupted input attempt is not a
//! rejection: preserve its runtime for reconciliation instead of deleting accepted work.

use super::AgentTreeMembership;
use super::LocalAgentControl;
use super::aliases::PersistedAgentSpawn;
use crate::codex_thread::CodexThread;
use crate::thread_manager::ThreadManagerState;
use crate::thread_manager::thread_store_error_kind;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrKind;
use codex_protocol::error::Result as CodexResult;
use codex_thread_store::ThreadStoreError;
use std::sync::Arc;
use tokio::sync::OwnedMutexGuard;
use tokio::task::JoinHandle;
use tracing::warn;

pub(super) struct PendingSpawn {
    control: LocalAgentControl,
    state: Arc<ThreadManagerState>,
    child_id: ThreadId,
    child: Option<Arc<CodexThread>>,
    armed: bool,
    membership: Option<AgentTreeMembership>,
    parent_guard: Option<OwnedMutexGuard<()>>,
    child_guard: Option<OwnedMutexGuard<()>>,
    edge_write: Option<JoinHandle<CodexResult<PersistedAgentSpawn>>>,
    graph_published: bool,
    graph_uncertain: bool,
    input_pending: bool,
}

impl PendingSpawn {
    pub(super) fn prepare(
        control: LocalAgentControl,
        state: Arc<ThreadManagerState>,
        child_id: ThreadId,
        parent_guard: Option<OwnedMutexGuard<()>>,
        membership: AgentTreeMembership,
    ) -> Self {
        Self {
            control,
            state,
            child_id,
            child: None,
            armed: true,
            membership: Some(membership),
            parent_guard,
            child_guard: None,
            edge_write: None,
            graph_published: false,
            graph_uncertain: false,
            input_pending: false,
        }
    }

    #[cfg(test)]
    pub(super) fn new(
        control: LocalAgentControl,
        state: Arc<ThreadManagerState>,
        child: Arc<CodexThread>,
        parent_guard: Option<OwnedMutexGuard<()>>,
        membership: AgentTreeMembership,
    ) -> Self {
        let mut pending = Self::prepare(
            control,
            state,
            child.session.thread_id(),
            parent_guard,
            membership,
        );
        pending.attach_child(child);
        pending
    }

    pub(super) fn attach_child(&mut self, child: Arc<CodexThread>) {
        assert_eq!(self.child_id, child.session.thread_id());
        assert!(self.child.is_none());
        self.child = Some(child);
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
        self.graph_published = persisted.graph_published;
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
        self.armed = false;
        if let Some(child) = self.child.take() {
            child
                .cancelled_spawn_alias_cleanup_pending
                .store(false, std::sync::atomic::Ordering::Release);
            self.state.notify_thread_created(child.session.thread_id());
        }
    }

    /// The caller has either not attempted input or received a definite rejection.
    pub(super) async fn rollback(mut self, error: CodexErr) -> CodexErr {
        self.armed = false;
        self.cleanup(error).await
    }

    async fn cleanup(&mut self, error: CodexErr) -> CodexErr {
        let child = self.child.take();
        if self.input_pending {
            if let Some(child) = child {
                child.session.quarantine_history(format!(
                    "spawn input outcome is unknown: {error}; reconcile before retrying"
                ));
            }
            return error;
        }
        let Some(membership) = self.membership.take() else {
            unreachable!("pending spawn must own agent-tree membership");
        };
        let teardown = membership.into_teardown_guard("child_spawn_cleanup", Some(self.child_id));
        // Join the original Open write before attempting Closed. A failed acknowledgement
        // leaves graph state uncertain; do not compensate or discard its source history.
        if let Some(edge_write) = self.edge_write.take() {
            match edge_write.await {
                Ok(Ok(persisted)) => {
                    self.graph_published = persisted.graph_published;
                    self.graph_uncertain = false;
                }
                Ok(Err(_)) | Err(_) => self.graph_uncertain = true,
            }
        }
        let id = self.child_id;
        if self.child_guard.is_none() {
            self.child_guard = Some(self.state.agent_lifecycle_lock(id).lock_owned().await);
        }
        let Some(child) = child else {
            if self.state.get_thread(id).await.is_ok() {
                // Without an attached Arc, a runtime at this ID belongs to another owner.
                teardown.record_shutdown_failure("validate_child", "runtime_unowned");
                teardown.complete();
                return CodexErr::Fatal(format!(
                    "{error}; reserved child ID became live before attachment; graph retained"
                ));
            }
            if self.graph_published
                && !self.graph_uncertain
                && let Err(close) = self.control.persist_agent_closed(id).await
            {
                teardown.record_shutdown_failure("close_spawn_edge", "agent_graph_store_error");
                teardown.complete();
                return CodexErr::Fatal(format!(
                    "{error}; spawn graph cleanup failed: {close}; history retained"
                ));
            }
            teardown.complete();
            return error;
        };
        if self
            .state
            .get_thread(id)
            .await
            .is_ok_and(|current| !Arc::ptr_eq(&current, &child))
        {
            child
                .session
                .quarantine_history("provisional child was replaced during cleanup".into());
            teardown.record_shutdown_failure("validate_child", "runtime_replaced");
            teardown.complete();
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
            if let Err(shutdown) = &shutdown {
                teardown.record_shutdown_failure(
                    "stop_child",
                    codex_protocol::error::CodexErrKind::from(shutdown).into(),
                );
            }
            if alias_cleanup.is_err() {
                teardown.record_shutdown_failure("close_spawn_edge", "agent_graph_store_error");
            }
            let retained = self.state.retain_failed_spawn_cleanup(&child).await;
            teardown.complete();
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
            teardown.record_shutdown_failure(
                "discard_child_persistence",
                thread_store_error_kind(&discard),
            );
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
        teardown.complete();
        error
    }
}

impl Drop for PendingSpawn {
    fn drop(&mut self) {
        if !std::mem::replace(&mut self.armed, false) {
            return;
        }
        if self.input_pending {
            if let Some(child) = self.child.take() {
                child.session.quarantine_history(
                    "spawn worker ended during input admission; input outcome unknown; do not resubmit"
                        .into(),
                );
            }
            return;
        }
        let mut cleanup = Self {
            control: self.control.clone(),
            state: Arc::clone(&self.state),
            child_id: self.child_id,
            child: self.child.take(),
            armed: false,
            membership: self.membership.take(),
            parent_guard: self.parent_guard.take(),
            child_guard: self.child_guard.take(),
            edge_write: self.edge_write.take(),
            graph_published: self.graph_published,
            graph_uncertain: self.graph_uncertain,
            input_pending: false,
        };
        drop(tokio::spawn(async move {
            let error = cleanup
                .cleanup(CodexErr::Fatal("spawn setup was abandoned".into()))
                .await;
            warn!("{error}");
        }));
    }
}

/// Hosted startup retains the host's graph authority and the existing cleanup contract.
pub(in crate::agent) struct PendingHostSpawn {
    state: Arc<ThreadManagerState>,
    pending: Option<(ThreadId, AgentTreeMembership)>,
}

impl PendingHostSpawn {
    pub(in crate::agent) fn new(
        state: Arc<ThreadManagerState>,
        child: ThreadId,
        membership: AgentTreeMembership,
    ) -> Self {
        Self {
            state,
            pending: Some((child, membership)),
        }
    }

    pub(in crate::agent) fn disarm(mut self) -> AgentTreeMembership {
        let Some((_child, membership)) = self.pending.take() else {
            unreachable!("pending spawn must own agent-tree membership");
        };
        membership
    }
}

impl Drop for PendingHostSpawn {
    fn drop(&mut self) {
        let Some((child, membership)) = self.pending.take() else {
            return;
        };
        let state = Arc::clone(&self.state);
        let teardown = membership.into_teardown_guard("child_spawn_cleanup", Some(child));
        drop(tokio::spawn(async move {
            if let Some(thread) = state.remove_thread(&child).await {
                if let Err(error) = thread.shutdown_and_wait().await {
                    teardown
                        .record_shutdown_failure("stop_child", CodexErrKind::from(&error).into());
                    warn!("failed to stop cancelled child spawn: {error}");
                }
                if let Some(live_thread) = thread.session.live_thread() {
                    match live_thread.discard().await {
                        Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => {}
                        Err(error) => {
                            teardown.record_shutdown_failure(
                                "discard_child_persistence",
                                thread_store_error_kind(&error),
                            );
                            warn!("failed to discard cancelled child spawn: {error}");
                        }
                    }
                }
            }
            teardown.complete();
        }));
    }
}
