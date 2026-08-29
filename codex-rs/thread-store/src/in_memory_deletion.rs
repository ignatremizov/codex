//! Match local-store singular graph preservation while keeping explicit bulk deletion strict.

use codex_protocol::ThreadId;

use super::InMemoryThreadStore;
use crate::DeleteThreadParams;
use crate::DeleteThreadsParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

enum AgentGraphDeletion {
    DeleteIncidentEdges,
    Preserve,
}

impl InMemoryThreadStore {
    pub(super) async fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreResult<()> {
        self.delete_thread_with_agent_graph(params.thread_id, AgentGraphDeletion::Preserve)
            .await
    }

    pub(super) async fn delete_threads(
        &self,
        params: DeleteThreadsParams,
    ) -> ThreadStoreResult<()> {
        for thread_id in params.thread_ids {
            match self
                .delete_thread_with_agent_graph(thread_id, AgentGraphDeletion::DeleteIncidentEdges)
                .await
            {
                Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    async fn delete_thread_with_agent_graph(
        &self,
        thread_id: ThreadId,
        agent_graph_deletion: AgentGraphDeletion,
    ) -> ThreadStoreResult<()> {
        self.state.lock().await.calls.delete_thread += 1;
        let deleted_state_rows = if let Some(state_db) = &self.state_db {
            match agent_graph_deletion {
                AgentGraphDeletion::DeleteIncidentEdges => {
                    state_db.delete_threads_strict(&[thread_id]).await
                }
                AgentGraphDeletion::Preserve => {
                    state_db
                        .delete_thread_preserving_agent_graph(thread_id)
                        .await
                }
            }
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to delete thread state: {error}"),
            })?
        } else {
            0
        };
        let mut state = self.state.lock().await;
        let existed = state.histories.remove(&thread_id).is_some();
        state.created_threads.remove(&thread_id);
        state.names.remove(&thread_id);
        state.metadata_updates.remove(&thread_id);
        state.sections.remove(&thread_id);
        state.section_positions.remove(&thread_id);
        state.section_entered_at.remove(&thread_id);
        state
            .rollout_paths
            .retain(|_, stored_thread_id| *stored_thread_id != thread_id);
        if existed || deleted_state_rows > 0 {
            Ok(())
        } else {
            Err(ThreadStoreError::ThreadNotFound { thread_id })
        }
    }
}

#[cfg(test)]
#[path = "in_memory_deletion_tests.rs"]
mod tests;
