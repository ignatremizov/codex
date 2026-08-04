use codex_protocol::ThreadId;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentCommentary {
    agent_reference: String,
    agent_id: ThreadId,
    turn_id: String,
    item_id: String,
    message: String,
}

impl SubagentCommentary {
    pub(crate) fn new(
        agent_reference: impl Into<String>,
        agent_id: ThreadId,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            agent_reference: agent_reference.into(),
            agent_id,
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            message: message.into(),
        }
    }
}

impl ContextualUserFragment for SubagentCommentary {
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
        format!(
            "\n{}\n",
            serde_json::json!({
                "agent_path": &self.agent_reference,
                "agent_id": self.agent_id,
                "turn_id": &self.turn_id,
                "item_id": &self.item_id,
                "message": &self.message,
            })
        )
    }
}
