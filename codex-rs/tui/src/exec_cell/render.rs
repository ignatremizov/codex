use std::time::Instant;

use super::model::CommandOutput;
use super::model::ExecCall;
use super::model::ExecCell;
use super::model::OutputPreviewLineLimits;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::history_cell::HistoryCell;
use crate::history_cell::plain_lines;
use crate::motion::MotionMode;
use crate::motion::ReducedMotionIndicator;
use crate::motion::activity_indicator;
use crate::render::highlight::highlight_bash_to_lines;
use crate::render::line_utils::prefix_lines;
use crate::render::line_utils::push_owned_lines;
use crate::ui_consts::TRANSCRIPT_HINT;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_line;
use crate::wrapping::adaptive_wrap_lines;
use codex_ansi_escape::ansi_escape_line;
use codex_app_server_protocol::CommandExecutionSource as ExecCommandSource;
use codex_protocol::parse_command::ParsedCommand;
use codex_shell_command::bash::extract_bash_command;
use codex_utils_elapsed::format_duration;
use itertools::Itertools;
use ratatui::prelude::*;
use ratatui::style::Modifier;
use ratatui::style::Stylize;
use textwrap::WordSplitter;
use unicode_width::UnicodeWidthStr;

pub(crate) const TOOL_CALL_MAX_LINES: usize = 5;
const MAX_INTERACTION_PREVIEW_CHARS: usize = 80;

pub(crate) struct OutputLinesParams {
    pub(crate) line_limit: usize,
    pub(crate) only_err: bool,
    pub(crate) include_angle_pipe: bool,
    pub(crate) include_prefix: bool,
}

pub(crate) fn new_active_exec_command(
    call_id: String,
    command: Vec<String>,
    parsed: Vec<ParsedCommand>,
    source: ExecCommandSource,
    interaction_input: Option<String>,
    animations_enabled: bool,
    output_preview_line_limits: OutputPreviewLineLimits,
) -> ExecCell {
    ExecCell::new(
        ExecCall {
            call_id,
            command,
            parsed,
            output: None,
            source,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input,
        },
        animations_enabled,
    )
    .with_output_preview_line_limits(output_preview_line_limits)
}

fn format_unified_exec_interaction(command: &[String], input: Option<&str>) -> String {
    let command_display = if let Some((_, script)) = extract_bash_command(command) {
        script.to_string()
    } else {
        command.join(" ")
    };
    match input {
        Some(data) if !data.is_empty() => {
            let preview = summarize_interaction_input(data);
            format!("Interacted with `{command_display}`, sent `{preview}`")
        }
        _ => format!("Waited for `{command_display}`"),
    }
}

fn summarize_interaction_input(input: &str) -> String {
    let single_line = input.replace('\n', "\\n");
    let sanitized = single_line.replace('`', "\\`");
    if sanitized.chars().count() <= MAX_INTERACTION_PREVIEW_CHARS {
        return sanitized;
    }

    let mut preview = String::new();
    for ch in sanitized.chars().take(MAX_INTERACTION_PREVIEW_CHARS) {
        preview.push(ch);
    }
    preview.push_str("...");
    preview
}

#[derive(Clone)]
pub(crate) struct OutputLines {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) omitted: usize,
}

pub(crate) fn cap_output_preview_rows(
    lines: Vec<Line<'static>>,
    row_limit: usize,
) -> Vec<Line<'static>> {
    cap_output_preview_rows_with_prior_omission(lines, row_limit, 0)
}

fn cap_output_preview_rows_with_prior_omission(
    lines: Vec<Line<'static>>,
    row_limit: usize,
    prior_omitted: usize,
) -> Vec<Line<'static>> {
    if row_limit == 0 || (prior_omitted == 0 && lines.len() <= row_limit) {
        return lines;
    }

    let visible_without_ellipsis = row_limit.saturating_sub(1);
    let retained = visible_without_ellipsis.min(lines.len());
    let head_count = retained.div_ceil(2);
    let tail_count = retained.saturating_sub(head_count);
    let omitted = prior_omitted.saturating_add(lines.len().saturating_sub(retained));

    let mut out = Vec::with_capacity(row_limit);
    out.extend(lines.iter().take(head_count).cloned());
    out.push(ExecCell::output_ellipsis_line(omitted));
    if tail_count > 0 {
        out.extend(
            lines[lines.len().saturating_sub(tail_count)..]
                .iter()
                .cloned(),
        );
    }
    out
}

pub(crate) fn output_lines(
    output: Option<&CommandOutput>,
    params: OutputLinesParams,
) -> OutputLines {
    let OutputLinesParams {
        line_limit,
        only_err,
        include_angle_pipe,
        include_prefix,
    } = params;
    let CommandOutput {
        aggregated_output, ..
    } = match output {
        Some(output) if only_err && output.exit_code == 0 => {
            return OutputLines {
                lines: Vec::new(),
                omitted: 0,
            };
        }
        Some(output) => output,
        None => {
            return OutputLines {
                lines: Vec::new(),
                omitted: 0,
            };
        }
    };

    let src = aggregated_output;
    let lines: Vec<&str> = src.lines().collect();
    let total = lines.len();
    let mut out: Vec<Line<'static>> = Vec::new();

    if line_limit == 0 || total <= line_limit {
        for (i, raw) in lines.iter().enumerate() {
            push_output_line(&mut out, raw, i, include_prefix, include_angle_pipe);
        }
        return OutputLines {
            lines: out,
            omitted: 0,
        };
    }

    let visible_without_ellipsis = line_limit.saturating_sub(1);
    let head_count = visible_without_ellipsis.div_ceil(2);
    let tail_count = visible_without_ellipsis.saturating_sub(head_count);

    for (i, raw) in lines[..head_count].iter().enumerate() {
        push_output_line(&mut out, raw, i, include_prefix, include_angle_pipe);
    }

    let tail_start = total.saturating_sub(tail_count);
    for (i, raw) in lines[tail_start..].iter().enumerate() {
        push_output_line(
            &mut out,
            raw,
            tail_start + i,
            include_prefix,
            include_angle_pipe,
        );
    }

    OutputLines {
        lines: out,
        omitted: total.saturating_sub(head_count + tail_count),
    }
}

fn push_output_line(
    out: &mut Vec<Line<'static>>,
    raw: &str,
    idx: usize,
    include_prefix: bool,
    include_angle_pipe: bool,
) {
    let mut line = ansi_escape_line(raw);
    let prefix = if !include_prefix {
        ""
    } else if idx == 0 && include_angle_pipe {
        "  └ "
    } else {
        "    "
    };
    line.spans.insert(0, prefix.into());
    line.spans.iter_mut().for_each(|span| {
        span.style = span.style.add_modifier(Modifier::DIM);
    });
    out.push(line);
}

fn activity_marker(start_time: Option<Instant>, animations_enabled: bool) -> Span<'static> {
    activity_indicator(
        start_time,
        MotionMode::from_animations_enabled(animations_enabled),
        ReducedMotionIndicator::StaticBullet,
    )
    .unwrap_or_else(|| "•".dim())
}

impl HistoryCell for ExecCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.is_exploring_cell() {
            self.exploring_display_lines(width)
        } else {
            self.command_display_lines(width)
        }
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = vec![];
        for (i, call) in self.iter_calls().enumerate() {
            if i > 0 {
                lines.push("".into());
            }
            let script = strip_bash_lc_and_escape(&call.command);
            let highlighted_script = highlight_bash_to_lines(&script);
            let cmd_display = adaptive_wrap_lines(
                &highlighted_script,
                RtOptions::new(width as usize)
                    .initial_indent("$ ".magenta().into())
                    .subsequent_indent("    ".into()),
            );
            lines.extend(cmd_display);

            if let Some(output) = call.output.as_ref() {
                if !call.is_unified_exec_interaction() {
                    let wrap_width = width.max(1) as usize;
                    let wrap_opts = RtOptions::new(wrap_width);
                    for unwrapped in output.formatted_output.lines().map(ansi_escape_line) {
                        let wrapped = adaptive_wrap_line(&unwrapped, wrap_opts.clone());
                        push_owned_lines(&wrapped, &mut lines);
                    }
                }
                let duration = call
                    .duration
                    .map(format_duration)
                    .unwrap_or_else(|| "unknown".to_string());
                let mut result: Line = if output.exit_code == 0 {
                    Line::from("✓".green().bold())
                } else {
                    Line::from(vec![
                        "✗".red().bold(),
                        format!(" ({})", output.exit_code).into(),
                    ])
                };
                result.push_span(format!(" • {duration}").dim());
                lines.push(result);
            }
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.transcript_lines(u16::MAX))
    }
}

impl ExecCell {
    fn output_ellipsis_text(omitted: usize) -> String {
        format!("… +{omitted} lines ({TRANSCRIPT_HINT})")
    }

    fn output_ellipsis_line(omitted: usize) -> Line<'static> {
        Line::from(vec![Self::output_ellipsis_text(omitted).dim()])
    }

    fn exploring_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        out.push(Line::from(vec![
            if self.is_active() {
                activity_marker(self.active_start_time(), self.animations_enabled())
            } else {
                "•".dim()
            },
            " ".into(),
            if self.is_active() {
                "Exploring".bold()
            } else {
                "Explored".bold()
            },
        ]));

        let mut calls = self.calls.clone();
        let mut out_indented = Vec::new();
        while !calls.is_empty() {
            let mut call = calls.remove(0);
            if call
                .parsed
                .iter()
                .all(|parsed| matches!(parsed, ParsedCommand::Read { .. }))
            {
                while let Some(next) = calls.first() {
                    if next
                        .parsed
                        .iter()
                        .all(|parsed| matches!(parsed, ParsedCommand::Read { .. }))
                    {
                        call.parsed.extend(next.parsed.clone());
                        calls.remove(0);
                    } else {
                        break;
                    }
                }
            }

            let reads_only = call
                .parsed
                .iter()
                .all(|parsed| matches!(parsed, ParsedCommand::Read { .. }));

            let call_lines: Vec<(&str, Vec<Span<'static>>)> = if reads_only {
                let names = call
                    .parsed
                    .iter()
                    .map(|parsed| match parsed {
                        ParsedCommand::Read { name, .. } => name.clone(),
                        _ => unreachable!(),
                    })
                    .unique();
                vec![(
                    "Read",
                    Itertools::intersperse(names.into_iter().map(Into::into), ", ".dim()).collect(),
                )]
            } else {
                let mut lines = Vec::new();
                for parsed in &call.parsed {
                    match parsed {
                        ParsedCommand::Read { name, .. } => {
                            lines.push(("Read", vec![name.clone().into()]));
                        }
                        ParsedCommand::ListFiles { cmd, path } => {
                            lines.push(("List", vec![path.clone().unwrap_or(cmd.clone()).into()]));
                        }
                        ParsedCommand::Search { cmd, query, path } => {
                            let spans = match (query, path) {
                                (Some(q), Some(p)) => {
                                    vec![q.clone().into(), " in ".dim(), p.clone().into()]
                                }
                                (Some(q), None) => vec![q.clone().into()],
                                _ => vec![cmd.clone().into()],
                            };
                            lines.push(("Search", spans));
                        }
                        ParsedCommand::Unknown { cmd } => {
                            lines.push(("Run", vec![cmd.clone().into()]));
                        }
                    }
                }
                lines
            };

            for (title, line) in call_lines {
                let line = Line::from(line);
                let initial_indent = Line::from(vec![title.cyan(), " ".into()]);
                let subsequent_indent = " ".repeat(initial_indent.width()).into();
                let wrapped = adaptive_wrap_line(
                    &line,
                    RtOptions::new(width as usize)
                        .initial_indent(initial_indent)
                        .subsequent_indent(subsequent_indent),
                );
                push_owned_lines(&wrapped, &mut out_indented);
            }
        }

        out.extend(prefix_lines(out_indented, "  └ ".dim(), "    ".into()));
        out
    }

    fn command_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let [call] = &self.calls.as_slice() else {
            panic!("Expected exactly one call in a command display cell");
        };
        let layout = EXEC_DISPLAY_LAYOUT;
        let success = call.output.as_ref().map(|o| o.exit_code == 0);
        let bullet = match success {
            Some(true) => "•".green().bold(),
            Some(false) => "•".red().bold(),
            None => activity_marker(call.start_time, self.animations_enabled()),
        };
        let is_interaction = call.is_unified_exec_interaction();
        let title = if is_interaction {
            ""
        } else if self.is_active() {
            "Running"
        } else if call.is_user_shell_command() {
            "You ran"
        } else {
            "Ran"
        };

        let mut header_line = if is_interaction {
            Line::from(vec![bullet.clone(), " ".into()])
        } else {
            Line::from(vec![bullet.clone(), " ".into(), title.bold(), " ".into()])
        };
        let header_prefix_width = header_line.width();

        let cmd_display = if call.is_unified_exec_interaction() {
            format_unified_exec_interaction(&call.command, call.interaction_input.as_deref())
        } else {
            strip_bash_lc_and_escape(&call.command)
        };
        let highlighted_lines = highlight_bash_to_lines(&cmd_display);

        let continuation_wrap_width = layout.command_continuation.wrap_width(width);
        let continuation_opts =
            RtOptions::new(continuation_wrap_width).word_splitter(WordSplitter::NoHyphenation);

        let mut continuation_lines: Vec<Line<'static>> = Vec::new();

        if let Some((first, rest)) = highlighted_lines.split_first() {
            let available_first_width = (width as usize).saturating_sub(header_prefix_width).max(1);
            let first_opts =
                RtOptions::new(available_first_width).word_splitter(WordSplitter::NoHyphenation);

            let mut first_wrapped: Vec<Line<'static>> = Vec::new();
            push_owned_lines(&adaptive_wrap_line(first, first_opts), &mut first_wrapped);
            let mut first_wrapped_iter = first_wrapped.into_iter();
            if let Some(first_segment) = first_wrapped_iter.next() {
                header_line.extend(first_segment);
            }
            continuation_lines.extend(first_wrapped_iter);

            for line in rest {
                push_owned_lines(
                    &adaptive_wrap_line(line, continuation_opts.clone()),
                    &mut continuation_lines,
                );
            }
        }

        let mut lines: Vec<Line<'static>> = vec![header_line];

        if !continuation_lines.is_empty() {
            lines.extend(prefix_lines(
                continuation_lines,
                Span::from(layout.command_continuation.initial_prefix).dim(),
                Span::from(layout.command_continuation.subsequent_prefix).dim(),
            ));
        }

        if let Some(output) = call.output.as_ref() {
            let line_limit = self.output_preview_lines(call.source);
            let raw_output = output_lines(
                Some(output),
                OutputLinesParams {
                    line_limit,
                    only_err: false,
                    include_angle_pipe: false,
                    include_prefix: false,
                },
            );
            if raw_output.lines.is_empty() && raw_output.omitted == 0 {
                if !call.is_unified_exec_interaction() {
                    lines.extend(prefix_lines(
                        vec![Line::from("(no output)".dim())],
                        Span::from(layout.output_block.initial_prefix).dim(),
                        Span::from(layout.output_block.subsequent_prefix),
                    ));
                }
            } else {
                let mut wrapped_output: Vec<Line<'static>> = Vec::new();
                let output_wrap_width = layout.output_block.wrap_width(width);
                let output_opts =
                    RtOptions::new(output_wrap_width).word_splitter(WordSplitter::NoHyphenation);
                for line in &raw_output.lines {
                    push_owned_lines(
                        &adaptive_wrap_line(line, output_opts.clone()),
                        &mut wrapped_output,
                    );
                }
                let wrapped_output = cap_output_preview_rows_with_prior_omission(
                    wrapped_output,
                    line_limit,
                    raw_output.omitted,
                );

                let prefixed_output = prefix_lines(
                    wrapped_output,
                    Span::from(layout.output_block.initial_prefix).dim(),
                    Span::from(layout.output_block.subsequent_prefix),
                );

                if !prefixed_output.is_empty() {
                    lines.extend(prefixed_output);
                }
            }
        }

        lines
    }
}

#[derive(Clone, Copy)]
struct PrefixedBlock {
    initial_prefix: &'static str,
    subsequent_prefix: &'static str,
}

impl PrefixedBlock {
    const fn new(initial_prefix: &'static str, subsequent_prefix: &'static str) -> Self {
        Self {
            initial_prefix,
            subsequent_prefix,
        }
    }

    fn wrap_width(self, total_width: u16) -> usize {
        let prefix_width = UnicodeWidthStr::width(self.initial_prefix)
            .max(UnicodeWidthStr::width(self.subsequent_prefix));
        usize::from(total_width).saturating_sub(prefix_width).max(1)
    }
}

#[derive(Clone, Copy)]
struct ExecDisplayLayout {
    command_continuation: PrefixedBlock,
    output_block: PrefixedBlock,
}

impl ExecDisplayLayout {
    const fn new(command_continuation: PrefixedBlock, output_block: PrefixedBlock) -> Self {
        Self {
            command_continuation,
            output_block,
        }
    }
}

const EXEC_DISPLAY_LAYOUT: ExecDisplayLayout = ExecDisplayLayout::new(
    PrefixedBlock::new("  │ ", "  │ "),
    PrefixedBlock::new("  └ ", "    "),
);

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::CommandExecutionSource as ExecCommandSource;
    use pretty_assertions::assert_eq;

    fn render_line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn command_output_wraps_retained_long_lines_without_inline_truncation() {
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo long".into()],
            parsed: Vec::new(),
            output: Some(CommandOutput {
                exit_code: 0,
                aggregated_output:
                    "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda omega"
                        .to_string(),
                formatted_output: String::new(),
            }),
            source: ExecCommandSource::UserShell,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let rendered_text = cell
            .command_display_lines(/*width*/ 32)
            .iter()
            .map(render_line_text)
            .join("\n");

        assert!(
            !rendered_text.contains("[...]") && !rendered_text.contains("ctrl + t"),
            "expected retained output line to wrap without truncation markers, got:\n{rendered_text}"
        );
        assert!(
            rendered_text.contains("omega"),
            "expected tail of retained output line to remain visible, got:\n{rendered_text}"
        );
    }

    #[test]
    fn command_output_limit_counts_wrapped_rows() {
        let output = (1..=12)
            .map(|idx| {
                format!(
                    "line-{idx:02} alpha beta gamma delta epsilon zeta eta theta iota kappa omega"
                )
            })
            .join("\n");
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "printf lines".into()],
            parsed: Vec::new(),
            output: Some(CommandOutput {
                exit_code: 0,
                aggregated_output: output,
                formatted_output: String::new(),
            }),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false)
            .with_output_preview_line_limits(OutputPreviewLineLimits {
                command: 6,
                user_shell: 50,
            });
        let rendered = cell.command_display_lines(/*width*/ 32);
        let rendered_text = rendered.iter().map(render_line_text).join("\n");

        assert!(
            rendered_text.contains("… +17 lines (ctrl + t to view transcript)"),
            "expected logical and wrapped omissions to be accumulated, got:\n{rendered_text}"
        );
        assert!(
            rendered.len() <= 7,
            "expected header plus six output preview rows at most, got {} rows:\n{rendered_text}",
            rendered.len()
        );
    }

    #[test]
    fn output_lines_ellipsis_includes_transcript_hint() {
        let output = CommandOutput {
            exit_code: 0,
            aggregated_output: (1..=7).map(|n| n.to_string()).join("\n"),
            formatted_output: String::new(),
        };

        let OutputLines { lines, omitted } = output_lines(
            Some(&output),
            OutputLinesParams {
                line_limit: 2,
                only_err: false,
                include_angle_pipe: false,
                include_prefix: false,
            },
        );
        let rendered: Vec<String> = cap_output_preview_rows_with_prior_omission(lines, 2, omitted)
            .iter()
            .map(render_line_text)
            .collect();

        assert!(
            rendered
                .iter()
                .any(|line| line.contains("… +6 lines (ctrl + t to view transcript)")),
            "expected logical truncation to include transcript hint, got: {rendered:?}"
        );
    }

    #[test]
    fn output_preview_accumulates_prior_and_wrapped_omissions() {
        let lines = (1..=8)
            .map(|idx| Line::from(format!("row {idx}")))
            .collect();
        let rendered = cap_output_preview_rows_with_prior_omission(lines, 5, 97)
            .iter()
            .map(render_line_text)
            .join("\n");

        insta::assert_snapshot!(rendered, @r"
        row 1
        row 2
        … +101 lines (ctrl + t to view transcript)
        row 7
        row 8
        ");
    }

    #[test]
    fn output_lines_zero_limit_preserves_all_lines_without_ellipsis() {
        let output = CommandOutput {
            exit_code: 0,
            aggregated_output: (1..=12).map(|n| n.to_string()).join("\n"),
            formatted_output: String::new(),
        };

        let rendered: Vec<String> = output_lines(
            Some(&output),
            OutputLinesParams {
                line_limit: 0,
                only_err: false,
                include_angle_pipe: false,
                include_prefix: false,
            },
        )
        .lines
        .iter()
        .map(render_line_text)
        .collect();

        assert_eq!(
            rendered,
            (1..=12).map(|n| n.to_string()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn command_display_preserves_full_multiline_command() {
        let command = std::iter::once("set -euo pipefail".to_string())
            .chain((1..=24).map(|idx| format!("echo setup line {idx:02}")))
            .chain([
                "repo=ignatremizov/codex".to_string(),
                "parent=$(git rev-parse origin/fork)".to_string(),
                "head=$(git rev-parse HEAD)".to_string(),
                "gh workflow run manual-release-build.yml --repo \"$repo\" --ref fork".to_string(),
            ])
            .join("\n");
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), command],
            parsed: Vec::new(),
            output: Some(CommandOutput {
                exit_code: 0,
                formatted_output: String::new(),
                aggregated_output: String::new(),
            }),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let rendered: Vec<String> = cell
            .command_display_lines(/*width*/ 36)
            .iter()
            .map(render_line_text)
            .collect();
        let rendered_text = rendered.join("\n");

        assert!(
            !rendered_text.contains("… +"),
            "expected full command without command-line ellipsis, got:\n{rendered_text}"
        );
        assert!(
            rendered_text.contains("gh workflow run"),
            "expected final command line to remain visible, got:\n{rendered_text}"
        );
    }

    #[test]
    fn command_display_does_not_split_long_url_token() {
        let url = "http://example.com/long-url-with-dashes-wider-than-terminal-window/blah-blah-blah-text/more-gibberish-text";

        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), format!("echo {url}")],
            parsed: Vec::new(),
            output: None,
            source: ExecCommandSource::UserShell,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let rendered: Vec<String> = cell
            .command_display_lines(/*width*/ 36)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(
            rendered.iter().filter(|line| line.contains(url)).count(),
            1,
            "expected full URL in one rendered line, got: {rendered:?}"
        );
    }

    #[test]
    fn active_command_without_animations_is_stable() {
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo done".into()],
            parsed: Vec::new(),
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let first: Vec<String> = cell
            .command_display_lines(/*width*/ 80)
            .iter()
            .map(render_line_text)
            .collect();
        let second: Vec<String> = cell
            .command_display_lines(/*width*/ 80)
            .iter()
            .map(render_line_text)
            .collect();

        assert_eq!(first, second);
        assert_eq!(first, vec!["• Running echo done".to_string()]);
    }

    #[test]
    fn exploring_display_does_not_split_long_url_like_search_query() {
        let url_like = "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/artifacts/reports/performance/summary/detail/with/a/very/long/path";
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "rg foo".into()],
            parsed: vec![ParsedCommand::Search {
                cmd: format!("rg {url_like}"),
                query: Some(url_like.to_string()),
                path: None,
            }],
            output: None,
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let rendered: Vec<String> = cell
            .display_lines(/*width*/ 36)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.contains(url_like))
                .count(),
            1,
            "expected full URL-like query in one rendered line, got: {rendered:?}"
        );
    }

    #[test]
    fn output_display_does_not_split_long_url_like_token_without_scheme() {
        let url = "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/artifacts/reports/performance/summary/detail/session_id=abc123def456ghi789jkl012mno345pqr678";

        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo done".into()],
            parsed: Vec::new(),
            output: Some(CommandOutput {
                exit_code: 0,
                formatted_output: String::new(),
                aggregated_output: url.to_string(),
            }),
            source: ExecCommandSource::UserShell,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let rendered: Vec<String> = cell
            .command_display_lines(/*width*/ 36)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(
            rendered.iter().filter(|line| line.contains(url)).count(),
            1,
            "expected full URL-like token in one rendered line, got: {rendered:?}"
        );
    }

    #[test]
    fn desired_transcript_height_accounts_for_wrapped_url_like_rows() {
        let url = "https://example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/artifacts/reports/performance/summary/detail/with/a/very/long/path/that/keeps/going/for/testing/purposes";
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo done".into()],
            parsed: Vec::new(),
            output: Some(CommandOutput {
                exit_code: 0,
                formatted_output: url.to_string(),
                aggregated_output: url.to_string(),
            }),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let width: u16 = 36;
        let logical_height = cell.transcript_lines(width).len() as u16;
        let wrapped_height = cell.desired_transcript_height(width);

        assert!(
            wrapped_height > logical_height,
            "expected transcript height to account for wrapped URL-like rows, logical_height={logical_height}, wrapped_height={wrapped_height}"
        );
    }
}
