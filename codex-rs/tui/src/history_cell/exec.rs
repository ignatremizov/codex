//! Background terminal interaction and process-summary history cells.

use super::*;
use textwrap::WordSplitter;

#[derive(Debug)]
pub(crate) struct UnifiedExecInteractionCell {
    command_display: Option<String>,
    stdin: String,
    kind: UnifiedExecInteractionKind,
}

#[derive(Clone, Copy, Debug)]
enum UnifiedExecInteractionKind {
    Input,
    Wait,
    OutputCheck,
}

impl UnifiedExecInteractionCell {
    pub(crate) fn new(command_display: Option<String>, stdin: String) -> Self {
        let kind = if stdin.is_empty() {
            UnifiedExecInteractionKind::Wait
        } else {
            UnifiedExecInteractionKind::Input
        };
        Self {
            command_display,
            stdin,
            kind,
        }
    }

    fn output_check(command_display: Option<String>) -> Self {
        Self {
            command_display,
            stdin: String::new(),
            kind: UnifiedExecInteractionKind::OutputCheck,
        }
    }
}

impl HistoryCell for UnifiedExecInteractionCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }
        let wrap_width = width as usize;

        let mut header_spans = match self.kind {
            UnifiedExecInteractionKind::Input => {
                vec!["↳ ".dim(), "Interacted with background terminal".bold()]
            }
            UnifiedExecInteractionKind::Wait => {
                vec!["• Waited for background terminal".bold()]
            }
            UnifiedExecInteractionKind::OutputCheck => {
                vec!["• Checked background terminal output".bold()]
            }
        };
        if let Some(command) = &self.command_display
            && !command.is_empty()
        {
            header_spans.push(" · ".dim());
            header_spans.push(command.clone().dim());
        }
        let header = Line::from(header_spans);

        let mut out: Vec<Line<'static>> = Vec::new();
        let header_wrapped = adaptive_wrap_line(&header, RtOptions::new(wrap_width));
        push_owned_lines(&header_wrapped, &mut out);

        if !matches!(self.kind, UnifiedExecInteractionKind::Input) {
            return out;
        }

        let input_lines: Vec<Line<'static>> = self
            .stdin
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect();

        let input_wrapped = adaptive_wrap_lines(
            input_lines,
            RtOptions::new(wrap_width)
                .initial_indent(Line::from("  └ ".dim()))
                .subsequent_indent(Line::from("    ".dim())),
        );
        out.extend(input_wrapped);
        out
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let action = match self.kind {
            UnifiedExecInteractionKind::Input => None,
            UnifiedExecInteractionKind::Wait => Some("Waited for background terminal"),
            UnifiedExecInteractionKind::OutputCheck => Some("Checked background terminal output"),
        };
        if let Some(action) = action {
            if let Some(command) = self
                .command_display
                .as_ref()
                .filter(|command| !command.is_empty())
            {
                out.push(Line::from(format!("{action}: {command}")));
            } else {
                out.push(Line::from(action));
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

pub(crate) fn new_unified_exec_output_check(
    command_display: Option<String>,
) -> UnifiedExecInteractionCell {
    UnifiedExecInteractionCell::output_check(command_display)
}

#[derive(Debug)]
struct UnifiedExecProcessesCell {
    processes: Vec<UnifiedExecProcessDetails>,
    output_preview_lines: usize,
}

impl UnifiedExecProcessesCell {
    fn new(processes: Vec<UnifiedExecProcessDetails>, output_preview_lines: usize) -> Self {
        Self {
            processes,
            output_preview_lines,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct UnifiedExecProcessDetails {
    pub(crate) command_display: String,
    pub(crate) recent_chunks: Vec<String>,
}

impl HistoryCell for UnifiedExecProcessesCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let wrap_width = width as usize;
        let max_processes = 16usize;
        let mut out: Vec<Line<'static>> = Vec::new();
        out.push(vec!["Background terminals".bold()].into());
        out.push("".into());

        if self.processes.is_empty() {
            out.push("  • No background terminals running.".italic().into());
            return out;
        }

        let prefix = "  • ";
        let mut shown = 0usize;
        for process in &self.processes {
            if shown >= max_processes {
                break;
            }
            let command_lines = process
                .command_display
                .lines()
                .map(|line| Line::from(line.to_string().cyan()))
                .collect::<Vec<_>>();
            let command_lines = if command_lines.is_empty() {
                vec![Line::from("")]
            } else {
                command_lines
            };
            let wrapped_command = adaptive_wrap_lines(
                command_lines,
                RtOptions::new(wrap_width)
                    .initial_indent(Line::from(prefix.dim()))
                    .subsequent_indent(Line::from("    ".dim()))
                    .word_splitter(WordSplitter::NoHyphenation),
            );
            out.extend(wrapped_command);

            let chunk_prefix_first = "    ↳ ";
            let chunk_prefix_next = "      ";
            let output_wrap_width = wrap_width
                .saturating_sub(
                    UnicodeWidthStr::width(chunk_prefix_first)
                        .max(UnicodeWidthStr::width(chunk_prefix_next)),
                )
                .max(1);
            let output_opts =
                RtOptions::new(output_wrap_width).word_splitter(WordSplitter::NoHyphenation);
            let mut wrapped_output = Vec::new();
            for chunk in &process.recent_chunks {
                let chunk_lines = chunk
                    .lines()
                    .map(|line| Line::from(line.to_string().dim()))
                    .collect::<Vec<_>>();
                let chunk_lines = if chunk_lines.is_empty() {
                    vec![Line::from("")]
                } else {
                    chunk_lines
                };
                let wrapped_chunks = adaptive_wrap_lines(chunk_lines, output_opts.clone());
                wrapped_output.extend(wrapped_chunks);
            }
            let wrapped_output = cap_output_preview_rows(wrapped_output, self.output_preview_lines);
            let prefixed_output = prefix_lines(
                wrapped_output,
                Span::from(chunk_prefix_first).dim(),
                Span::from(chunk_prefix_next).dim(),
            );
            if !prefixed_output.is_empty() {
                out.extend(prefixed_output);
            }
            shown += 1;
        }

        let remaining = self.processes.len().saturating_sub(shown);
        if remaining > 0 {
            let more_text = format!("... and {remaining} more running");
            let prefix_width = UnicodeWidthStr::width(prefix);
            if wrap_width <= prefix_width {
                out.push(Line::from(prefix.dim()));
            } else {
                let budget = wrap_width.saturating_sub(prefix_width);
                let (truncated, _, _) = take_prefix_by_width(&more_text, budget);
                out.push(vec![prefix.dim(), truncated.dim()].into());
            }
        }

        out
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(u16::MAX))
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.display_lines(width).len() as u16
    }
}

pub(crate) fn new_unified_exec_processes_output_with_limit(
    processes: Vec<UnifiedExecProcessDetails>,
    output_preview_lines: usize,
) -> CompositeHistoryCell {
    let command = PlainHistoryCell::new(vec!["/ps".magenta().into()]);
    let summary = UnifiedExecProcessesCell::new(processes, output_preview_lines);
    CompositeHistoryCell::new(vec![Box::new(command), Box::new(summary)])
}
