//! Process-generation-local fencing for ambiguous spawn-edge rollback.
//!
//! This is not a durable transaction log. A process crash/restart loses these fences;
//! resolving graph/storage ambiguity across processes requires external recovery.

use super::*;
use crate::agent::control::SessionPresentationId;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;

#[derive(Clone)]
pub(crate) struct RestorationFence {
    pub(crate) runtime: SessionPresentationId,
    pub(crate) parent_thread_id: ThreadId,
    pub(crate) owner_session_id: Option<codex_protocol::SessionId>,
    pub(crate) previous_edge: ThreadSpawnEdgeStatus,
    pub(crate) reason: String,
}

impl ThreadManagerState {
    pub(crate) fn fence_restoration(&self, fence: RestorationFence) {
        self.restoration_fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(fence.runtime.thread_id, fence);
    }

    pub(crate) fn restoration_fence(&self, thread_id: ThreadId) -> Option<RestorationFence> {
        self.restoration_fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .cloned()
    }

    pub(crate) fn check_restoration_fence(&self, thread_id: ThreadId) -> CodexResult<()> {
        if let Some(fence) = self.restoration_fence(thread_id) {
            return Err(CodexErr::InvalidRequest(format!(
                "thread {thread_id} restoration is quarantined after ambiguous {:?} spawn-edge rollback: {}; explicitly close the child, then resume it through its live owner",
                fence.previous_edge, fence.reason,
            )));
        }
        Ok(())
    }

    /// Called by explicit close under child lifecycle and messaging admission gates.
    pub(crate) async fn close_fenced_restoration_edge(
        &self,
        fence: &RestorationFence,
        revoked_thread_ids: &[ThreadId],
    ) -> CodexResult<bool> {
        if let Ok(thread) = self.get_thread(fence.runtime.thread_id).await
            && thread.session.presentation_id() != fence.runtime
        {
            return Err(CodexErr::InvalidRequest(
                "quarantined restoration runtime was replaced".to_string(),
            ));
        }
        let graph = self.agent_graph_store().ok_or_else(|| {
            CodexErr::InvalidRequest(
                "cannot close a quarantined restoration without its captured agent graph"
                    .to_string(),
            )
        })?;
        // Parent and owner comparison must precede mutation in the same graph transaction.
        // A stale fence must never close an edge that a later adoption has reparented.
        graph
            .close_thread_spawn_edge_if_current(
                codex_agent_graph_store::ThreadSpawnEdgeAuthority {
                    thread_id: fence.runtime.thread_id,
                    parent_thread_id: fence.parent_thread_id,
                    owner_session_id: fence.owner_session_id,
                },
                revoked_thread_ids.to_vec(),
            )
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("quarantined restoration close failed: {error}"))
            })
    }

    /// Clear only the attempt whose acknowledged close and exact cleanup just completed.
    pub(crate) fn clear_restoration_fence(&self, fence: &RestorationFence) {
        let mut fences = self
            .restoration_fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if fences
            .get(&fence.runtime.thread_id)
            .is_some_and(|current| current.runtime == fence.runtime)
        {
            fences.remove(&fence.runtime.thread_id);
        }
    }
}
