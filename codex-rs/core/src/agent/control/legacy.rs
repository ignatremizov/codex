use super::*;
use codex_protocol::error::CodexErrorDetails;

#[derive(Clone, Copy)]
pub(crate) enum LiveAgentMetadataDisposition {
    Preserve,
    Release,
}

impl AgentControl {
    /// Remove a specific restored runtime when graceful rollback cannot finish.
    ///
    /// Dropping the manager's last `CodexThread` handle closes its submission channel. Cleanup
    /// deliberately targets the concrete instance so a concurrent replacement with the same
    /// thread ID is left intact.
    pub(crate) async fn discard_live_agent_instance(
        &self,
        thread: &Arc<CodexThread>,
        metadata_disposition: LiveAgentMetadataDisposition,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let child = thread.session.presentation_id();
        let _ = state
            .remove_thread_if_current(thread, || {
                self.forget_v2_residency(child.thread_id);
                if matches!(metadata_disposition, LiveAgentMetadataDisposition::Release) {
                    self.release_spawned_thread(SpawnedThreadRelease::Session(child));
                }
            })
            .await;
        Ok(())
    }

    /// Shut down one concrete runtime without affecting a replacement that reused its thread ID.
    pub(crate) async fn shutdown_live_agent_instance(
        &self,
        thread: &Arc<CodexThread>,
        metadata_disposition: LiveAgentMetadataDisposition,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let agent_id = thread.session.thread_id();
        thread.session.ensure_rollout_materialized().await;
        thread.session.flush_rollout().await?;
        let result = if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
            Ok(String::new())
        } else {
            state
                .send_op_to_thread(thread, Op::Shutdown {}, /*parent_turn_id*/ None)
                .await
        };
        thread.wait_until_terminated().await;
        let child = thread.session.presentation_id();
        let _ = state
            .remove_thread_if_current(thread, || {
                self.forget_v2_residency(agent_id);
                if matches!(metadata_disposition, LiveAgentMetadataDisposition::Release) {
                    self.release_spawned_thread(SpawnedThreadRelease::Session(child));
                }
            })
            .await;
        result
    }

    /// Submit a shutdown request for a live agent without marking it explicitly closed in
    /// persisted spawn-edge state.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let thread = match state.get_thread(agent_id).await {
            Ok(thread) => thread,
            Err(err) => {
                let _ = state
                    .run_if_thread_absent(agent_id, || {
                        self.forget_v2_residency(agent_id);
                        self.release_spawned_thread(SpawnedThreadRelease::AbsentThread(agent_id));
                    })
                    .await;
                return Err(err);
            }
        };
        self.shutdown_live_agent_instance(&thread, LiveAgentMetadataDisposition::Release)
            .await
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and any live descendants reached from the in-memory tree.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let known_agent = self.state.agent_metadata_for_thread(agent_id).is_some();
        match state.get_thread(agent_id).await {
            Ok(thread) => {
                if !thread.config_snapshot().await.ephemeral
                    && let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    warn!("failed to persist thread-spawn edge status for {agent_id}: {err}");
                }
            }
            Err(err)
                if known_agent && matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) =>
            {
                if let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    return Err(CodexErr::Fatal(format!(
                        "failed to persist stale thread-spawn edge status for {agent_id}: {err}"
                    )));
                }
            }
            Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {}
            Err(err) => {
                warn!("failed to inspect agent before close {agent_id}: {err}");
            }
        }
        match Box::pin(self.shutdown_agent_tree(agent_id)).await {
            Err(err)
                if known_agent
                    && matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) =>
            {
                Ok(String::new())
            }
            result => result,
        }
    }

    /// Shut down `agent_id` and any live descendants reachable from the in-memory spawn tree.
    pub(crate) async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let descendant_ids = self.live_thread_spawn_descendants(agent_id).await?;
        let result = self.shutdown_live_agent(agent_id).await;
        for descendant_id in descendant_ids {
            match self.shutdown_live_agent(descendant_id).await {
                Ok(_) => {}
                Err(err)
                    if matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) => {}
                Err(err) => return Err(err),
            }
        }
        result
    }
}
