//! Completion history for explicit unified-exec process waits.

use super::*;
use crate::status_indicator_widget::fmt_elapsed_compact;
use codex_app_server_protocol::TerminalWaitCompletionReason;

#[derive(Debug)]
pub(crate) struct UnifiedExecWaitResultCell {
    command_display: Option<String>,
    process_id: String,
    interaction_id: String,
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
            interaction_id,
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
        let elapsed = fmt_elapsed_compact(self.elapsed_ms / 1_000);
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
            spans.push(
                format!(
                    " (process {}; wait {})",
                    self.process_id, self.interaction_id
                )
                .dim(),
            );
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
        let elapsed = fmt_elapsed_compact(self.elapsed_ms / 1_000);
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
            "Waited {elapsed} · {}{command}{process} [wait {}]",
            self.reason_label(),
            self.interaction_id
        ))]
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
