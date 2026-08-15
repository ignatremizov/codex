use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use serde_json::Map;
use serde_json::Value;

/// Source-relative identity included in model-visible agent context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AgentContextIdentity {
    /// V1 targets use the same compact root-scoped identities accepted by follow-up tools.
    V1 {
        agent_id: ThreadId,
        agent_ref: Option<u64>,
        nickname: Option<String>,
    },
    /// V2 targets use their routed agent path.
    V2 {
        agent_id: ThreadId,
        agent_path: AgentPath,
    },
    /// UUID-only fallback for an agent with no usable source-relative identity.
    Canonical { agent_id: ThreadId },
}

impl AgentContextIdentity {
    pub(super) fn json_fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        match self {
            Self::V1 {
                agent_id,
                agent_ref,
                nickname,
            } => {
                fields.insert("agent_id".to_string(), Value::String(agent_id.to_string()));
                if let Some(agent_ref) = agent_ref {
                    fields.insert("ref".to_string(), Value::String(agent_ref.to_string()));
                }
                if let Some(nickname) = nickname {
                    fields.insert("nickname".to_string(), Value::String(nickname.clone()));
                }
            }
            Self::V2 {
                agent_id,
                agent_path,
            } => {
                fields.insert("agent_id".to_string(), Value::String(agent_id.to_string()));
                fields.insert(
                    "agent_path".to_string(),
                    Value::String(agent_path.to_string()),
                );
            }
            Self::Canonical { agent_id } => {
                fields.insert("agent_id".to_string(), Value::String(agent_id.to_string()));
            }
        }
        fields
    }
}
