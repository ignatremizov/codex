use codex_protocol::models::ContentItemKind;
use serde_json::Value;

use super::AgentContextIdentity;
use super::ContextualUserFragment;

/// Attributed input accepted through an exact-turn agent reply route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttributedAgentMessage {
    agent: AgentContextIdentity,
    turn_id: String,
    message: String,
}

impl AttributedAgentMessage {
    pub(crate) fn new(
        agent: AgentContextIdentity,
        turn_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            agent,
            turn_id: turn_id.into(),
            message: message.into(),
        }
    }
}

impl ContextualUserFragment for AttributedAgentMessage {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.attributed_agent_message".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<agent_message>", "</agent_message>")
    }

    fn body(&self) -> String {
        let mut fields = self.agent.json_fields();
        fields.insert("turn_id".to_string(), Value::String(self.turn_id.clone()));
        fields.insert("message".to_string(), Value::String(self.message.clone()));
        format!("\n{}\n", Value::Object(fields))
    }
}
