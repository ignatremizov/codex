//! Explicit UUID adoption may hand off an idle independent root without a separate close call.

use super::LocalAgentControl;
use crate::thread_manager::AgentBranchHandoff;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

impl LocalAgentControl {
    pub(super) async fn prepare_live_root_adoption(&self, thread_id: ThreadId) -> CodexResult<()> {
        let manager = self.runtime.upgrade()?;
        if manager.get_thread(thread_id).await.is_err() {
            return Ok(());
        }
        let membership = self.runtime.admit_start()?;
        tokio::spawn(async move {
            let _membership = membership;
            // The handoff repeats the root-ownership check under the branch lifecycle locks.
            let branch = manager
                .unload_idle_agent_branch(thread_id, AgentBranchHandoff::AdoptRoot)
                .await?;
            drop(branch);
            Ok(())
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("idle root handoff failed: {error}")))?
    }
}
