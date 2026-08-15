use serde::Deserialize;
use serde::Serialize;

use crate::JsonSchema;
use crate::TS;

/// Parameters for listing durable aliases in one agent-root namespace.
///
/// `root_thread_id` may name the root itself or any thread in that root's session tree.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentAliasListParams {
    /// Root or member thread whose root-scoped alias namespace should be listed.
    pub root_thread_id: String,
    /// Opaque pagination cursor returned by a previous call.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Maximum number of aliases to return.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

/// Durable lifecycle state of an alias in one root namespace.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum AgentAliasState {
    Active,
    Closed,
    Transferred,
}

/// Root-scoped user-facing aliases for one canonical agent thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentAlias {
    pub thread_id: String,
    #[serde(rename = "ref")]
    #[ts(rename = "ref")]
    pub agent_ref: String,
    pub nickname: Option<String>,
    pub state: AgentAliasState,
}

/// One page of aliases in stable numeric-ref order.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentAliasListResponse {
    pub data: Vec<AgentAlias>,
    /// Opaque cursor for the next page, or `null` when no aliases remain.
    pub next_cursor: Option<String>,
}
