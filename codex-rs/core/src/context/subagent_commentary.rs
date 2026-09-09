use codex_protocol::ThreadId;
use codex_protocol::models::ContentItemKind;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use super::AgentContextIdentity;
use super::ContextualUserFragment;
use super::agent_envelope_projection::CanonicalEnvelopeIdentity;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentCommentary {
    agent: AgentContextIdentity,
    turn_id: String,
    item_id: String,
    message: String,
}

impl SubagentCommentary {
    pub(super) fn project_model_text(
        text: &str,
        aliases: &HashMap<ThreadId, u64>,
    ) -> Option<String> {
        #[derive(Deserialize)]
        struct Envelope {
            #[serde(flatten)]
            identity: CanonicalEnvelopeIdentity,
            message: String,
        }

        let (start, end) = Self::type_markers();
        let body = text.strip_prefix(start)?.strip_suffix(end)?;
        let envelope: Envelope = serde_json::from_str(body).ok()?;
        let mut fields = envelope.identity.for_model(aliases)?.compact_json_fields();
        fields.insert("message".to_string(), Value::String(envelope.message));
        let body = Value::Object(fields)
            .to_string()
            .replace('<', "\\u003c")
            .replace('>', "\\u003e");
        Some(format!("{start}\n{body}\n{end}"))
    }

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
