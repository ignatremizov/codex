use serde::Deserialize;
use serde::Serialize;

use super::AgentResponseHandling;
use super::UserInput;
use crate::JsonSchema;
use crate::TS;

/// Parameters for listing pending turns in one agent-root queue.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentQueueListParams {
    /// Root or member thread whose root-scoped queue should be listed.
    pub root_thread_id: String,
    /// Opaque pagination cursor returned by a previous call.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Maximum number of queued turns to return.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

/// Structured input waiting to start a distinct target turn.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentQueueEntry {
    pub id: String,
    pub source_thread_id: String,
    pub target_thread_id: String,
    pub input: Vec<UserInput>,
    pub prompt_preview: String,
    pub response_handling: AgentResponseHandling,
    pub authored_selector: Option<String>,
}

/// Queue provenance and response handling bound to a newly started target turn.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentQueueTurnMetadata {
    pub queue_id: String,
    pub source_thread_id: String,
    /// Committed queue-entry policy, including queued source delivery; `null` when source-side
    /// persistence degraded after admission.
    pub response_handling: Option<AgentResponseHandling>,
}

/// One page of pending queued turns in target FIFO order.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentQueueListResponse {
    pub data: Vec<AgentQueueEntry>,
    /// Opaque cursor for the next page, or `null` when no queued turns remain.
    pub next_cursor: Option<String>,
}

/// Parameters for removing one pending queued turn.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentQueueDeleteParams {
    /// Root or member thread whose root-scoped queue owns the entry.
    pub root_thread_id: String,
    pub id: String,
}

/// Result of removing one pending queued turn.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentQueueDeleteResponse {
    pub id: String,
}
