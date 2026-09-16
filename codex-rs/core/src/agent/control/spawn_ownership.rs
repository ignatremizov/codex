//! A cancelled spawn retains its exact runtime and lifecycle gate until durable cleanup.

use super::LocalAgentControl;
use super::spawn::SpawnInitialInput;
use super::spawn::SpawnedAgent;
use crate::CodexThread;
use crate::agent::types::LiveAgent;
use crate::agent::types::SpawnAgentOptions;
use crate::config::Config;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(super) struct PreparedAgentSpawn {
    pub(super) spawned: SpawnedAgent,
    pub(super) cleanup: SpawnCleanup,
}

impl PreparedAgentSpawn {
    pub(super) fn commit(mut self) -> SpawnedAgent {
        if let Some(cleanup) = self.cleanup.pending.take() {
            cleanup
                .thread
                .cancelled_spawn_alias_cleanup_pending
                .store(/*val*/ false, Ordering::Release);
            cleanup
                .state
                .notify_thread_created(self.spawned.agent.thread_id);
        }
        self.spawned
    }
}

pub(super) struct SpawnCleanup {
    pending: Option<PendingCleanup>,
}

struct PendingCleanup {
    control: LocalAgentControl,
    state: Arc<ThreadManagerState>,
    thread: Arc<CodexThread>,
    _parent_guard: Option<OwnedMutexGuard<()>>,
    child_guard: Option<OwnedMutexGuard<()>>,
}

impl SpawnCleanup {
    pub(super) fn new(
        control: LocalAgentControl,
        state: Arc<ThreadManagerState>,
        thread: Arc<CodexThread>,
        parent_guard: Option<OwnedMutexGuard<()>>,
        child_guard: OwnedMutexGuard<()>,
    ) -> Self {
        Self {
            pending: Some(PendingCleanup {
                control,
                state,
                thread,
                _parent_guard: parent_guard,
                child_guard: Some(child_guard),
            }),
        }
    }

    /// The current input dispatcher acquires the child gate itself.
    pub(super) fn release_input_gate(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.child_guard.take();
        }
    }

    pub(super) async fn reacquire_input_gate(&mut self) -> CodexResult<()> {
        if let Some(pending) = &mut self.pending {
            let id = pending.thread.session.thread_id();
            pending.child_guard = Some(pending.state.agent_lifecycle_lock(id).lock_owned().await);
            let current = pending.state.get_thread(id).await?;
            if !Arc::ptr_eq(&current, &pending.thread) || !current.is_running() {
                return Err(CodexErr::InvalidRequest(
                    "spawned runtime retired before handoff".into(),
                ));
            }
            current.ensure_not_unloading()?;
        }
        Ok(())
    }

    pub(super) async fn rollback(mut self) -> CodexResult<()> {
        match self.pending.take() {
            Some(pending) => pending.run().await,
            None => Ok(()),
        }
    }
}

impl Drop for SpawnCleanup {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            tokio::spawn(async move {
                if let Err(error) = pending.run().await {
                    tracing::warn!(%error, "cancelled spawn retained for durable unload retry");
                }
            });
        }
    }
}

impl PendingCleanup {
    async fn run(self) -> CodexResult<()> {
        self.thread.session.submission_admission.seal_for_unload();
        let disarm = self.thread.session.disarm_terminal_presentation();
        self.thread
            .session
            .retire_agent_status_observers(crate::session::AgentStatusRetirement::RestoreRollback);
        let shutdown = self.thread.shutdown_durably_and_wait().await;
        let alias_cleanup = self
            .control
            .finish_cancelled_spawn_alias_cleanup(&self.thread)
            .await;
        let result = async {
            shutdown?;
            alias_cleanup?;
            self.control
                .remove_durably_unloaded_instance(&self.thread)
                .await?;
            Ok(())
        }
        .await;
        disarm.commit();
        if result.is_err() {
            self.state.retain_failed_spawn_cleanup(&self.thread).await?;
        }
        result
    }
}

impl LocalAgentControl {
    /// Finish a cancelled creation's alias rollback before removing its retained runtime.
    pub(crate) async fn finish_cancelled_spawn_alias_cleanup(
        &self,
        thread: &CodexThread,
    ) -> CodexResult<()> {
        if thread
            .cancelled_spawn_alias_cleanup_pending
            .load(Ordering::Acquire)
        {
            self.persist_agent_closed(thread.session.thread_id())
                .await?;
            thread
                .cancelled_spawn_alias_cleanup_pending
                .store(/*val*/ false, Ordering::Release);
        }
        Ok(())
    }

    pub(super) async fn spawn_agent_internal(
        &self,
        config: Config,
        initial_input: SpawnInitialInput,
        source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        let spawned = self
            .spawn_with_receipt(config, initial_input, source, options)
            .await?;
        match spawned.post_admission_warning {
            Some(warning) => Err(CodexErr::InvalidRequest(format!(
                "agent {} exists and its input must not be resent: {warning}",
                spawned.agent.thread_id,
            ))),
            None => Ok(spawned.agent),
        }
    }

    pub(super) async fn spawn_with_receipt(
        &self,
        config: Config,
        initial_input: SpawnInitialInput,
        source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<SpawnedAgent> {
        let control = self.clone();
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let (response, result) = oneshot::channel();
        tokio::spawn(async move {
            let prepared = Box::pin(control.spawn_agent_prepared(
                config,
                initial_input,
                source,
                options,
                cancellation,
            ))
            .await;
            // An unread queued receipt still owns its armed cleanup and parent gate.
            if let Err(undelivered) = response.send(prepared) {
                drop(undelivered);
            }
        });
        Ok(result
            .await
            .map_err(|_| CodexErr::InternalAgentDied)??
            .commit())
    }
}
