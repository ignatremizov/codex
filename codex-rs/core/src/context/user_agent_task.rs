use codex_protocol::models::ContentItemKind;
use serde_json::Value;

use super::AgentContextIdentity;
use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserAgentTask {
    agent: AgentContextIdentity,
    task_preview: String,
}

impl UserAgentTask {
    pub(crate) fn new(agent: AgentContextIdentity, task_preview: impl Into<String>) -> Self {
        Self {
            agent,
            task_preview: task_preview.into(),
        }
    }
}

impl ContextualUserFragment for UserAgentTask {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.user_agent_task".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<user_agent_task>", "</user_agent_task>")
    }

    fn body(&self) -> String {
        let mut fields = self.agent.json_fields();
        fields.insert(
            "task_preview".to_string(),
            Value::String(self.task_preview.clone()),
        );
        format!("\n{}\n", Value::Object(fields))
    }
}
