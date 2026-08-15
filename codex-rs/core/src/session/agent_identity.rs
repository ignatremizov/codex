//! Model-visible identity follows the current controller, not historical rollout parentage.

use super::Session;
use crate::context::AgentContextIdentity;
use codex_protocol::SessionId;
use codex_protocol::protocol::MultiAgentVersion;

/// Internal startup capability supplied only after controller ownership has been validated.
///
/// A pending transfer may precede its alias transaction, but must retain writer/lifecycle
/// reservations until that transaction and checked runtime publication complete.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AgentSessionOwnershipOverride {
    pub(crate) session_id: SessionId,
}

impl Session {
    pub(super) async fn current_model_visible_agent_identity(
        &self,
        multi_agent_version: MultiAgentVersion,
    ) -> AgentContextIdentity {
        self.services
            .agent_control
            .model_visible_agent_identity_for_version(multi_agent_version, self.thread_id)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    thread_id = %self.thread_id,
                    %error,
                    "failed to resolve source-relative agent identity"
                );
                AgentContextIdentity::Canonical {
                    agent_id: self.thread_id,
                }
            })
    }
}
