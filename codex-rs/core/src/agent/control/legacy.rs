use super::*;
use codex_protocol::error::CodexErrorDetails;
use codex_thread_store::PersistContext;
use std::collections::HashSet;

#[derive(Clone, Copy)]
pub(crate) enum LiveAgentMetadataDisposition {
    Preserve,
    Release,
}

impl AgentControl {
    /// Remove a runtime that was never published to thread-created subscribers.
    ///
    /// Suppressing the final-outcome fallback prevents a failed spawn or resume from emitting a
    /// completion for an agent identity that its caller never received.
    pub(crate) async fn discard_unpublished_agent_instance(
        &self,
        thread: &Arc<CodexThread>,
        metadata_disposition: LiveAgentMetadataDisposition,
    ) -> CodexResult<()> {
        let terminal_presentation_disarm = thread.session.disarm_terminal_presentation();
        let result = self
            .discard_live_agent_instance(thread, metadata_disposition)
            .await;
        terminal_presentation_disarm.commit();
        result
    }

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
            .remove_thread_if_current_or_cleanup_if_absent(thread, || {
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
        thread
            .session
            .ensure_rollout_materialized(PersistContext::Standard)
            .await;
        thread.session.flush_rollout().await?;
        self.shutdown_prepared_live_agent_instance(thread, metadata_disposition)
            .await
    }

    /// Shut down a concrete runtime after its current rollout writes passed a durability barrier.
    ///
    /// The session shutdown handler performs its own final persistence shutdown after active work
    /// stops. This entry point exists so an explicit subtree close can flush every live member
    /// before committing any alias or response-observation lifecycle changes.
    async fn shutdown_prepared_live_agent_instance(
        &self,
        thread: &Arc<CodexThread>,
        metadata_disposition: LiveAgentMetadataDisposition,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let agent_id = thread.session.thread_id();
        let result = if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
            Ok(String::new())
        } else {
            state
                .send_op_to_thread(
                    thread,
                    Op::Shutdown {},
                    /*parent_turn_id*/ None,
                    /*root_turn_id*/ None,
                )
                .await
        };
        thread.wait_until_terminated().await;
        let child = thread.session.presentation_id();
        let _ = state
            .remove_thread_if_current_or_cleanup_if_absent(thread, || {
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
    #[cfg(test)]
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let thread = match state.get_thread_including_pending(agent_id).await {
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
    #[cfg(test)]
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        self.close_agent_with_status(agent_id)
            .await
            .map(|_| String::new())
    }

    pub(crate) async fn close_agent_with_status(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<ClosedAgent> {
        let state = self.upgrade()?;
        let lifecycle_lock = state.agent_lifecycle_lock(agent_id);
        let _lifecycle_guard = lifecycle_lock.lock_owned().await;
        self.require_current_agent_ownership(agent_id).await?;
        let known_agent = self.state.agent_metadata_for_thread(agent_id).is_some();
        let target_thread = match state.get_thread_including_pending(agent_id).await {
            Ok(thread) => Some(thread),
            Err(err)
                if known_agent
                    && matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) =>
            {
                None
            }
            Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => None,
            Err(err) => return Err(err),
        };
        let persist_target_closed = match target_thread.as_ref() {
            Some(thread) => !thread.config_snapshot().await.ephemeral,
            None => known_agent,
        };
        let previous_status = match target_thread.as_ref() {
            Some(thread) => thread.agent_status().await,
            None => AgentStatus::NotFound,
        };

        // Membership changes take the direct parent's lifecycle lock before publishing or
        // reopening a child. Stabilize the live subtree by taking every discovered descendant
        // lock in parent-before-child order, then re-snapshot until no new member can appear.
        let mut descendant_ids = Vec::new();
        let mut locked_thread_ids = HashSet::from([agent_id]);
        let mut _descendant_lifecycle_guards = Vec::new();
        loop {
            let discovered_ids = self.live_thread_spawn_descendants(agent_id).await?;
            let mut added_descendant = false;
            for descendant_id in discovered_ids {
                if locked_thread_ids.insert(descendant_id) {
                    let descendant_guard =
                        state.agent_lifecycle_lock(descendant_id).lock_owned().await;
                    descendant_ids.push(descendant_id);
                    _descendant_lifecycle_guards.push(descendant_guard);
                    added_descendant = true;
                }
            }
            if !added_descendant {
                break;
            }
        }

        let mut descendant_threads = Vec::new();
        for descendant_id in &descendant_ids {
            match state.get_thread_including_pending(*descendant_id).await {
                Ok(thread) => descendant_threads.push(thread),
                Err(err)
                    if matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) => {}
                Err(err) => return Err(err),
            }
        }
        let closed_thread_ids = std::iter::once(agent_id)
            .chain(descendant_ids.iter().copied())
            .collect::<Vec<_>>();
        // Queue admission takes the target lifecycle boundary before this source boundary. Since
        // the complete closing subtree is already locked, waiting here cannot invert that order.
        // Once these guards are held, close can either fail without cancelling work or commit and
        // cancel every prompt authored by the closing subtree before any worker admits it.
        let _source_admission_guards = state
            .agent_turn_queue
            .acquire_source_admissions(closed_thread_ids.iter().copied())
            .await;

        // Do not make a durable alias Closed, or revoke response delivery, until every currently
        // live subtree member has crossed a rollout durability barrier. A flush failure therefore
        // leaves the complete subtree active and retryable instead of publishing a partial close.
        for thread in target_thread.iter().chain(descendant_threads.iter()) {
            thread
                .session
                .ensure_rollout_materialized(PersistContext::Standard)
                .await;
            thread.session.flush_rollout().await?;
        }
        if persist_target_closed {
            self.persist_agent_closed(agent_id).await?;
        }

        // Explicit close is authoritative over passive and wake response observation for the
        // entire subtree. Revoke before shutdown so Shutdown cannot wake an old observer or
        // schedule V1 watcher recovery for a later runtime with the same rollout thread ID.
        // Persisted descendant edges deliberately remain Open: explicitly resuming the closed
        // target may reopen its prior subtree, but every response relationship below is fresh.
        let mut affected_wake_observers = HashSet::new();
        state
            .agent_turn_queue
            .cancel_for_threads(closed_thread_ids.iter().copied());
        for closed_thread_id in closed_thread_ids {
            state.advance_agent_lifecycle_generation(closed_thread_id);
            affected_wake_observers
                .extend(self.revoke_response_observations_for_child(closed_thread_id));
        }
        let result = Box::pin(self.shutdown_prepared_agent_tree_with_descendants(
            agent_id,
            target_thread,
            descendant_threads,
        ))
        .await;
        drop(_descendant_lifecycle_guards);
        drop(_lifecycle_guard);

        // A foreign observer may already be idle when this close removes its last outstanding
        // wake. Re-run idle lifecycle after shutdown so automatic work such as an active goal
        // cannot remain deferred indefinitely. Observers that are active, replaced, or themselves
        // part of the closed subtree reject the callback through the ordinary lifecycle gates.
        for observer in affected_wake_observers {
            self.recheck_thread_idle_lifecycle(observer).await;
        }

        match result {
            Err(err)
                if known_agent
                    && matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) =>
            {
                Ok(ClosedAgent { previous_status })
            }
            result => result.map(|_| ClosedAgent { previous_status }),
        }
    }

    async fn shutdown_prepared_agent_tree_with_descendants(
        &self,
        agent_id: ThreadId,
        target_thread: Option<Arc<CodexThread>>,
        descendant_threads: Vec<Arc<CodexThread>>,
    ) -> CodexResult<String> {
        let result = match target_thread {
            Some(thread) => {
                self.shutdown_prepared_live_agent_instance(
                    &thread,
                    LiveAgentMetadataDisposition::Release,
                )
                .await
            }
            None => {
                self.forget_v2_residency(agent_id);
                self.release_spawned_thread(SpawnedThreadRelease::AbsentThread(agent_id));
                Err(CodexErr::ThreadNotFound(agent_id))
            }
        };
        for thread in descendant_threads {
            match self
                .shutdown_prepared_live_agent_instance(
                    &thread,
                    LiveAgentMetadataDisposition::Release,
                )
                .await
            {
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
