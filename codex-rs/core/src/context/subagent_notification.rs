use codex_protocol::models::ContentItemKind;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubagentNotification {
    pub(crate) agent_reference: String,
    pub(crate) agent_id: ThreadId,
    pub(crate) status: AgentStatus,
}

impl SubagentNotification {
    pub(crate) fn new(
        agent_reference: impl Into<String>,
        agent_id: ThreadId,
        status: AgentStatus,
    ) -> Self {
        Self {
            agent_reference: agent_reference.into(),
            agent_id,
            status,
        }
    }
}

impl ContextualUserFragment for SubagentNotification {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.subagent_notification".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<subagent_notification>", "</subagent_notification>")
    }

    fn body(&self) -> String {
        format!(
            "\n{}\n",
            serde_json::json!({
                "agent_path": &self.agent_reference,
                "agent_id": self.agent_id,
                "status": &self.status,
            })
        )
    }
}
