//! Completion history for explicit unified-exec process waits.

use super::*;
use codex_app_server_protocol::TerminalWaitCompletionReason;

#[derive(Debug)]
pub(crate) struct UnifiedExecWaitResultCell {
    command_display: Option<String>,
    process_id: String,
    // Keep the correlation ID with the completed result for internal identity preservation, but
    // do not expose it in transcript text intended for users.
    _interaction_id: String,
    elapsed_ms: u64,
    reason: TerminalWaitCompletionReason,
}

impl UnifiedExecWaitResultCell {
    pub(crate) fn new(
        command_display: Option<String>,
        process_id: String,
        interaction_id: String,
        elapsed_ms: u64,
        reason: TerminalWaitCompletionReason,
    ) -> Self {
        Self {
            command_display,
            process_id,
            _interaction_id: interaction_id,
            elapsed_ms,
            reason,
        }
    }

    fn reason_label(&self) -> &'static str {
        match self.reason {
            TerminalWaitCompletionReason::Exited => "process exited",
            TerminalWaitCompletionReason::Timeout => "timed wait ended",
            TerminalWaitCompletionReason::Input => "interrupted by input",
            TerminalWaitCompletionReason::Cancelled => "wait cancelled",
            TerminalWaitCompletionReason::Failed => "wait failed",
        }
    }

    fn result_line(&self) -> Line<'static> {
        let elapsed = format_wait_duration(self.elapsed_ms);
        let reason = match self.reason {
            TerminalWaitCompletionReason::Exited => self.reason_label().green(),
            TerminalWaitCompletionReason::Input => self.reason_label().cyan(),
            TerminalWaitCompletionReason::Failed => self.reason_label().red(),
            TerminalWaitCompletionReason::Timeout | TerminalWaitCompletionReason::Cancelled => {
                self.reason_label().dim()
            }
        };
        let mut spans = vec![format!("• Waited {elapsed}").bold(), " · ".dim(), reason];
        if let Some(command) = self
            .command_display
            .as_deref()
            .filter(|command| !command.is_empty())
        {
            spans.push(" · ".dim());
            spans.push(command.to_string().dim());
        }
        if !self.process_id.is_empty() {
            spans.push(format!(" (process {})", self.process_id).dim());
        }
        Line::from(spans)
    }
}

impl HistoryCell for UnifiedExecWaitResultCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }
        let line = self.result_line();
        let wrapped = adaptive_wrap_line(&line, RtOptions::new(width.into()));
        let mut out = Vec::new();
        push_owned_lines(&wrapped, &mut out);
        out
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let elapsed = format_wait_duration(self.elapsed_ms);
        let command = self
            .command_display
            .as_deref()
            .filter(|command| !command.is_empty())
            .map(|command| format!(" · {command}"))
            .unwrap_or_default();
        let process = if self.process_id.is_empty() {
            String::new()
        } else {
            format!(" · process {}", self.process_id)
        };
        vec![Line::from(format!(
            "Waited {elapsed} · {}{command}{process}",
            self.reason_label()
        ))]
    }
}

/// Format the authoritative backend wait duration without discarding subsecond precision.
///
/// The backend measures from a monotonic clock and serializes whole milliseconds. A zero value
/// therefore means the measured interval was shorter than one millisecond, not that a one-second
/// wait occurred.
fn format_wait_duration(elapsed_ms: u64) -> String {
    if elapsed_ms == 0 {
        return String::from("<1ms");
    }
    if elapsed_ms < 1_000 {
        return format!("{elapsed_ms}ms");
    }

    let seconds = elapsed_ms / 1_000;
    let milliseconds = elapsed_ms % 1_000;
    let display_seconds = seconds % 60;
    let fractional_seconds = if milliseconds == 0 {
        format!("{display_seconds:02}s")
    } else {
        let fraction = format!("{milliseconds:03}");
        let fraction = fraction.trim_end_matches('0');
        format!("{display_seconds:02}.{fraction}s")
    };

    if seconds >= 3_600 {
        let hours = seconds / 3_600;
        let minutes = (seconds % 3_600) / 60;
        format!("{hours}h {minutes:02}m {fractional_seconds}")
    } else if seconds >= 60 {
        let minutes = seconds / 60;
        format!("{minutes}m {fractional_seconds}")
    } else if milliseconds == 0 {
        format!("{seconds}s")
    } else {
        let fraction = format!("{milliseconds:03}");
        let fraction = fraction.trim_end_matches('0');
        format!("{seconds}.{fraction}s")
    }
}

pub(crate) fn new_unified_exec_wait_result(
    command_display: Option<String>,
    process_id: String,
    interaction_id: String,
    elapsed_ms: u64,
    reason: TerminalWaitCompletionReason,
) -> UnifiedExecWaitResultCell {
    UnifiedExecWaitResultCell::new(
        command_display,
        process_id,
        interaction_id,
        elapsed_ms,
        reason,
    )
}
