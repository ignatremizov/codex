//! Resolves controller targets using the caller's registered identity.
//! Legacy callers can still supply their captured session source for path resolution.

use super::LocalAgentControl;
use super::LocalAgentRuntime;
use crate::agent::api::AgentTarget;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;

impl LocalAgentControl {
    pub(crate) async fn resolve_target(
        &self,
        caller: ThreadId,
        target: &AgentTarget,
    ) -> CodexResult<ThreadId> {
        match target {
            AgentTarget::Id(thread_id) => Ok(*thread_id),
            AgentTarget::Reference(reference) => {
                let metadata = self.runtime.ensure_agent_known(caller)?;
                self.resolve_controller_reference(
                    caller,
                    &metadata.agent_path.unwrap_or_else(AgentPath::root),
                    reference,
                )
                .await
            }
        }
    }
}

impl LocalAgentControl {
    /// Explicit selectors share the durable root-scoped owner used by V1 and user controls.
    /// Bare V2 names retain their original relative-path interpretation. A missing or denied
    /// explicit alias never falls back to a same-named path or another backend.
    pub(super) async fn resolve_controller_reference(
        &self,
        caller: ThreadId,
        caller_path: &AgentPath,
        reference: &str,
    ) -> CodexResult<ThreadId> {
        if ["id:", "ref:", "nick:", "task:"]
            .iter()
            .any(|prefix| reference.starts_with(prefix))
        {
            return self
                .resolve_controlled_agent_target(caller, reference)
                .await;
        }
        self.runtime.resolve_path_reference(caller_path, reference)
    }
}

impl LocalAgentRuntime {
    pub(crate) async fn resolve_agent_reference(
        &self,
        _current_thread_id: ThreadId,
        current_session_source: &SessionSource,
        agent_reference: &str,
    ) -> CodexResult<ThreadId> {
        let current_agent_path = current_session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        self.resolve_path_reference(&current_agent_path, agent_reference)
    }

    pub(super) fn resolve_path_reference(
        &self,
        current_agent_path: &AgentPath,
        agent_reference: &str,
    ) -> CodexResult<ThreadId> {
        let agent_path = current_agent_path
            .resolve(agent_reference)
            .map_err(CodexErr::UnsupportedOperation)?;
        if let Some(thread_id) = self.registry.agent_id_for_path(&agent_path) {
            return Ok(thread_id);
        }
        Err(CodexErr::UnsupportedOperation(format!(
            "live agent path `{}` not found",
            agent_path.as_str()
        )))
    }
}
