use codex_protocol::models::ContentItemKind;
use serde_json::Value;

use super::AgentContextIdentity;
use super::ContextualUserFragment;

/// Model-authored input with a trusted send-time identity snapshot.
///
/// Canonical identities and source-turn metadata belong to the separate audit presentation,
/// not this compact model-visible envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttributedAgentMessage {
    agent: AgentContextIdentity,
    message: String,
}

impl AttributedAgentMessage {
    pub(crate) fn new(agent: AgentContextIdentity, message: impl Into<String>) -> Self {
        Self {
            agent,
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
        fields.remove("agent_id");
        fields.insert("message".to_string(), Value::String(self.message.clone()));
        // JSON quotes/newlines protect header values and payload boundaries. Escape angle
        // brackets too: serde_json otherwise leaves embedded envelope markers literal.
        // These JSON escapes decode to the exact original text, without a payload cap.
        let body = Value::Object(fields)
            .to_string()
            .replace('<', "\\u003c")
            .replace('>', "\\u003e");
        format!("\n{body}\n")
    }
}

#[cfg(test)]
#[path = "attributed_agent_message_tests.rs"]
mod tests;
