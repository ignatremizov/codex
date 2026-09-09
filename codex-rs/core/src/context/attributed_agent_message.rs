use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use super::AgentContextIdentity;
use super::ContextualUserFragment;
use super::agent_envelope_projection::CanonicalEnvelopeIdentity;

pub(super) const ATTRIBUTED_AGENT_MESSAGE_KIND: &str = "multi_agent.attributed_agent_message";

/// Model-authored input with a trusted send-time identity snapshot.
///
/// The canonical envelope retains identity for recovery; only its model projection is compact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttributedAgentMessage {
    agent: AgentContextIdentity,
    message: String,
}

impl AttributedAgentMessage {
    /// Marks already annotated input at a trusted attributed-input recording boundary.
    ///
    /// This never infers attribution from text and does not create missing annotations.
    /// Callers must establish typed attribution before replacing the first content kind.
    pub(crate) fn mark_model_input(item: &mut ResponseItem) {
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = item
        else {
            return;
        };
        if role != "user" || !matches!(content.first(), Some(ContentItem::InputText { .. })) {
            return;
        }
        if let Some(kind) = metadata
            .content_item_kinds
            .as_mut()
            .and_then(|kinds| kinds.first_mut())
        {
            *kind = ContentItemKind(ATTRIBUTED_AGENT_MESSAGE_KIND.to_string());
        }
    }

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

    pub(crate) fn new(agent: AgentContextIdentity, message: impl Into<String>) -> Self {
        Self {
            agent,
            message: message.into(),
        }
    }
}

impl ContextualUserFragment for AttributedAgentMessage {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(ATTRIBUTED_AGENT_MESSAGE_KIND.to_string())
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
