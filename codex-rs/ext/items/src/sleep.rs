use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Terminal outcome of an interruptible `clock.sleep` tool call.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum SleepOutcome {
    Completed,
    Interrupted,
    Error,
}

/// Display item emitted by the interruptible `clock.sleep` tool.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SleepItem {
    pub id: String,
    #[ts(type = "number")]
    pub duration_ms: u64,
    pub outcome: Option<SleepOutcome>,
    #[ts(type = "number | null")]
    pub elapsed_ms: Option<u64>,
}
