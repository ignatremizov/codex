//! Admission-time runtime ownership, independent of durable sender attribution.

use super::*;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

impl LocalAgentControl {
    /// Logical root identity and the captured native allocation must agree.
    pub(in crate::agent::control) fn owns_messaging_runtime(
        &self,
        thread: &crate::CodexThread,
    ) -> bool {
        thread.session.session_id() == self.session_id()
            && self
                .runtime
                .shares_tree_with(&thread.session.services.local_agent_runtime)
    }

    pub(crate) async fn is_live_agent_descendant(
        &self,
        ancestor: ThreadId,
        descendant: ThreadId,
    ) -> CodexResult<bool> {
        let manager = self.runtime.upgrade()?;
        let mut cursor = descendant;
        let mut visited = HashSet::new();
        while visited.insert(cursor) {
            let target = manager.get_thread(cursor).await?;
            if !self.owns_messaging_runtime(&target) {
                return Ok(false);
            }
            let Some(parent) = target.session_source.parent_thread_id() else {
                return Ok(false);
            };
            if parent == ancestor {
                return Ok(true);
            }
            cursor = parent;
        }
        Ok(false)
    }

    /// Validate exact endpoints in the owned runtime at SessionIo admission.
    /// Human input never enters this check, including previously accepted human queues.
    /// Accepted queues may outlive the source turn, but not its runtime incarnation.
    pub(in crate::agent::control) async fn ensure_model_input_authorized(
        &self,
        sender: SessionPresentationId,
        recipient: SessionPresentationId,
    ) -> CodexResult<()> {
        let manager = self.runtime.upgrade()?;
        let source = manager.get_thread(sender.thread_id).await?;
        if source.session.presentation_id() != sender
            || !self.owns_messaging_runtime(&source)
            || sender == recipient
        {
            return Err(CodexErr::InvalidRequest(
                "agent input source is no longer current".into(),
            ));
        }
        let target = manager.get_thread(recipient.thread_id).await?;
        if target.session.presentation_id() != recipient || !self.owns_messaging_runtime(&target) {
            return Err(CodexErr::InvalidRequest(
                "agent input target is no longer current".into(),
            ));
        }
        Ok(())
    }
}
