//! Admission results for an array-target send. Errors do not prove non-delivery.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CollabAgentInputBatch {
    /// Shared, normalized handling flags. Empty means default passive handling.
    pub flags: String,
    /// Sender identity for a child-originated live/replayed batch presentation.
    #[serde(default)]
    pub sender_thread_id: Option<crate::ThreadId>,
    /// Empty while the batch is in progress; populated only on normal completion.
    pub results: Vec<CollabAgentInputResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CollabAgentInputResult {
    /// Canonical ref when available, otherwise UUID or the unresolved authored selector.
    pub target: String,
    /// Joins the parent item's receiver metadata; absent for unresolved selectors.
    pub receiver_thread_id: Option<String>,
    pub status: CollabAgentInputStatus,
    pub error: Option<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum CollabAgentInputStatus {
    Submitted,
    Queued,
    MailboxAccepted,
    /// The operation returned an error, potentially after admission.
    Error,
}

#[cfg(test)]
#[path = "collab_input_tests.rs"]
mod tests;
