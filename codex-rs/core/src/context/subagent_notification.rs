use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::AgentStatus;
use serde_json::Value;

use super::AgentContextIdentity;
use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubagentNotification {
    agent: AgentContextIdentity,
    pub(crate) status: AgentStatus,
}

impl SubagentNotification {
    pub(crate) fn new(agent: AgentContextIdentity, status: AgentStatus) -> Self {
        Self { agent, status }
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
        let mut fields = self.agent.json_fields();
        fields.insert("status".to_string(), serde_json::json!(&self.status));
        format!("\n{}\n", Value::Object(fields))
    }
}
