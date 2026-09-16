//! Transfers the existing provisional spawn owner only when its receipt is consumed.
//! Graph/input uncertainty stays with PendingSpawn instead of a second compensating owner.

use super::LocalAgentControl;
use super::spawn::SpawnInitialInput;
use super::spawn::SpawnedAgent;
use super::spawn_guard::PendingSpawn;
use crate::agent::types::SpawnAgentOptions;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(super) struct PreparedAgentSpawn {
    pub(super) spawned: SpawnedAgent,
    pub(super) cleanup: PendingSpawn,
}

impl PreparedAgentSpawn {
    pub(super) fn commit(self) -> SpawnedAgent {
        self.cleanup.commit();
        self.spawned
    }
}

impl LocalAgentControl {
    /// A failed compensation remains attached to this exact runtime for unload retry.
    pub(crate) async fn finish_cancelled_spawn_alias_cleanup(
        &self,
        thread: &Arc<CodexThread>,
    ) -> CodexResult<()> {
        if thread
            .cancelled_spawn_alias_cleanup_pending
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.persist_agent_closed(thread.session.thread_id())
                .await?;
            thread
                .cancelled_spawn_alias_cleanup_pending
                .store(false, std::sync::atomic::Ordering::Release);
        }
        Ok(())
    }

    pub(super) async fn spawn_with_receipt(
        &self,
        config: Config,
        initial_input: SpawnInitialInput,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<SpawnedAgent> {
        let control = self.clone();
        let abandoned = CancellationToken::new();
        let handoff = abandoned.clone().drop_guard();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let result = Box::pin(control.spawn_agent_prepared(
                config,
                initial_input,
                session_source,
                options,
                abandoned,
            ))
            .await;
            // The queued value remains armed. Both failed send and an unread result
            // release the same cleanup owner, rather than treating send as acknowledgement.
            let _ = sender.send(result);
        });
        let prepared = receiver.await.map_err(|_| CodexErr::InternalAgentDied)??;
        let spawned = prepared.commit();
        handoff.disarm();
        Ok(spawned)
    }
}
