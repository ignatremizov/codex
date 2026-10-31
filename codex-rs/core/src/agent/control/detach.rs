//! Model-requested detachment retains native ownership and root teardown admission.

use super::LocalAgentControl;
use crate::thread_manager::AgentDetachAuthority;
use crate::thread_manager::DetachedAgentBranch;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

impl LocalAgentControl {
    pub(crate) async fn detach_agent(
        &self,
        caller: ThreadId,
        target: ThreadId,
    ) -> CodexResult<DetachedAgentBranch> {
        if target == caller || target == ThreadId::from(self.session_id()) {
            return Err(CodexErr::InvalidRequest(
                "detachment must target another non-root agent".into(),
            ));
        }
        self.require_current_agent_ownership(target).await?;
        let manager = self.runtime.upgrade()?;
        manager
            .detach_agent_branch(
                target,
                AgentDetachAuthority::Agent {
                    session_id: self.session_id(),
                    caller,
                    _membership: self.runtime.admit_start()?,
                },
            )
            .await
    }
}
