//! Typed status authority for UUID mailbox selection, independent of adoption.

use super::AgentControl;
use codex_agent_graph_store::AgentAliasState;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum V1WaitStatusAuthority {
    Controlled,
    MailOnly,
}

#[cfg(test)]
#[path = "wait_status_authority_tests.rs"]
mod tests;

impl AgentControl {
    /// Classifies UUID status access without loading foreign status or adopting it.
    /// Mail selection itself is independent of this lifecycle authority.
    pub(crate) async fn v1_wait_status_authority(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<V1WaitStatusAuthority> {
        let Ok(state) = self.upgrade() else {
            return Ok(V1WaitStatusAuthority::MailOnly);
        };
        if let Some(session_id) = self.bound_session_id()
            && let Some(store) = state
                .agent_graph_store()
                .filter(|store| store.supports_agent_aliases())
        {
            store
                .ensure_agent_alias_namespace(session_id)
                .await
                .map_err(|err| {
                    CodexErr::Fatal(format!(
                        "failed to initialize durable agent aliases for {session_id}: {err}"
                    ))
                })?;
            if let Some(alias) = self.find_session_agent_alias(thread_id).await? {
                return Ok(match alias.state {
                    AgentAliasState::Active | AgentAliasState::Closed => {
                        V1WaitStatusAuthority::Controlled
                    }
                    AgentAliasState::Transferred => V1WaitStatusAuthority::MailOnly,
                });
            }
            return Ok(
                if self.get_agent_metadata(thread_id).is_some()
                    && self.has_ephemeral_runtime(thread_id).await
                {
                    V1WaitStatusAuthority::Controlled
                } else {
                    V1WaitStatusAuthority::MailOnly
                },
            );
        }
        // Retain the existing compatibility behavior for controls without a
        // durable root namespace. No stored foreign thread needs to be read.
        Ok(
            if self.get_agent_metadata(thread_id).is_some()
                || (self.bound_session_id().is_none()
                    && state.get_thread_including_pending(thread_id).await.is_ok())
            {
                V1WaitStatusAuthority::Controlled
            } else {
                V1WaitStatusAuthority::MailOnly
            },
        )
    }

    pub(super) async fn has_ephemeral_runtime(&self, thread_id: ThreadId) -> bool {
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(thread) = state.get_thread_including_pending(thread_id).await else {
            return false;
        };
        thread.config_snapshot().await.ephemeral
    }
}
