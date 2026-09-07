use codex_protocol::openai_models::ReasoningEffort;
use serde::Deserialize;
use serde::Serialize;

use crate::JsonSchema;
use crate::TS;

/// Trusted send-time identity, retained even when current aliases or ownership change.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentInputIdentity {
    pub thread_id: String,
    pub nickname: Option<String>,
    pub agent_ref: Option<String>,
    pub task_path: Option<String>,
    pub role: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Presentation-only provenance; the original message and attachments are in the item's input.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentInputAttribution {
    pub sender: AgentInputIdentity,
    pub recipient: AgentInputIdentity,
    pub sender_turn_id: String,
}

/// Committed adoption label mapping. Thread identity remains unchanged.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentTaskPathMapping {
    pub thread_id: String,
    pub previous_task_path: Option<String>,
    pub task_path: Option<String>,
}

impl From<codex_protocol::AgentInputIdentity> for AgentInputIdentity {
    fn from(value: codex_protocol::AgentInputIdentity) -> Self {
        Self {
            thread_id: value.thread_id.to_string(),
            nickname: value.nickname,
            agent_ref: value.agent_ref,
            task_path: value.task_path,
            role: value.role,
            model: value.model,
            reasoning_effort: value.reasoning_effort,
        }
    }
}

impl From<codex_protocol::AgentInputAttribution> for AgentInputAttribution {
    fn from(value: codex_protocol::AgentInputAttribution) -> Self {
        Self {
            sender: value.sender.into(),
            recipient: value.recipient.into(),
            sender_turn_id: value.sender_turn_id,
        }
    }
}

impl From<codex_protocol::AgentTaskPathMapping> for AgentTaskPathMapping {
    fn from(value: codex_protocol::AgentTaskPathMapping) -> Self {
        Self {
            thread_id: value.thread_id.to_string(),
            previous_task_path: value.previous_task_path,
            task_path: value.task_path,
        }
    }
}

#[cfg(test)]
#[path = "agent_input_attribution_tests.rs"]
mod tests;
