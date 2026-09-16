//! Stabilize an owning spawn subtree without closing its durable agent identities.

use super::ThreadManager;
use super::ThreadManagerState;
use crate::codex_thread::CodexThread;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ThreadStoreError;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::OwnedMutexGuard;

pub(crate) enum ThreadRemovalAuthority {
    Ordinary,
    DurableUnload,
}

/// Owns the existing lifecycle locks and exact runtime instances of one spawn subtree.
///
/// Callers must finish subscription/authorization preflight before invoking shutdown. Dropping
/// this guard before shutdown does not mutate runtime state. A failed shutdown retains every
/// manager entry, including already-stopped actors, so another attempt can complete cleanup.
pub struct LoadedSubtree {
    state: Arc<ThreadManagerState>,
    root_thread_id: ThreadId,
    thread_ids: Vec<ThreadId>,
    threads: Vec<Arc<CodexThread>>,
    guard_ids: Vec<ThreadId>,
    guards: Vec<OwnedMutexGuard<()>>,
}

impl LoadedSubtree {
    pub fn root_thread_id(&self) -> ThreadId {
        self.root_thread_id
    }

    /// Loaded runtime IDs captured while membership was fenced, in parent-before-child order.
    pub fn thread_ids(&self) -> &[ThreadId] {
        &self.thread_ids
    }

    /// Durably stop every captured actor, then remove only those exact runtime instances.
    ///
    /// All drains settle before errors are returned. No entry is removed on drain failure.
    /// Ordinary spawn aliases and history are preserved. A retained cancelled spawn first
    /// completes its separate alias rollback obligation.
    pub async fn shutdown_and_remove(&mut self) -> CodexResult<Vec<ThreadId>> {
        {
            let current = self.state.threads.write().await;
            for thread in &self.threads {
                let id = thread.session.thread_id();
                let is_current = current
                    .get(&id)
                    .is_some_and(|loaded| Arc::ptr_eq(loaded, thread));
                if !is_current {
                    return Err(CodexErr::InvalidRequest(format!(
                        "thread {id} changed before unload admission; retry thread/unload"
                    )));
                }
            }
            for thread in &self.threads {
                thread.session.submission_admission.seal_for_unload();
            }
        }
        // Sealing membership is separate from draining deliveries. V1 terminal delivery
        // holds an accepted-completion token while acquiring a child's lifecycle lock.
        // Keeping those locks here would deadlock its parent's shutdown admission.
        self.guards.clear();
        let results = futures::future::join_all(self.threads.iter().map(|thread| async {
            if !thread.io.durable_shutdown_succeeded() {
                thread.ensure_rollout_materialized().await;
                thread.flush_rollout().await?;
            }
            thread.shutdown_durably_and_wait().await?;
            thread
                .session
                .services
                .agent_control
                .finish_cancelled_spawn_alias_cleanup(thread)
                .await
        }))
        .await;
        for result in results {
            result?;
        }
        for thread_id in &self.guard_ids {
            self.guards.push(
                self.state
                    .agent_lifecycle_lock(*thread_id)
                    .lock_owned()
                    .await,
            );
        }
        let mut removed = Vec::new();
        for thread in &self.threads {
            if thread
                .session
                .services
                .agent_control
                .remove_durably_unloaded_instance(thread)
                .await?
            {
                removed.push(thread.session.thread_id());
            }
        }
        Ok(removed)
    }
}

impl ThreadManager {
    /// Capture the owning root and its loaded descendants behind existing lifecycle boundaries.
    ///
    /// Persisted spawn ownership is authoritative; ordinary fork ancestry is not ownership.
    /// Known unloaded subtrees produce an empty capture, permitting lost-acknowledgement retries.
    pub async fn prepare_subtree_unload(&self, thread_id: ThreadId) -> CodexResult<LoadedSubtree> {
        let initial_ancestry = self.state.spawn_ancestry(thread_id).await?;
        if self.state.get_thread(thread_id).await.is_err() && initial_ancestry.len() == 1 {
            self.state
                .thread_store
                .read_thread(ReadThreadParams {
                    thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
                .map_err(|error| match error {
                    ThreadStoreError::ThreadNotFound { .. } => CodexErr::ThreadNotFound(thread_id),
                    error => CodexErr::Fatal(format!("read thread before unload: {error}")),
                })?;
        }
        loop {
            let ancestry = self.state.spawn_ancestry(thread_id).await?;
            let root_thread_id = *ancestry
                .last()
                .ok_or_else(|| CodexErr::Fatal("thread spawn ancestry is empty".to_string()))?;
            let mut guards = vec![
                self.state
                    .agent_lifecycle_lock(root_thread_id)
                    .lock_owned()
                    .await,
            ];
            if self.state.spawn_ancestry(thread_id).await? != ancestry {
                continue;
            }
            let mut locked = HashSet::from([root_thread_id]);
            let mut ordered_ids = vec![root_thread_id];
            loop {
                let mut discovered = self.list_agent_subtree_thread_ids(root_thread_id).await?;
                // A parent may already be idle-unloaded while its child remains loaded.
                // Persisted source ancestry bridges that gap even without an agent-graph store.
                for loaded_id in self.state.list_thread_ids().await {
                    let ancestry = self.state.spawn_ancestry(loaded_id).await?;
                    if ancestry.last() == Some(&root_thread_id) {
                        discovered.extend(ancestry);
                    }
                }
                let mut additions = Vec::new();
                let mut discovered_once = HashSet::new();
                for descendant in discovered {
                    if !locked.contains(&descendant) && discovered_once.insert(descendant) {
                        let ancestry = self.state.spawn_ancestry(descendant).await?;
                        if ancestry.last() == Some(&root_thread_id) {
                            additions.push((ancestry.len(), descendant));
                        }
                    }
                }
                additions.sort_by_key(|(depth, id)| (*depth, id.to_string()));
                if additions.is_empty() {
                    break;
                }
                for (_, descendant) in additions {
                    guards.push(
                        self.state
                            .agent_lifecycle_lock(descendant)
                            .lock_owned()
                            .await,
                    );
                    locked.insert(descendant);
                    ordered_ids.push(descendant);
                }
            }
            // Ownership may have changed while a descendant boundary was being acquired.
            // Release and retry rather than unloading a stale ownership snapshot.
            let mut stale = self.state.spawn_ancestry(thread_id).await? != ancestry;
            for descendant in &ordered_ids {
                stale |=
                    self.state.spawn_ancestry(*descendant).await?.last() != Some(&root_thread_id);
            }
            if stale {
                continue;
            }
            let mut threads = Vec::new();
            let mut thread_ids = Vec::new();
            for id in &ordered_ids {
                if let Ok(thread) = self.state.get_thread(*id).await {
                    thread_ids.push(*id);
                    threads.push(thread);
                }
            }
            return Ok(LoadedSubtree {
                state: Arc::clone(&self.state),
                root_thread_id,
                thread_ids,
                threads,
                guard_ids: ordered_ids,
                guards,
            });
        }
    }
}

impl ThreadManagerState {
    pub(crate) async fn ensure_membership_mutation_allowed(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<()> {
        match self.get_thread(thread_id).await {
            Ok(thread) => thread.ensure_not_unloading()?,
            Err(error) if matches!(error.details(), CodexErrorDetails::ThreadNotFound(_)) => {}
            Err(error) => return Err(error),
        }
        // A cold root has no runtime on which to store the seal. Surviving sealed children
        // still own that subtree's unload boundary, so the ancestor must not be recreated.
        // Callers hold the target's lifecycle guard; no descendant lock is acquired here.
        let sealed = self
            .threads
            .read()
            .await
            .values()
            .filter(|thread| thread.ensure_not_unloading().is_err())
            .cloned()
            .collect::<Vec<_>>();
        for thread in sealed {
            if self
                .spawn_ancestry(thread.session.thread_id())
                .await?
                .contains(&thread_id)
            {
                thread.ensure_not_unloading()?;
            }
        }
        Ok(())
    }

    /// Retain a failed spawn rollback in the existing runtime inventory for unload retry.
    ///
    /// The caller keeps its parent and child lifecycle guards until ownership is transferred.
    pub(crate) async fn retain_failed_spawn_cleanup(
        &self,
        thread: &Arc<CodexThread>,
    ) -> CodexResult<()> {
        let id = thread.session.thread_id();
        let mut current = self.threads.write().await;
        if current
            .get(&id)
            .is_some_and(|loaded| !Arc::ptr_eq(loaded, thread))
        {
            return Err(CodexErr::InvalidRequest(format!(
                "thread {id} changed during failed spawn cleanup"
            )));
        }
        thread.session.submission_admission.seal_for_unload();
        current.insert(id, Arc::clone(thread));
        Ok(())
    }

    async fn spawn_ancestry(&self, thread_id: ThreadId) -> CodexResult<Vec<ThreadId>> {
        let mut ancestry = Vec::new();
        let mut seen = HashSet::new();
        let mut current = thread_id;
        loop {
            if !seen.insert(current) {
                return Err(CodexErr::Fatal(format!(
                    "cycle in thread spawn ownership at {current}"
                )));
            }
            ancestry.push(current);
            let persisted_parent = match self.agent_graph_store() {
                Some(store) => store
                    .find_thread_spawn_parent(current)
                    .await
                    .map_err(|error| {
                        CodexErr::Fatal(format!("read thread spawn ownership: {error}"))
                    })?,
                None => None,
            };
            let parent = match persisted_parent {
                Some(parent) => Some(parent),
                None => {
                    let source = match self.get_thread(current).await {
                        Ok(thread) => Some(thread.session_source.clone()),
                        Err(_) => match self
                            .thread_store
                            .read_thread(ReadThreadParams {
                                thread_id: current,
                                include_archived: true,
                                include_history: false,
                            })
                            .await
                        {
                            Ok(stored) => Some(stored.source),
                            Err(ThreadStoreError::ThreadNotFound { .. }) => None,
                            Err(error) => {
                                return Err(CodexErr::Fatal(format!(
                                    "read persisted thread spawn ancestry: {error}"
                                )));
                            }
                        },
                    };
                    source.and_then(|source| match source {
                        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id,
                            ..
                        }) => Some(parent_thread_id),
                        _ => None,
                    })
                }
            };
            match parent {
                Some(parent) => current = parent,
                None => return Ok(ancestry),
            }
        }
    }
}

#[cfg(test)]
#[path = "loaded_subtree_tests.rs"]
mod tests;
