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

    pub(super) fn disarm(mut self) {
        self.child = None;
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
        let _child_guard = self.state.agent_lifecycle_lock(id).lock_owned().await;
        if self.state.get_thread(id).await.is_ok_and(|current| !Arc::ptr_eq(&current, &child)) {
            child.session.quarantine_history("provisional child was replaced during cleanup".into());
            return CodexErr::Fatal(format!("{error}; provisional child was replaced; history retained"));
        }
        let mut error = self.control.cleanup_unpublished_restoration(&child, error).await;
        self.state.remove_thread_if_matches_with(&id, &child, || {
            self.control.runtime.registry.release_spawned_thread(id);
        }).await;
        if !self.graph_uncertain {
            if self.graph_published && child.session_source.parent_thread_id().is_some()
                && !child.config_snapshot().await.ephemeral
                && let Err(close) = self.control.persist_agent_closed(id).await
            {
                return CodexErr::Fatal(format!("{error}; spawn graph cleanup failed: {close}; history retained"));
            }
            // A published edge or alias is durable audit state even after Closed. Preserve
            // its matching history rather than leaving a resumable alias without a source.
            if !self.graph_published && let Some(live_thread) = child.session.live_thread()
                && let Err(discard) = live_thread.discard().await
            {
                error = CodexErr::Fatal(format!("{error}; rejected spawn history cleanup failed: {discard}"));
            }
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
