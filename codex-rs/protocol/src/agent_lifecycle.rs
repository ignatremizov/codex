//! Presentation metadata for ownership changes performed through existing agent tools.

use crate::ThreadId;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Ownership operation, distinct from the tool used to request it.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum CollabAgentLifecycleAction {
    Detach,
    Reattach,
}

/// Root involved in a detach or reattachment; this metadata grants no control authority.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct CollabAgentLifecycle {
    pub action: CollabAgentLifecycleAction,
    /// Source root for detachment, destination root for reattachment.
    pub root_thread_id: ThreadId,
}
