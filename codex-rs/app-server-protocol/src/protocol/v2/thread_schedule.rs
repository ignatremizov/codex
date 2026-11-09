use super::QueuedSubmission;
use super::Turn;
use super::UserInput;
use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Delayed ordinary text input, eligible for loading in the receiving server's profile.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueScheduleParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    pub client_user_message_id: String,
    /// Earliest delivery time, in Unix seconds. Past instants are immediately due.
    #[ts(type = "number")]
    pub scheduled_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueResultParams {
    pub queued_submission_id: String,
}

/// The pending entry or its receiving turn; result lookup does not load a runtime.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueResultResponse {
    pub queued_submission: Option<QueuedSubmission>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    /// Whether this input started the turn rather than steering somebody else's turn.
    pub started: Option<bool>,
    pub turn: Option<Turn>,
}
