//! Harness-classified policy notices. Retained text is audit evidence, not a live grant.

use codex_protocol::models::ContentItemKind;

use super::ContextualUserFragment;

pub(crate) const AGENT_MESSAGING_POLICY_PREFIX: &str = "multi_agent.messaging_policy.";

pub(crate) struct AgentMessagingPolicyNotice {
    pub(crate) key: String,
    pub(crate) text: String,
}

impl ContextualUserFragment for AgentMessagingPolicyNotice {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(format!("{AGENT_MESSAGING_POLICY_PREFIX}{}", self.key))
    }

    fn markers(&self) -> (&'static str, &'static str) {
        ("", "")
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.text.clone()
    }
}
