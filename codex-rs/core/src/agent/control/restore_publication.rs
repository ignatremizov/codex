//! Checked publication of a prepared runtime through its existing control plane.

use super::*;
use crate::codex_thread::CodexThread;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;

impl LocalAgentControl {
    pub(crate) async fn publish_restored_agent(
        &self,
        thread: &Arc<CodexThread>,
        parent: Option<&Arc<CodexThread>>,
        commit_metadata: impl FnOnce() -> CodexResult<()> + Send,
    ) -> CodexResult<()> {
        let state = self.runtime.upgrade()?;
        let thread_id = thread.session.thread_id();
        self.restore_agent_send_settings(thread.session.presentation_id())
            .await?;
        let disarm = thread.session.disarm_terminal_presentation();
        // A lifecycle/configuration parent does not turn a specialized session (for
        // example a reviewer) into a ThreadSpawn with ordinary completion delivery.
        // A real ThreadSpawn still checks its exact source parent in the binder.
        if let Some(parent) = parent
            && thread.session_source.parent_thread_id().is_some()
        {
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
        let checked_graph = if (thread.multi_agent_version() == Some(MultiAgentVersion::V2)
            || graph
                .as_ref()
                .is_some_and(|graph| graph.supports_agent_aliases()))
            && let Some(parent_id) = parent_id
        {
            let graph = graph.ok_or_else(|| {
                CodexErr::InvalidRequest(
                    "cannot restore a child without its persisted agent graph".to_string(),
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
                        "cannot restore a child without its persisted spawn edge".to_string(),
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
            if let Some((graph, previous)) = checked_graph {
                let owner_session_id = self
                    .bound_session_id()
                    .filter(|_| graph.supports_agent_aliases());
                let rollback: CodexResult<()> = async {
                    let parent_thread_id = parent_id.ok_or_else(|| {
                        CodexErr::InvalidRequest("missing restored parent".to_string())
                    })?;
                    let _permission = self.acquire_messaging_permission_transaction().await;
                    match previous {
                        ThreadSpawnEdgeStatus::Closed => {
                            let revoked = graph
                                .close_thread_spawn_edge_if_current(
                                    codex_agent_graph_store::ThreadSpawnEdgeAuthority {
                                        thread_id,
                                        parent_thread_id,
                                        owner_session_id,
                                    },
                                    vec![thread_id],
                                )
                                .await
                                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
                            if revoked
                                && let Err(cleanup) = state
                                    .supersede_mailbox_final_subscriptions_for_threads(vec![
                                        thread_id,
                                    ])
                                    .await
                            {
                                warn!(
                                    %cleanup,
                                    %thread_id,
                                    "mailbox cleanup after restoration rollback failed; graph epochs fence old intents"
                                );
                            }
                        }
                        ThreadSpawnEdgeStatus::Open => {
                            // Publication only writes Open. Do not reopen an edge that
                            // another owner closed while the failed publication unwound.
                            let open = graph
                                .list_thread_spawn_children(
                                    parent_thread_id,
                                    Some(ThreadSpawnEdgeStatus::Open),
                                )
                                .await
                                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
                            if !open.contains(&thread_id) {
                                return Err(CodexErr::InvalidRequest(
                                    "restored spawn edge changed during publication".to_string(),
                                ));
                            }
                            if graph.supports_agent_aliases() {
                                let current = graph
                                    .find_current_agent_alias_by_thread(thread_id)
                                    .await
                                    .map_err(|error| CodexErr::Fatal(error.to_string()))?;
                                if current.as_ref().map(|alias| alias.session_id)
                                    != owner_session_id
                                    || owner_session_id.is_none()
                                {
                                    return Err(CodexErr::InvalidRequest(
                                        "restored spawn edge owner changed during publication"
                                            .to_string(),
                                    ));
                                }
                            }
                        }
                    }
                    Ok(())
                }
                .await;
                if let Err(rollback) = rollback {
                    let parent_thread_id = parent_id.ok_or_else(|| {
                        CodexErr::InvalidRequest("missing restored parent".to_string())
                    })?;
                    state.fence_restoration(crate::thread_manager::RestorationFence {
                        runtime: thread.session.presentation_id(),
                        parent_thread_id,
                        owner_session_id,
                        previous_edge: previous,
                        reason: rollback.to_string(),
                    });
                    return Err(CodexErr::Fatal(format!(
                        "{error}; spawn-edge rollback failed: {rollback}; restoration is quarantined for this manager; explicitly close the child, then resume it through its live owner"
                    )));
                }
            }
            return Err(error);
        }
        state.notify_thread_created(thread_id);
        Ok(())
    }

    pub(crate) async fn cleanup_unpublished_restoration(
        &self,
        thread: &Arc<CodexThread>,
        error: CodexErr,
    ) -> CodexErr {
        // The detached restoration transaction retains its lifecycle gates through
        // this cleanup. Failure publishes only a sealed cleanup owner, never a
        // usable replacement runtime or authority to close an unrelated alias.
        thread.session.submission_admission.seal_for_unload();
        let disarm = thread.session.disarm_terminal_presentation();
        thread
            .session
            .retire_agent_status_observers(crate::session::AgentStatusRetirement::RestoreRollback);
        let shutdown = thread.shutdown_durably_and_wait().await;
        disarm.commit();
        match shutdown {
            Ok(()) => {
                self.forget_v2_residency(thread.session.thread_id());
                error
            }
            Err(cleanup) => {
                let retained = match self.runtime.upgrade() {
                    Ok(state) => state.retain_failed_spawn_cleanup(thread).await,
                    Err(error) => Err(error),
                };
                CodexErr::Fatal(format!(
                    "{error}; unpublished runtime durable shutdown failed: {cleanup}; unload-retry retention: {retained:?}"
                ))
            }
        }
    }
}

#[cfg(test)]
#[path = "restore_publication_tests.rs"]
mod tests;
