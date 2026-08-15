use codex_protocol::models::ContentItemKind;
use serde_json::Value;

use super::AgentContextIdentity;
use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentCommentary {
    agent: AgentContextIdentity,
    turn_id: String,
    item_id: String,
    message: String,
}

impl SubagentCommentary {
    pub(crate) fn new(
        agent: AgentContextIdentity,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            agent,
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            message: message.into(),
        }
    }
}

impl ContextualUserFragment for SubagentCommentary {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.subagent_commentary".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<subagent_commentary>", "</subagent_commentary>")
    }

    fn body(&self) -> String {
        let mut fields = self.agent.json_fields();
        fields.insert("turn_id".to_string(), Value::String(self.turn_id.clone()));
        fields.insert("item_id".to_string(), Value::String(self.item_id.clone()));
        fields.insert("message".to_string(), Value::String(self.message.clone()));
        format!("\n{}\n", Value::Object(fields))
    }
}
