//! Durable history forks reserve old short targets before exposing their new root runtime.
//!
//! Reservation runs independently of the caller, so cancellation cannot race its
//! cleanup. Unpublished startup, the lifecycle fence and alias rollback remain
//! owned until durable cleanup succeeds.
//! Ephemeral roots and context-forked children retain their existing startup path.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use codex_agent_graph_store::AgentGraphStore;
use codex_agent_graph_store::ReserveForkAgentAliasesRequest;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use futures::FutureExt;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::NewThread;
use super::StartThreadOptions;
use super::ThreadManager;
use super::ThreadRegistration;
use super::ThreadSpawnRequest;
use super::ThreadSpawnResult;
use crate::session::AgentStatusRetirement;
use crate::session::startup::SessionStartup;

struct PreparedHistoryFork {
    spawned: ThreadSpawnResult,
    cleanup: ForkCleanup,
}

struct ForkCleanup {
    pending: Option<PendingForkCleanup>,
}

struct PendingForkCleanup {
    startup: Arc<SessionStartup>,
    graph: Arc<dyn AgentGraphStore>,
    reserved_namespace: Option<SessionId>,
    _lifecycle: OwnedMutexGuard<()>,
}

impl Drop for ForkCleanup {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            tokio::spawn(async move {
                // Explicitly capture the guard: disjoint async captures must not
                // release it when only the other pending fields are used below.
                let _lifecycle = pending._lifecycle;
                if let Some(session) = pending.startup.session.get() {
                    session.submission_admission.seal_for_unload();
                    session.disarm_terminal_presentation().commit();
                    session.retire_agent_status_observers(AgentStatusRetirement::RestoreRollback);
                }
                loop {
                    let result = AssertUnwindSafe(async {
                        pending.startup.cleanup_durably().await?;
                        if let Some(namespace) = pending.reserved_namespace {
                            pending
                                .graph
                                .discard_fork_agent_alias_reservations(namespace)
                                .await
                                .map_err(|error| {
                                    CodexErr::Fatal(format!(
                                        "failed to discard unpublished fork aliases: {error}"
                                    ))
                                })?;
                        }
                        CodexResult::Ok(())
                    })
                    .catch_unwind()
                    .await
                    .unwrap_or(Err(CodexErr::InternalAgentDied));
                    match result {
                        Ok(()) => break,
                        Err(error) => {
                            tracing::warn!(%error, "unpublished fork cleanup failed; retaining owner");
                            tokio::time::sleep(Duration::from_secs(/*secs*/ 1)).await;
                        }
                    }
                }
            });
        }
    }
}

impl ThreadManager {
    pub(super) async fn fork_alias_source(
        &self,
        options: &StartThreadOptions,
        history: &InitialHistory,
        source_thread_id: Option<ThreadId>,
    ) -> CodexResult<Option<(Arc<dyn AgentGraphStore>, SessionId)>> {
        if options.config.ephemeral
            || options
                .session_source
                .as_ref()
                .unwrap_or(&self.state.session_source)
                .is_non_root_agent()
        {
            return Ok(None);
        }
        let Some(source_thread_id) = source_thread_id else {
            return Ok(None);
        };
        let Some(graph) = self
            .state
            .agent_graph_store()
            .filter(|graph| graph.supports_agent_aliases())
        else {
            return Ok(None);
        };
        if let Ok(source) = self.state.get_thread(source_thread_id).await {
            return Ok(Some((graph, source.session.session_id())));
        }
        if let Some(alias) = graph
            .find_current_agent_alias_by_thread(source_thread_id)
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("failed to resolve fork alias owner: {error}"))
            })?
        {
            return Ok(Some((graph, alias.session_id)));
        }
        let mut root = source_thread_id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(root) {
                return Err(CodexErr::InvalidRequest(format!(
                    "fork source {source_thread_id} has cyclic persisted ancestry at {root}"
                )));
            }
            let Some(parent) = graph
                .find_thread_spawn_parent(root)
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!("failed to resolve fork ancestry: {error}"))
                })?
            else {
                break;
            };
            root = parent;
        }
        let source_session_id = if root == source_thread_id {
            history
                .get_rollout_items()
                .iter()
                .rev()
                .find_map(|item| {
                    if let RolloutItem::SessionMeta(meta) = item
                        && meta.meta.id == source_thread_id
                    {
                        Some(meta.meta.session_id)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| SessionId::from(source_thread_id))
        } else {
            SessionId::from(root)
        };
        Ok(Some((graph, source_session_id)))
    }

    pub(super) async fn spawn_fork_with_aliases(
        &self,
        mut request: ThreadSpawnRequest,
        graph: Arc<dyn AgentGraphStore>,
        source_session_id: SessionId,
    ) -> CodexResult<NewThread> {
        let state = Arc::clone(&self.state);
        let thread_id = request
            .options
            .reserved_thread_id
            .unwrap_or_else(|| self.reserve_thread_id());
        request.options.reserved_thread_id = Some(thread_id);
        request.registration = ThreadRegistration::Deferred;
        let abandoned = CancellationToken::new();
        let handoff = abandoned.clone().drop_guard();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let result = async {
                let lifecycle = state.agent_lifecycle_lock(thread_id).lock_owned().await;
                if state.get_thread(thread_id).await.is_ok() {
                    return Err(CodexErr::InvalidRequest(format!(
                        "fork thread {thread_id} is already running"
                    )));
                }
                let startup = Arc::new(SessionStartup::default());
                request.startup = Some(Arc::clone(&startup));
                let mut cleanup = ForkCleanup {
                    pending: Some(PendingForkCleanup {
                        startup,
                        graph: Arc::clone(&graph),
                        reserved_namespace: None,
                        _lifecycle: lifecycle,
                    }),
                };
                let spawned = tokio::select! {
                    biased;
                    _ = abandoned.cancelled() => return Err(CodexErr::TurnAborted),
                    result = Box::pin(state.spawn_thread(request)) => result?,
                };
                let namespace = spawned.thread.session.session_id();
                if let Some(pending) = &mut cleanup.pending {
                    // Even an error can follow an ambiguous commit. Cleanup must retain
                    // this exact namespace before issuing the storage operation.
                    pending.reserved_namespace = Some(namespace);
                }
                graph
                    .reserve_agent_aliases_for_fork(ReserveForkAgentAliasesRequest {
                        source_session_id,
                        fork_session_id: namespace,
                    })
                    .await
                    .map_err(|error| {
                        CodexErr::Fatal(format!(
                            "failed to reserve inherited aliases for fork {thread_id}: {error}"
                        ))
                    })?;
                if abandoned.is_cancelled() {
                    return Err(CodexErr::TurnAborted);
                }
                Ok(PreparedHistoryFork { spawned, cleanup })
            }
            .await;
            // A queued result remains armed until its caller publishes it. Dropping an
            // unread result or failing the send starts the same retained cleanup.
            let _ = sender.send(result);
        });
        let mut prepared = receiver.await.map_err(|_| CodexErr::InternalAgentDied)??;
        if let Some(pending) = &prepared.cleanup.pending {
            pending.startup.persistence.lock().await.commit();
        }
        self.state
            .publish_restored_thread(&prepared.spawned.thread, /*parent*/ None, || Ok(()))
            .await?;
        // No await separates publication and ownership transfer.
        prepared.cleanup.pending.take();
        handoff.disarm();
        self.state.notify_thread_created(prepared.spawned.thread_id);
        Ok(prepared.spawned.into_new_thread())
    }
}
