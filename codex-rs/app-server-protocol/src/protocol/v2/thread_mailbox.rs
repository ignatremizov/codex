use super::UserInput;
use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Deposit user-authored input for explicit receiver consumption, not a payload-bearing turn.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ThreadMailboxAddParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    /// Receiver-scoped retry identity. Reuse only for the same original typed input.
    pub client_user_message_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadMailboxMessageState {
    Pending,
    Claimed,
    Consumed,
    Rejected,
}

/// Current durable acceptance state, including the original outcome on retries.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadMailboxAddResponse {
    /// Stable mailbox message identity; never a turn ID.
    pub message_id: String,
    pub state: ThreadMailboxMessageState,
    pub rejection_reason: Option<String>,
}
