//! Checked publication of a prepared runtime through its existing control plane.

use super::*;
use crate::codex_thread::CodexThread;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;

impl LocalAgentControl {
    pub(super) async fn publish_restored_agent(
        &self,
        thread: &Arc<CodexThread>,
        parent: Option<&Arc<CodexThread>>,
        commit_metadata: impl FnOnce() -> CodexResult<()> + Send,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let thread_id = thread.session.thread_id();
        let disarm = thread.session.disarm_terminal_presentation();
        if let Some(parent) = parent {
            let source = thread.session_source.clone();
            let child_path = source.get_agent_path();
            let reference = child_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| thread_id.to_string());
            self.bind_completion_watcher_with_parent(
                thread,
                parent,
                source,
                reference,
                child_path,
                thread
                    .multi_agent_version()
                    .unwrap_or(MultiAgentVersion::V1),
            )?;
        }
        let graph = state.agent_graph_store();
        let parent_id = thread.session_source.parent_thread_id();
        let checked_graph = if thread.multi_agent_version() == Some(MultiAgentVersion::V2)
            && let Some(parent_id) = parent_id
        {
            let graph = graph.ok_or_else(|| {
                CodexErr::InvalidRequest(
                    "cannot restore a V2 child without its persisted agent graph".to_string(),
                )
            })?;
            let open = graph
                .list_thread_spawn_children(parent_id, Some(ThreadSpawnEdgeStatus::Open))
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
            let previous = if open.contains(&thread_id) {
                ThreadSpawnEdgeStatus::Open
            } else {
                let closed = graph
                    .list_thread_spawn_children(parent_id, Some(ThreadSpawnEdgeStatus::Closed))
                    .await
                    .map_err(|error| CodexErr::Fatal(error.to_string()))?;
                if !closed.contains(&thread_id) {
                    return Err(CodexErr::InvalidRequest(
                        "cannot restore a V2 child without its persisted spawn edge".to_string(),
                    ));
                }
                ThreadSpawnEdgeStatus::Closed
            };
            Some((graph, previous))
        } else {
            None
        };
        let publication = async {
            if let Some((graph, _)) = &checked_graph {
                graph
                    .set_thread_spawn_edge_status(thread_id, ThreadSpawnEdgeStatus::Open)
                    .await
                    .map_err(|error| CodexErr::Fatal(error.to_string()))?;
                let children = graph
                    .list_thread_spawn_children(
                        parent_id.ok_or_else(|| {
                            CodexErr::InvalidRequest("missing restored parent".to_string())
                        })?,
                        Some(ThreadSpawnEdgeStatus::Open),
                    )
                    .await
                    .map_err(|error| CodexErr::Fatal(error.to_string()))?;
                if !children.contains(&thread_id) {
                    return Err(CodexErr::Fatal(
                        "restored spawn edge did not become open".to_string(),
                    ));
                }
            }
            state
                .publish_restored_thread(thread, parent, || {
                    commit_metadata()?;
                    drop(disarm);
                    Ok(())
                })
                .await
        }
        .await;
        if let Err(error) = publication {
            if let Some((graph, previous)) = checked_graph
                && let Err(rollback) = graph
                    .set_thread_spawn_edge_status(thread_id, previous)
                    .await
            {
                if let Some(parent_thread_id) = parent_id {
                    state.fence_restoration(crate::thread_manager::RestorationFence {
                        runtime: thread.session.presentation_id(),
                        parent_thread_id,
                        previous_edge: previous,
                        reason: rollback.to_string(),
                    });
                }
                return Err(CodexErr::Fatal(format!(
                    "{error}; spawn-edge rollback failed: {rollback}; restoration is quarantined for this manager; explicitly close the child, then resume it through its live owner"
                )));
            }
            return Err(error);
        }
        state.notify_thread_created(thread_id);
        Ok(())
    }

    pub(super) async fn cleanup_unpublished_restoration(
        &self,
        thread: &Arc<CodexThread>,
        error: CodexErr,
    ) -> CodexErr {
        let disarm = thread.session.disarm_terminal_presentation();
        thread
            .session
            .retire_agent_status_observers(crate::session::AgentStatusRetirement::RestoreRollback);
        let shutdown = thread.shutdown_and_wait().await;
        if shutdown.is_err() {
            // Retain the runtime and the caller's restoration gate until the session-loop
            // barrier completes. A failed shutdown request is not proof of writer release.
            thread.wait_until_terminated().await;
        }
        disarm.commit();
        self.forget_v2_residency(thread.session.thread_id());
        match shutdown {
            Ok(()) => error,
            Err(cleanup) => CodexErr::Fatal(format!(
                "{error}; unpublished runtime shutdown failed: {cleanup}"
            )),
        }
    }
}

#[cfg(test)]
#[path = "restore_publication_tests.rs"]
mod tests;
