//! Send-time identity snapshots for trusted agent input, independent of model presentation.

use crate::ThreadId;
use crate::openai_models::ReasoningEffort;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Core-authored provenance, never inferred from caller-authored payload text.
///
/// Display labels describe the sender at admission time and are not routing authority.
/// The original payload and attachments remain in the containing item's typed input.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct AgentInputAttribution {
    pub sender: AgentInputIdentity,
    pub recipient: AgentInputIdentity,
    pub sender_turn_id: String,
}

/// Identity and display metadata captured at send time, not re-resolved during replay.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct AgentInputIdentity {
    pub thread_id: ThreadId,
    pub nickname: Option<String>,
    pub agent_ref: Option<String>,
    pub task_path: Option<String>,
    pub role: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// A task-label change committed during adoption; canonical identity is unchanged.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct AgentTaskPathMapping {
    pub thread_id: ThreadId,
    pub previous_task_path: Option<String>,
    pub task_path: Option<String>,
}
