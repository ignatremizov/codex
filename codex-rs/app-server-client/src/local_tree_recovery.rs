//! Local recovery is a host operation, never a remotely callable protocol method.

use crate::ClientCommand;
use crate::InProcessAppServerClient;
use crate::LocalTreeRecoveryOutcome;
use crate::PreparedLocalTreeRecovery;
use codex_protocol::ThreadId;
use std::io;
use tokio::sync::oneshot;

impl InProcessAppServerClient {
    /// Rebuilds an owning tree after confirmed death of its former local server.
    ///
    /// No interrupted turn starts until the returned plan is explicitly consumed.
    pub async fn prepare_local_tree_recovery(
        &self,
        thread_id: ThreadId,
    ) -> io::Result<PreparedLocalTreeRecovery> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::PrepareLocalTreeRecovery {
                thread_id,
                response_tx,
            })
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "embedded server worker closed")
            })?;
        response_rx.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "local recovery preparation channel closed",
            )
        })?
    }

    /// Continues recorded unfinished turns after the client has adopted this server.
    pub async fn continue_local_tree_recovery(
        &self,
        plan: PreparedLocalTreeRecovery,
    ) -> io::Result<LocalTreeRecoveryOutcome> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::ContinueLocalTreeRecovery {
                plan: Box::new(plan),
                response_tx,
            })
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "embedded server worker closed")
            })?;
        response_rx.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "local recovery continuation channel closed",
            )
        })?
    }
}
