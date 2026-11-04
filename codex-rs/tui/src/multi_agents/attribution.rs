//! Merge trusted message snapshots with optional navigation and replay metadata.

use super::AgentMetadata;
use super::AgentTaskPath;
use super::SpawnRequestSummary;
use codex_app_server_protocol::AgentInputIdentity;

impl AgentMetadata {
    pub(crate) fn with_identity(mut self, identity: &AgentInputIdentity) -> Self {
        if identity.agent_ref.is_some() {
            self.agent_ref.clone_from(&identity.agent_ref);
        }
        if identity.nickname.is_some() {
            self.agent_nickname.clone_from(&identity.nickname);
        }
        if identity.role.is_some() {
            self.agent_role.clone_from(&identity.role);
        }
        if let Some(task_path) = &identity.task_path {
            self.task_path = AgentTaskPath::Known(Some(task_path.clone()));
        }
        if identity.model.is_some() || identity.reasoning_effort.is_some() {
            let previous = self.spawn_request.unwrap_or_default();
            self.spawn_request = Some(SpawnRequestSummary {
                model: identity.model.clone().or(previous.model),
                reasoning_effort: identity
                    .reasoning_effort
                    .clone()
                    .or(previous.reasoning_effort),
            });
        }
        self
    }
}
