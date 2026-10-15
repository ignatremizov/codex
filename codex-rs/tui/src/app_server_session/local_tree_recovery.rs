//! Host-only recovery cannot be forwarded to a remote app-server.

use super::AppServerSession;
use codex_app_server_client::AppServerClient;
use codex_app_server_client::LocalTreeRecoveryOutcome;
use codex_app_server_client::PreparedLocalTreeRecovery;
use codex_protocol::ThreadId;
use color_eyre::eyre::Result;

impl AppServerSession {
    pub(crate) async fn prepare_local_tree_recovery(
        &self,
        thread_id: ThreadId,
    ) -> Result<PreparedLocalTreeRecovery> {
        let AppServerClient::InProcess(client) = &self.client else {
            color_eyre::eyre::bail!("local tree recovery requires an embedded server");
        };
        Ok(client.prepare_local_tree_recovery(thread_id).await?)
    }

    pub(crate) async fn continue_local_tree_recovery(
        &self,
        plan: PreparedLocalTreeRecovery,
    ) -> Result<LocalTreeRecoveryOutcome> {
        let AppServerClient::InProcess(client) = &self.client else {
            color_eyre::eyre::bail!("local tree recovery requires an embedded server");
        };
        Ok(client.continue_local_tree_recovery(plan).await?)
    }
}
