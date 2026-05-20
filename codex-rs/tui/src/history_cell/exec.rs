//! Background terminal interaction and process-summary history cells.

use super::*;
use textwrap::WordSplitter;

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
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
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

        let mut out: Vec<Line<'static>> = Vec::new();
        let header_wrapped = adaptive_wrap_line(&header, RtOptions::new(wrap_width));
        push_owned_lines(&header_wrapped, &mut out);

        if waited_only {
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
}

impl UnifiedExecProcessesCell {
    fn new(processes: Vec<UnifiedExecProcessDetails>) -> Self {
        Self { processes }
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
            let mut showed_output_prefix = false;
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
                let wrapped_chunks = adaptive_wrap_lines(
                    chunk_lines,
                    RtOptions::new(wrap_width)
                        .initial_indent(Line::from(
                            if showed_output_prefix {
                                chunk_prefix_next
                            } else {
                                chunk_prefix_first
                            }
                            .dim(),
                        ))
                        .subsequent_indent(Line::from(chunk_prefix_next.dim()))
                        .word_splitter(WordSplitter::NoHyphenation),
                );
                out.extend(wrapped_chunks);
                showed_output_prefix = true;
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

pub(crate) fn new_unified_exec_processes_output(
    processes: Vec<UnifiedExecProcessDetails>,
) -> CompositeHistoryCell {
    let command = PlainHistoryCell::new(vec!["/ps".magenta().into()]);
    let summary = UnifiedExecProcessesCell::new(processes);
    CompositeHistoryCell::new(vec![Box::new(command), Box::new(summary)])
}
