use codex_protocol::ThreadId;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND;
use serde_json::Value;

use super::AgentContextIdentity;
use super::ContextualUserFragment;

/// Attributed route back to the agent that granted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentReplyRoute {
    agent: AgentContextIdentity,
    scope: AgentReplyRouteScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentReplyRouteScope {
    CurrentTurn,
    UntilDisabled,
}

impl AgentReplyRoute {
    const CURRENT_TURN_CONTENT_KIND: &'static str = "multi_agent.agent_reply_route";

    pub(crate) fn new(agent: AgentContextIdentity) -> Self {
        Self {
            agent,
            scope: AgentReplyRouteScope::CurrentTurn,
        }
    }

    pub(crate) fn until_disabled(agent: AgentContextIdentity) -> Self {
        Self {
            agent,
            scope: AgentReplyRouteScope::UntilDisabled,
        }
    }

    pub(crate) fn persistent_agent_id(item: &ResponseItem) -> Option<ThreadId> {
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = item
        else {
            return None;
        };
        if role != "user" {
            return None;
        }
        let kinds = internal_chat_message_metadata_passthrough
            .as_ref()?
            .content_item_kinds
            .as_ref()?;
        content.iter().zip(kinds).find_map(|(content, kind)| {
            if kind.0 != PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND {
                return None;
            }
            let codex_protocol::models::ContentItem::InputText { text } = content else {
                return None;
            };
            Self::persistent_agent_id_from_text(text)
        })
    }

    fn persistent_agent_id_from_text(text: &str) -> Option<ThreadId> {
        if !Self::matches_text(text) {
            return None;
        }
        let (start_marker, end_marker) = Self::type_markers();
        let trimmed = text.trim();
        let body = trimmed.get(start_marker.len()..trimmed.len().checked_sub(end_marker.len())?)?;
        let fields = serde_json::from_str::<Value>(body.trim()).ok()?;
        if fields.get("send_input")?.as_str()? != "allowed_until_disabled" {
            return None;
        }
        ThreadId::from_string(fields.get("agent_id")?.as_str()?).ok()
    }
}

impl ContextualUserFragment for AgentReplyRoute {
    fn content_kind(&self) -> ContentItemKind {
        let content_kind = match self.scope {
            AgentReplyRouteScope::CurrentTurn => Self::CURRENT_TURN_CONTENT_KIND,
            AgentReplyRouteScope::UntilDisabled => PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND,
        };
        ContentItemKind(content_kind.to_string())
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
        let route = match self.scope {
            AgentReplyRouteScope::CurrentTurn => "allowed_this_turn",
            AgentReplyRouteScope::UntilDisabled => "allowed_until_disabled",
        };
        fields.insert("send_input".to_string(), Value::String(route.to_string()));
        format!("\n{}\n", Value::Object(fields))
    }
}
