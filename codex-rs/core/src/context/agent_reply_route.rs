use codex_protocol::models::ContentItemKind;
use serde_json::Value;

use super::AgentContextIdentity;
use super::ContextualUserFragment;

/// Exact-turn route promoted by a caller that selected `w: m`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentReplyRoute {
    agent: AgentContextIdentity,
}

impl AgentReplyRoute {
    pub(crate) fn new(agent: AgentContextIdentity) -> Self {
        Self { agent }
    }
}

impl ContextualUserFragment for AgentReplyRoute {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.agent_reply_route".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<agent_reply_route>", "</agent_reply_route>")
    }

    fn body(&self) -> String {
        let mut fields = self.agent.json_fields();
        fields.insert(
            "send_input".to_string(),
            Value::String("allowed_this_turn".to_string()),
        );
        format!("\n{}\n", Value::Object(fields))
    }
}
