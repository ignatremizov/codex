use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Start or completion metadata for one `write_stdin` wait.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(
    tag = "phase",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(
    tag = "phase",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    export_to = "v2/"
)]
pub enum TerminalWait {
    /// A wait has started for the identified invocation.
    Started {
        /// The `write_stdin` function-call ID, distinct from the command item ID.
        interaction_id: String,
        /// Unix timestamp in milliseconds when the wait began.
        started_at_ms: i64,
        /// Whether this wait has a deadline or continues until process exit.
        mode: TerminalWaitMode,
    },
    /// A wait has ended; the process may still be running for non-exit reasons.
    Finished {
        /// The `write_stdin` function-call ID matching the start event.
        interaction_id: String,
        /// Actual elapsed wait time in milliseconds.
        elapsed_ms: u64,
        /// Why the wait finished.
        reason: TerminalWaitCompletionReason,
    },
}

/// Duration policy for a `write_stdin` wait.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TerminalWaitMode {
    /// Wait for the requested or resolved `yield_time_ms` deadline.
    Timed,
    /// Wait until the associated process exits or the waiter is released.
    UntilExit,
}

/// Reason a `write_stdin` wait stopped waiting.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TerminalWaitCompletionReason {
    /// The associated process exited.
    Exited,
    /// The bounded poll deadline elapsed while the process remained live.
    Timeout,
    /// New turn input released an until-exit wait.
    Input,
    /// The tool invocation was cancelled.
    Cancelled,
    /// The wait ended because process or approval handling failed.
    Failed,
}
