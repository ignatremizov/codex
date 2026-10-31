use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Explicitly promote an idle child branch to an independent root, retaining its UUIDs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadDetachParams {
    pub thread_id: String,
}

/// The caller resumes `thread_id` to attach to its new owning root.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadDetachResponse {
    pub thread_id: String,
    pub previous_root_thread_id: String,
    pub unloaded_thread_ids: Vec<String>,
}
