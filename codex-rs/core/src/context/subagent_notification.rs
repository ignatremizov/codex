use codex_protocol::ThreadId;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::AgentStatus;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use super::AgentContextIdentity;
use super::ContextualUserFragment;
use super::agent_envelope_projection::CanonicalEnvelopeIdentity;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubagentNotification {
    agent: AgentContextIdentity,
    pub(crate) status: AgentStatus,
}

impl SubagentNotification {
    pub(super) fn project_model_text(
        text: &str,
        aliases: &HashMap<ThreadId, u64>,
    ) -> Option<String> {
        #[derive(Deserialize)]
        struct Envelope {
            #[serde(flatten)]
            identity: CanonicalEnvelopeIdentity,
            status: AgentStatus,
        }

        let (start, end) = Self::type_markers();
        let body = text.strip_prefix(start)?.strip_suffix(end)?;
        let envelope: Envelope = serde_json::from_str(body).ok()?;
        let mut fields = envelope.identity.for_model(aliases)?.compact_json_fields();
        fields.insert("status".to_string(), serde_json::json!(envelope.status));
        let body = Value::Object(fields)
            .to_string()
            .replace('<', "\\u003c")
            .replace('>', "\\u003e");
        Some(format!("{start}\n{body}\n{end}"))
    }

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
