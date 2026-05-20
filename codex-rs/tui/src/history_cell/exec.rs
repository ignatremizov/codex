//! Background terminal interaction and process-summary history cells.
//!
//! Polling and stdin bookkeeping stay in detailed and raw history; the normal chat view
//! relies on command output and the background-process status instead of repeating notices.

use super::*;
use crate::style::accent_color;
use crate::terminal_hyperlinks::adaptive_wrap_hyperlink_lines;

#[derive(Debug)]
pub(crate) struct UnifiedExecInteractionCell {
    command_display: Option<String>,
    stdin: String,
}

impl UnifiedExecInteractionCell {
    pub(crate) fn new(command_display: Option<String>, stdin: String) -> Self {
        Self {
            command_display,
            stdin,
        }
    }
}

impl HistoryCell for UnifiedExecInteractionCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        Vec::new()
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.transcript_hyperlink_lines(width))
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        if width == 0 {
            return Vec::new();
        }
        let wrap_width = width as usize;
        let waited_only = self.stdin.is_empty();

        let mut header_spans = if waited_only {
            vec!["• Waited for background terminal".bold()]
        } else {
            vec!["↳ ".dim(), "Interacted with background terminal".bold()]
        };
        if let Some(command) = &self.command_display
            && !command.is_empty()
        {
            header_spans.push(" · ".dim());
            header_spans.push(command.clone().dim());
        }
        let header = Line::from(header_spans);

        let mut out = adaptive_wrap_hyperlink_lines(&[header.into()], RtOptions::new(wrap_width));

        if waited_only {
            return out;
        }

        let input_lines: Vec<Line<'static>> = self
            .stdin
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect();

        let input_wrapped = adaptive_wrap_hyperlink_lines(
            &plain_hyperlink_lines(input_lines),
            RtOptions::new(wrap_width)
                .initial_indent(Line::from("  └ ".dim()))
                .subsequent_indent(Line::from("    ".dim())),
        );
        out.extend(input_wrapped);
        out
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if self.stdin.is_empty() {
            if let Some(command) = self
                .command_display
                .as_ref()
                .filter(|command| !command.is_empty())
            {
                out.push(Line::from(format!(
                    "Waited for background terminal: {command}"
                )));
            } else {
                out.push(Line::from("Waited for background terminal"));
            }
            return out;
        }

        if let Some(command) = self
            .command_display
            .as_ref()
            .filter(|command| !command.is_empty())
        {
            out.push(Line::from(format!(
                "Interacted with background terminal: {command}"
            )));
        } else {
            out.push(Line::from("Interacted with background terminal"));
        }
        out.extend(raw_lines_from_source(&self.stdin));
        out
    }
}

pub(crate) fn new_unified_exec_interaction(
    command_display: Option<String>,
    stdin: String,
) -> UnifiedExecInteractionCell {
    UnifiedExecInteractionCell::new(command_display, stdin)
}

#[derive(Debug)]
struct UnifiedExecProcessesCell {
    processes: Vec<UnifiedExecProcessDetails>,
    output_preview_lines: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct UnifiedExecProcessDetails {
    pub(crate) command_display: String,
    pub(crate) recent_chunks: crate::exec_cell::LiveCommandOutput,
}

impl UnifiedExecProcessesCell {
    fn rendered_lines(&self, width: u16, limit: usize) -> Vec<HyperlinkLine> {
        if width == 0 {
            return Vec::new();
        }
        let mut out: Vec<HyperlinkLine> = vec![
            Line::from("Background terminals".bold()).into(),
            Line::default().into(),
        ];
        if self.processes.is_empty() {
            out.push(Line::from("  • No background terminals running.".italic()).into());
            return out;
        }
        for process in self.processes.iter().take(/*n*/ 16) {
            let command = process
                .command_display
                .lines()
                .map(|line| Line::from(line.to_owned().fg(accent_color())).into())
                .collect::<Vec<_>>();
            out.extend(adaptive_wrap_hyperlink_lines(
                &command,
                RtOptions::new(usize::from(width))
                    .initial_indent(Line::from("  • ".dim()))
                    .subsequent_indent(Line::from("    ")),
            ));
            let output = &process.recent_chunks;
            let preview = crate::exec_cell::output_preview(
                output.transcript_lines().map(|raw| {
                    let mut line = codex_ansi_escape::ansi_escape_line(raw.as_ref());
                    line.style = line.style.add_modifier(Modifier::DIM);
                    line.into()
                }),
                usize::from(width)
                    .saturating_sub(/*rhs*/ 6)
                    .max(/*other*/ 1),
                limit,
                output.total_lines().saturating_sub(output.retained_lines()),
            );
            out.extend(crate::terminal_hyperlinks::prefix_hyperlink_lines(
                preview.lines,
                "    ↳ ".dim(),
                "      ".into(),
            ));
        }
        let remaining = self.processes.len().saturating_sub(/*rhs*/ 16);
        if remaining > 0 {
            out.extend(adaptive_wrap_hyperlink_lines(
                &[Line::from(format!("  • ... and {remaining} more running").dim()).into()],
                RtOptions::new(usize::from(width)),
            ));
        }
        out
    }
}

impl HistoryCell for UnifiedExecProcessesCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.display_hyperlink_lines(width))
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.rendered_lines(width, self.output_preview_lines)
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.transcript_hyperlink_lines(width))
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.rendered_lines(width, /*limit*/ 0)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.transcript_lines(u16::MAX))
    }
}

#[cfg(test)]
pub(crate) fn new_unified_exec_processes_output(
    processes: Vec<UnifiedExecProcessDetails>,
) -> CompositeHistoryCell {
    new_unified_exec_processes_output_with_limit(
        processes,
        codex_config::types::DEFAULT_TUI_COMMAND_OUTPUT_PREVIEW_LINES,
    )
}

pub(crate) fn new_unified_exec_processes_output_with_limit(
    processes: Vec<UnifiedExecProcessDetails>,
    output_preview_lines: usize,
) -> CompositeHistoryCell {
    let command = PlainHistoryCell::new(vec!["/ps".magenta().into()]);
    let summary = UnifiedExecProcessesCell {
        processes,
        output_preview_lines,
    };
    CompositeHistoryCell::new(vec![Box::new(command), Box::new(summary)])
}
