use super::*;
use crate::agent::api::AgentInfo;
use codex_protocol::error::CodexErrorDetails;
use codex_thread_store::PersistContext;
use std::collections::HashSet;

impl LocalAgentControl {
    /// Retire an exact runtime while serializing against explicit restoration.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let control = self.clone();
        tokio::spawn(async move {
            let state = control.runtime.upgrade()?;
            let lock = state.v2_spawn_resume_lock(agent_id);
            let _guard = lock.lock_owned().await;
            control.shutdown_live_agent_unlocked(agent_id).await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("agent shutdown worker failed: {error}")))?
    }

    async fn shutdown_live_agent_unlocked(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.runtime.upgrade()?;
        let thread = match state.get_thread(agent_id).await {
            Ok(thread) => thread,
            Err(error) => {
                state
                    .run_if_thread_absent(agent_id, || {
                        self.forget_v2_residency(agent_id);
                        self.runtime.registry.release_spawned_thread(agent_id);
                    })
                    .await;
                return Err(error);
            }
        };
        thread.session.ensure_rollout_materialized(PersistContext::Standard).await;
        // Flush failure must not release this runtime's writer or lifecycle ownership early.
        let flush = thread.session.flush_rollout().await;
        let submission_id = match state.send_op_to_thread(
            &thread, Op::Shutdown, /*parent_turn_id*/ None, /*root_turn_id*/ None,
        ).await {
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
                self.runtime.registry.release_spawned_thread(agent_id);
            })
            .await;
        flush?;
        Ok(submission_id)
    }

    /// Persist explicit closure and retire the live subtree, returning the pre-close snapshot.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<AgentInfo> {
        let control = self.clone();
        tokio::spawn(async move { control.close_agent_serialized(agent_id).await })
            .await
            .map_err(|error| CodexErr::Fatal(format!("agent close worker failed: {error}")))?
    }

    async fn close_agent_serialized(&self, agent_id: ThreadId) -> CodexResult<AgentInfo> {
        let state = self.runtime.upgrade()?;
        let lock = state.v2_spawn_resume_lock(agent_id);
        let _guard = lock.lock_owned().await;
        let fence = state.restoration_fence(agent_id);
        let metadata = self.get_agent_metadata(agent_id);
        let known_agent = fence.is_some() || metadata.is_some()
            || self.current_agent_alias(agent_id).await?.is_some();
        let snapshot = match state.get_thread(agent_id).await {
            Ok(thread) => AgentInfo::Loaded {
                agent: LiveAgent {
                    thread_id: agent_id,
                    metadata: metadata.unwrap_or_default(),
                    status: thread.agent_status().await,
                },
                config: Box::new(thread.config_snapshot().await),
            },
            Err(error)
                if known_agent
                    && matches!(error.details(), CodexErrorDetails::ThreadNotFound(_)) =>
            {
                AgentInfo::Unloaded(metadata.unwrap_or_default())
            }
            Err(error) => return Err(error),
        };
        // Child publication takes its direct parent's lifecycle gate. Retain every
        // discovered descendant gate and re-snapshot until subtree membership is stable.
        let mut descendants = Vec::new();
        let mut locked = HashSet::from([agent_id]);
        let mut descendant_guards = Vec::new();
        loop {
            let mut added = false;
            for child_id in self.runtime.live_thread_spawn_descendants(agent_id).await? {
                if locked.insert(child_id) {
                    descendant_guards.push(state.agent_lifecycle_lock(child_id).lock_owned().await);
                    descendants.push(child_id);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        if fence.is_none() {
            if known_agent {
                self.require_current_agent_ownership(agent_id).await?;
            }
            // Failure before this barrier must not close the alias or revoke future
            // observations for a subtree whose durable history could not be flushed.
            for id in std::iter::once(agent_id).chain(descendants.iter().copied()) {
                if let Ok(thread) = state.get_thread(id).await {
                    thread
                        .session
                        .ensure_rollout_materialized(PersistContext::Standard)
                        .await;
                    thread.session.flush_rollout().await?;
                }
            }
        }
        if let Some(fence) = &fence {
            state.close_fenced_restoration_edge(fence).await?;
        } else {
            let persistent = match &snapshot {
                AgentInfo::Loaded { config, .. } => !config.ephemeral,
                AgentInfo::Unloaded(_) => true,
            };
            if persistent {
                self.persist_agent_closed(agent_id).await?;
            }
        }
        // An explicit close revokes future live recovery across every observer control.
        // Accepted exact-session deliveries are drained independently; their receipts are
        // never moved to a later runtime with the same rollout UUID.
        for closed_id in std::iter::once(agent_id).chain(descendants.iter().copied()) {
            state.advance_agent_lifecycle_generation(closed_id);
            if let Ok(thread) = state.get_thread(closed_id).await {
                self.revoke_response_observations_for_child(thread.session.presentation_id());
            }
        }
        let result = self.shutdown_live_agent_unlocked(agent_id).await;
        for child_id in descendants {
            match Box::pin(self.shutdown_live_agent_unlocked(child_id)).await {
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
            Err(error) if known_agent && matches!(error.details(),
                CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
            ) => Ok(snapshot),
            result => result.map(|_| snapshot),
        };
        if result.is_ok() && let Some(fence) = &fence {
            state.clear_restoration_fence(fence);
        }
        result
    }

    /// Shut down `agent_id` and any live descendants reachable from the in-memory spawn tree.
    pub(crate) async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let descendant_ids = self.runtime.live_thread_spawn_descendants(agent_id).await?;
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
