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
        tokio::spawn(async move {
            control
                .close_agent_serialized(agent_id)
                .await
                .map(|(submission, _)| submission)
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("agent close worker failed: {error}")))?
    }

    pub(crate) async fn close_agent_with_status(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<ClosedAgent> {
        let control = self.clone();
        tokio::spawn(async move {
            control
                .close_agent_serialized(agent_id)
                .await
                .map(|(_, closed)| closed)
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("agent close worker failed: {error}")))?
    }

    async fn close_agent_serialized(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<(String, ClosedAgent)> {
        let state = self.upgrade()?;
        let lock = state.v2_spawn_resume_lock(agent_id);
        let _guard = lock.lock_owned().await;
        let closed = match state.get_thread(agent_id).await {
            Ok(thread) => {
                let (snapshot, _) = thread.session.subscribe_agent_responses();
                ClosedAgent {
                    previous_status: thread.agent_status().await,
                    previous_presentation: Some(thread.session.presentation_id()),
                    previous_turn_id: snapshot
                        .active_turn_id
                        .is_none()
                        .then(|| snapshot.last_terminal.map(|(turn_id, _)| turn_id))
                        .flatten(),
                }
            }
            Err(_) => ClosedAgent {
                previous_status: AgentStatus::NotFound,
                previous_presentation: None,
                previous_turn_id: None,
            },
        };
        let fence = state.restoration_fence(agent_id);
        let known_agent = fence.is_some()
            || self.state.agent_metadata_for_thread(agent_id).is_some()
            || self.current_agent_alias(agent_id).await?.is_some();
        // Child publication takes its direct parent's lifecycle gate. Retain every
        // discovered descendant gate and re-snapshot until subtree membership is stable.
        let mut descendants = Vec::new();
        let mut locked = HashSet::from([agent_id]);
        let mut descendant_guards = Vec::new();
        loop {
            let mut added = false;
            for child_id in self.live_thread_spawn_descendants(agent_id).await? {
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
        let _source_admissions = state
            .agent_turn_queue
            .acquire_source_admissions(locked.iter().copied())
            .await;
        let closed_thread_ids = std::iter::once(agent_id)
            .chain(descendants.iter().copied())
            .collect::<Vec<_>>();
        let messaging_permission = self.acquire_messaging_permission_transaction().await;
        let authority_revoked = if let Some(fence) = &fence {
            state
                .close_fenced_restoration_edge(fence, &closed_thread_ids)
                .await?
        } else {
            match state.get_thread(agent_id).await {
                Ok(thread) => {
                    if !thread.config_snapshot().await.ephemeral {
                        self.persist_agent_closed_for_subtree(agent_id, &closed_thread_ids)
                            .await?
                    } else {
                        false
                    }
                }
                Err(err)
                    if known_agent
                        && matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) =>
                {
                    self.persist_agent_closed_for_subtree(agent_id, &closed_thread_ids)
                        .await?
                }
                Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => false,
                Err(err) => return Err(err),
            }
        };
        if authority_revoked
            && let Err(error) = state
                .thread_store()
                .supersede_mailbox_final_subscriptions_for_threads(closed_thread_ids.clone())
                .await
        {
            warn!(
                %error,
                thread_id = %agent_id,
                "mailbox cleanup after close failed; committed graph epochs fence old intents"
            );
        }
        // An explicit close revokes future live recovery across every observer control.
        // Accepted exact-session deliveries are drained independently; their receipts are
        // never moved to a later runtime with the same rollout UUID.
        for closed_id in closed_thread_ids {
            let thread = state.get_thread(closed_id).await.ok();
            // Re-closing an absent closed alias must not invalidate fresh current-epoch
            // opportunities. A still-live exact runtime is independently retired below.
            if authority_revoked || thread.is_some() {
                state.advance_agent_lifecycle_generation(closed_id);
            }
            if let Some(thread) = thread {
                self.revoke_response_observations_for_child(thread.session.presentation_id());
            }
        }
        // Delivery/shutdown draining must not hold the admission lock needed by accepted work.
        drop(messaging_permission);
        state
            .agent_turn_queue
            .cancel_for_threads(locked.iter().copied());
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
        result.map(|submission| (submission, closed))
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
