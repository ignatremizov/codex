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

    pub(super) fn disarm(mut self) {
        self.armed = false;
        self.child = None;
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
        let _child_guard = self.state.agent_lifecycle_lock(id).lock_owned().await;
        if child.is_none() && self.state.get_thread(id).await.is_ok() {
            // Before startup returns an exact Arc, no runtime registered under this ID
            // belongs to this guard. Do not close another owner's graph or history.
            teardown.record_shutdown_failure("validate_child", "runtime_unowned");
            teardown.complete();
            return CodexErr::Fatal(format!(
                "{error}; reserved child ID became live before attachment; graph retained"
            ));
        }
        let mut error = error;
        if let Some(child) = &child {
            if self
                .state
                .get_thread(id)
                .await
                .is_ok_and(|current| !Arc::ptr_eq(&current, child))
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
            error = self
                .control
                .cleanup_unpublished_restoration(child, error)
                .await;
            self.state
                .remove_thread_if_matches_with(&id, child, || {
                    self.control.runtime.registry.release_spawned_thread(id);
                })
                .await;
        }
        if !self.graph_uncertain {
            if self.graph_published
                && let Err(close) = self.control.persist_agent_closed(id).await
            {
                teardown.record_shutdown_failure("close_spawn_edge", "agent_graph_store_error");
                teardown.complete();
                return CodexErr::Fatal(format!(
                    "{error}; spawn graph cleanup failed: {close}; history retained"
                ));
            }
            // A published edge or alias is durable audit state even after Closed. Preserve
            // its matching history rather than leaving a resumable alias without a source.
            if !self.graph_published
                && let Some(child) = child
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
