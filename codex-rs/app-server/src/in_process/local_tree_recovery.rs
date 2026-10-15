//! Explicit, in-process-only recovery after an embedder confirms its old server exited.

use super::InProcessClientMessage;
use super::InProcessClientSender;
use crate::LocalTreeRecoveryOutcome;
use crate::PreparedLocalTreeRecovery;
use codex_protocol::ThreadId;
use std::io;
use tokio::sync::oneshot;

impl InProcessClientSender {
    /// Restores the persisted owning tree without starting interrupted work yet.
    ///
    /// The embedder must prove the prior server exited before requesting recovery.
    /// Ordinary thread writer and ancestry validation still apply to every member.
    pub async fn prepare_local_tree_recovery(
        &self,
        thread_id: ThreadId,
    ) -> io::Result<PreparedLocalTreeRecovery> {
        let (response_tx, response_rx) = oneshot::channel();
        self.try_send_client_message(InProcessClientMessage::PrepareLocalTreeRecovery {
            thread_id,
            response_tx,
        })?;
        response_rx.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "local recovery preparation channel closed",
            )
        })?
    }

    /// Consumes the server-bound preparation after the embedder adopts its event stream.
    pub async fn continue_local_tree_recovery(
        &self,
        plan: PreparedLocalTreeRecovery,
    ) -> io::Result<LocalTreeRecoveryOutcome> {
        let (response_tx, response_rx) = oneshot::channel();
        self.try_send_client_message(InProcessClientMessage::ContinueLocalTreeRecovery {
            plan: Box::new(plan),
            response_tx,
        })?;
        response_rx.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "local recovery continuation channel closed",
            )
        })?
    }
}
