//! Final state cleanup retains deletion tombstones atomically with thread-row removal.
//! Graph-preserving deletion retains historical ownership evidence, not permission to revive a
//! deleted identity or reconstruct ownership from missing metadata.

use codex_protocol::ThreadId;

use super::StateRuntime;

enum AgentGraphDeletion {
    DeleteIncidentEdges,
    Preserve,
}

impl StateRuntime {
    /// Delete a thread and its associated state, including incident spawn edges.
    pub async fn delete_thread(&self, thread_id: ThreadId) -> anyhow::Result<u64> {
        self.delete_threads_strict(&[thread_id]).await
    }

    /// Delete one thread's state while retaining durable agent graph and alias evidence.
    ///
    /// Related threads keep their existing identities; the selected ID remains tombstoned.
    /// This does not recover missing ownership metadata or reopen descendants across deleted
    /// intermediates.
    pub async fn delete_thread_preserving_agent_graph(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<u64> {
        self.delete_threads_with_agent_graph(&[thread_id], AgentGraphDeletion::Preserve)
            .await
    }

    /// Delete a set of threads and their associated state, including incident spawn edges.
    ///
    /// Spawn edges and thread rows are deleted last so a failed delete can be retried with enough
    /// state left to rediscover the same spawned subtree.
    pub async fn delete_threads_strict(&self, thread_ids: &[ThreadId]) -> anyhow::Result<u64> {
        self.delete_threads_with_agent_graph(thread_ids, AgentGraphDeletion::DeleteIncidentEdges)
            .await
    }

    async fn delete_threads_with_agent_graph(
        &self,
        thread_ids: &[ThreadId],
        agent_graph_deletion: AgentGraphDeletion,
    ) -> anyhow::Result<u64> {
        if thread_ids.is_empty() {
            return Ok(0);
        }

        let thread_id_strings = thread_ids
            .iter()
            .map(ThreadId::to_string)
            .collect::<Vec<_>>();
        for (thread_id, thread_id_string) in thread_ids.iter().zip(&thread_id_strings) {
            sqlx::query("DELETE FROM logs WHERE thread_id = ?")
                .bind(thread_id_string)
                .execute(self.logs_pool.as_ref())
                .await?;
            self.thread_queue.delete_thread_queue(*thread_id).await?;
            self.delete_versioned_thread_memory(*thread_id).await?;
            self.thread_goals.delete_thread_goal(*thread_id).await?;
        }

        let mut tx = self.pool.begin().await?;
        for thread_id_string in &thread_id_strings {
            sqlx::query(
                r#"
INSERT OR IGNORE INTO agent_alias_tombstones (thread_id)
SELECT ?
WHERE EXISTS (SELECT 1 FROM threads WHERE id = ?)
   OR EXISTS (SELECT 1 FROM agent_aliases WHERE thread_id = ?)
   OR EXISTS (
       SELECT 1 FROM thread_spawn_edges
       WHERE parent_thread_id = ? OR child_thread_id = ?
   )
                "#,
            )
            .bind(thread_id_string)
            .bind(thread_id_string)
            .bind(thread_id_string)
            .bind(thread_id_string)
            .bind(thread_id_string)
            .execute(&mut *tx)
            .await?;
        }
        for thread_id_string in &thread_id_strings {
            sqlx::query("DELETE FROM thread_dynamic_tools WHERE thread_id = ?")
                .bind(thread_id_string)
                .execute(&mut *tx)
                .await?;
        }
        match agent_graph_deletion {
            AgentGraphDeletion::DeleteIncidentEdges => {
                for thread_id_string in &thread_id_strings {
                    sqlx::query(
                        "DELETE FROM thread_spawn_edges WHERE parent_thread_id = ? OR child_thread_id = ?",
                    )
                    .bind(thread_id_string)
                    .bind(thread_id_string)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            AgentGraphDeletion::Preserve => {}
        }
        let mut rows_affected = 0;
        for thread_id_string in &thread_id_strings {
            rows_affected += sqlx::query("DELETE FROM threads WHERE id = ?")
                .bind(thread_id_string)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;

        Ok(rows_affected)
    }
}

#[cfg(test)]
#[path = "thread_deletion_tests.rs"]
mod tests;
