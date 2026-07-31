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

    /// Called only by explicit close while holding the shared child lifecycle gate.
    pub(crate) async fn close_fenced_restoration_edge(
        &self,
        fence: &RestorationFence,
    ) -> CodexResult<()> {
        let graph = self.agent_graph_store().ok_or_else(|| {
            CodexErr::InvalidRequest(
                "cannot close a quarantined restoration without its captured agent graph"
                    .to_string(),
            )
        })?;
        graph
            .set_thread_spawn_edge_status(fence.runtime.thread_id, ThreadSpawnEdgeStatus::Closed)
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("quarantined restoration close failed: {error}"))
            })?;
        let closed = graph
            .list_thread_spawn_children(fence.parent_thread_id, Some(ThreadSpawnEdgeStatus::Closed))
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "quarantined restoration close verification failed: {error}"
                ))
            })?;
        if !closed.contains(&fence.runtime.thread_id) {
            return Err(CodexErr::InvalidRequest(
                "quarantined restoration's captured spawn edge was not closed".to_string(),
            ));
        }
        Ok(())
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
