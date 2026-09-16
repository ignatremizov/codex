//! Accepted spawn owns initialization and rollback independently of its response waiter.

use super::AgentControl;
use super::LiveAgent;
use super::SpawnAgentOptions;
use super::setup_cleanup::SetupCleanupGuard;
use super::spawn::SpawnInitialInput;
use crate::config::Config;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(super) struct PreparedAgentSpawn {
    pub(super) agent: LiveAgent,
    pub(super) cleanup: SetupCleanupGuard,
    pub(super) state: Arc<ThreadManagerState>,
}

impl AgentControl {
    /// Complete a cancelled spawn's alias rollback before its retained runtime can be removed.
    pub(crate) async fn finish_cancelled_spawn_alias_cleanup(
        &self,
        thread: &crate::codex_thread::CodexThread,
    ) -> CodexResult<()> {
        if thread
            .cancelled_spawn_alias_cleanup_pending
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.persist_agent_closed(thread.session.thread_id())
                .await?;
            thread
                .cancelled_spawn_alias_cleanup_pending
                .store(/*val*/ false, std::sync::atomic::Ordering::Release);
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
        let control = self.clone();
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let (response, result) = oneshot::channel();
        tokio::spawn(async move {
            let prepared = control
                .spawn_agent_prepared(config, initial_input, source, options, cancellation)
                .await;
            // A dropped receiver drops the still-armed cleanup value. Its captured
            // lifecycle guards stay owned until rollback completes or transfers ownership.
            if let Err(undelivered) = response.send(prepared) {
                drop(undelivered);
            }
        });
        let prepared = result.await.map_err(|_| CodexErr::InternalAgentDied)??;
        // No await between consuming the response, announcing publication, and commit.
        // Cancellation while queued in the oneshot still drops an armed cleanup guard.
        prepared
            .state
            .notify_thread_created(prepared.agent.thread_id);
        prepared.cleanup.disarm();
        Ok(prepared.agent)
    }
}
