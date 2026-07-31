use super::*;
use codex_protocol::error::CodexErrorDetails;
use codex_thread_store::PersistContext;

impl LocalAgentControl {
    /// Retire an exact runtime while serializing against explicit restoration.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let control = self.clone();
        tokio::spawn(async move {
            let state = control.upgrade()?;
            let lock = state.v2_spawn_resume_lock(agent_id);
            let _guard = lock.lock_owned().await;
            control.shutdown_live_agent_unlocked(agent_id).await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("agent shutdown worker failed: {error}")))?
    }

    async fn shutdown_live_agent_unlocked(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let thread = match state.get_thread(agent_id).await {
            Ok(thread) => thread,
            Err(error) => {
                state
                    .run_if_thread_absent(agent_id, || {
                        self.forget_v2_residency(agent_id);
                        self.state.release_spawned_thread(agent_id);
                    })
                    .await;
                return Err(error);
            }
        };
        thread
            .session
            .ensure_rollout_materialized(PersistContext::Standard)
            .await;
        let flush = thread.session.flush_rollout().await;
        let submission_id = match state
            .send_op_to_thread(
                &thread,
                Op::Shutdown,
                /*parent_turn_id*/ None,
                /*root_turn_id*/ None,
            )
            .await
        {
            Ok(submission_id) => submission_id,
            Err(error) if matches!(error.details(), CodexErrorDetails::InternalAgentDied) => {
                String::new()
            }
            Err(error) => return Err(error),
        };
        thread.wait_until_terminated().await;
        state
            .remove_thread_if_matches_with(&agent_id, &thread, || {
                self.forget_v2_residency(agent_id);
                self.state.release_spawned_thread(agent_id);
            })
            .await;
        flush?;
        Ok(submission_id)
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and any live descendants reached from the in-memory tree.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let control = self.clone();
        tokio::spawn(async move { control.close_agent_serialized(agent_id).await })
            .await
            .map_err(|error| CodexErr::Fatal(format!("agent close worker failed: {error}")))?
    }

    async fn close_agent_serialized(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let lock = state.v2_spawn_resume_lock(agent_id);
        let _guard = lock.lock_owned().await;
        let fence = state.restoration_fence(agent_id);
        let known_agent =
            fence.is_some() || self.state.agent_metadata_for_thread(agent_id).is_some();
        if let Some(fence) = &fence {
            state.close_fenced_restoration_edge(fence).await?;
        } else {
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
                        return Err(CodexErr::Fatal(format!(
                            "failed to persist thread-spawn edge status for {agent_id}: {err}"
                        )));
                    }
                }
                Err(err)
                    if known_agent
                        && matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) =>
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
        }
        let descendants = self.live_thread_spawn_descendants(agent_id).await?;
        let result = self.shutdown_live_agent_unlocked(agent_id).await;
        for child_id in descendants {
            match Box::pin(self.shutdown_live_agent(child_id)).await {
                Err(error)
                    if !matches!(
                        error.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) =>
                {
                    return Err(error);
                }
                Ok(_) | Err(_) => {}
            }
        }
        let result = match result {
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
        };
        if result.is_ok()
            && let Some(fence) = &fence
        {
            state.clear_restoration_fence(fence);
        }
        result
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
