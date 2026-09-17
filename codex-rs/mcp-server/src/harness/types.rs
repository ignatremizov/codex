use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;

use crate::codex_tool_config::CodexToolCallSandboxMode;

/// Parameters for the `exec_command` MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecCommandParams {
    /// The command string to execute.
    pub command: String,

    /// Working directory for the command. Defaults to the server's working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    /// Explicit environment variables to inject into the process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,

    /// Shell binary and arguments to invoke the command with.
    /// Defaults to "/bin/bash -lc".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,

    /// Requested sandbox mode: `read-only`, `workspace-write`, or `danger-full-access`.
    /// Omit this parameter by default unless the user explicitly requests sandboxing.
    /// When omitted, runs under the server's default configuration without restricting PID namespaces or device access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<CodexToolCallSandboxMode>,

    /// Initial wait before returning a still-running process as a resumable session.
    /// Defaults to 60,000 ms (60 seconds). Set to 0 to yield immediately.
    /// Yielding does not terminate the process; continue it with write_stdin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yield_time_ms: Option<u64>,

    /// Whether to allocate a pseudo-terminal (PTY) for interactive programs. Defaults to false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<bool>,

    /// Maximum output bytes retained per stream before truncation. Defaults to 1,048,576 (1 MB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<usize>,
}

/// Output returned by the `exec_command` MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecCommandResponse {
    /// Status of the execution: "completed", "running", "rejected", or "error".
    pub status: String,

    /// Captured standard output.
    pub stdout: String,

    /// Captured standard error.
    pub stderr: String,

    /// Combined standard output and standard error. The MCP structured result may omit this
    /// field when it is identical to stdout or stderr.
    pub output: String,

    /// Whether output exceeded max_output_bytes and was truncated.
    pub output_truncated: bool,

    /// Total output bytes emitted before truncation.
    pub output_bytes_total: usize,

    /// Process exit code if completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,

    /// OS process ID of the spawned process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,

    /// Active session ID for use with write_stdin if still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<i32>,

    /// Wall-clock elapsed time in milliseconds.
    pub wall_time_ms: u64,

    /// Rejection reason if denied by Starlark policy or sandbox ceiling check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
}

/// Signal types supported by `write_stdin`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum ProcessSignalParam {
    #[serde(rename = "SIGINT")]
    Sigint,
    #[serde(rename = "SIGTERM")]
    Sigterm,
    #[serde(rename = "SIGKILL")]
    Sigkill,
}

/// Parameters for the `write_stdin` MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WriteStdinParams {
    /// The active session ID returned from a previous exec_command or write_stdin call.
    pub session_id: i32,

    /// Characters/bytes to write to standard input. If omitted, the call polls for output deltas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chars: Option<String>,

    /// POSIX signal to send to the process group (e.g. SIGINT for Ctrl-C, SIGTERM for termination).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<ProcessSignalParam>,

    /// Milliseconds to wait for output delta after writing or signaling. Defaults to 10,000 (10 seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yield_time_ms: Option<u64>,

    /// If true, waits until the process exits (up to timeout_ms) instead of returning after yield_time_ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_until_exit: Option<bool>,

    /// Timeout in milliseconds when wait_until_exit is true. Defaults to 120,000 (2 minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,

    /// Whether to close the stdin pipe after writing chars (sends EOF).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_stdin: Option<bool>,
}

/// Output returned by the `write_stdin` MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WriteStdinResponse {
    /// Status: "completed", "running", "not_found", "timed_out", or "error".
    pub status: String,

    /// New standard output delta emitted since the last call.
    pub stdout: String,

    /// New standard error delta emitted since the last call.
    pub stderr: String,

    /// Combined standard output and error delta. The MCP structured result may omit this field
    /// when it is identical to stdout or stderr.
    pub output: String,

    /// Whether output exceeded max_output_bytes and was truncated.
    pub output_truncated: bool,

    /// Process exit code if terminated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,

    /// OS process ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,

    /// Active session ID if the process is still alive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<i32>,
}

/// Parameters for the `apply_patch` MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPatchParams {
    /// Patch content in unified diff format or Codex patch format.
    pub patch: String,

    /// Base directory for relative paths in the patch. Defaults to the server's current working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    /// If true, validates whether the patch applies cleanly without mutating files on disk. Defaults to false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_only: Option<bool>,
}

/// A structured file modification result from `apply_patch`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FilePatchAction {
    /// Path of the file affected.
    pub path: String,

    /// Action taken or proposed: "modified", "created", or "deleted".
    pub action: String,
}

/// Output returned by the `apply_patch` MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPatchResponse {
    /// Whether the patch was applied (or validated, if check_only was true) successfully.
    pub success: bool,

    /// Human-readable summary of the operation or error message.
    pub summary: String,

    /// List of per-file actions performed or proposed.
    pub files: Vec<FilePatchAction>,
}
