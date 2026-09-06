//! Fixtures for notices emitted by older versions; production has no route-notice writer.

use super::AgentContextIdentity;
use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND;
use serde_json::Value;

#[derive(Clone)]
pub(crate) struct AgentReplyRoute {
    agent: AgentContextIdentity,
    scope: Scope,
}

#[derive(Clone, Copy)]
enum Scope {
    CurrentTurn,
    UntilDisabled,
}

impl AgentReplyRoute {
    pub(crate) fn new(agent: AgentContextIdentity) -> Self {
        Self {
            agent,
            scope: Scope::CurrentTurn,
        }
    }

    pub(crate) fn until_disabled(agent: AgentContextIdentity) -> Self {
        Self {
            agent,
            scope: Scope::UntilDisabled,
        }
    }
}

impl ContextualUserFragment for AgentReplyRoute {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(
            match self.scope {
                Scope::CurrentTurn => "multi_agent.agent_reply_route",
                Scope::UntilDisabled => PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND,
            }
            .to_string(),
        )
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

    fn body(&self) -> codex_protocol::models::ContentItem {
        let mut fields = self.agent.json_fields();
        let scope = match self.scope {
            Scope::CurrentTurn => "allowed_this_turn",
            Scope::UntilDisabled => "allowed_until_disabled",
        };
        fields.insert("send_input".to_string(), Value::String(scope.to_string()));
        codex_protocol::models::input_text(format!("\n{}\n", Value::Object(fields)))
    }
}
