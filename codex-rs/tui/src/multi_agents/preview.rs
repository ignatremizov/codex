//! Width-dependent collaboration previews retain their complete source for raw history and export.

use super::*;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::LogicalLineSource;
use crate::terminal_hyperlinks::annotate_web_urls_in_line;
use crate::terminal_hyperlinks::remap_source_wrapped_line;
use crate::terminal_hyperlinks::visible_lines;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_line_with_source;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AgentPreviewLineLimits {
    pub(crate) prompt: usize,
    pub(crate) response: usize,
}
impl Default for AgentPreviewLineLimits {
    fn default() -> Self {
        Self {
            prompt: codex_config::types::DEFAULT_TUI_AGENT_PROMPT_PREVIEW_LINES,
            response: codex_config::types::DEFAULT_TUI_AGENT_RESPONSE_PREVIEW_LINES,
        }
    }
}
impl From<&codex_config::types::Tui> for AgentPreviewLineLimits {
    fn from(tui: &codex_config::types::Tui) -> Self {
        Self {
            prompt: tui.agent_prompt_preview_lines,
            response: tui.agent_response_preview_lines,
        }
    }
}

#[derive(Debug)]
pub(crate) struct CollabAgentHistoryCell {
    title: Line<'static>,
    details: Vec<CollabDetail>,
}

impl CollabAgentHistoryCell {
    pub(super) fn new(title: Line<'static>, details: Vec<CollabDetail>) -> Self {
        Self { title, details }
    }
}

impl HistoryCell for CollabAgentHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.display_hyperlink_lines(width))
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        if width == 0 {
            return Vec::new();
        }
        let mut lines = vec![HyperlinkLine::new(self.title.clone())];
        let mut first_detail = true;
        for detail in &self.details {
            let detail_lines = detail.display_lines(width, first_detail);
            if !detail_lines.is_empty() {
                first_detail = false;
                lines.extend(detail_lines);
            }
        }
        lines
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![self.title.clone()];
        let mut first = true;
        for detail in &self.details {
            let source = match detail {
                CollabDetail::Lines(lines) | CollabDetail::Preview { lines, .. } => lines,
            };
            if !source.is_empty() {
                lines.extend(crate::render::line_utils::prefix_lines(
                    source.clone(),
                    if first {
                        "  └ ".into()
                    } else {
                        "    ".into()
                    },
                    "    ".into(),
                ));
                first = false;
            }
        }
        crate::history_cell::plain_lines(lines)
    }
}

#[derive(Clone, Debug)]
pub(super) enum CollabDetail {
    Lines(Vec<Line<'static>>),
    Preview {
        lines: Vec<Line<'static>>,
        max_rows: usize,
        marker_indent: &'static str,
    },
}

impl CollabDetail {
    pub(super) fn line(line: Line<'static>) -> Self {
        Self::Lines(vec![line])
    }

    pub(super) fn lines(lines: Vec<Line<'static>>) -> Self {
        Self::Lines(lines)
    }

    pub(super) fn preview(lines: Vec<Line<'static>>, max_rows: usize) -> Self {
        Self::Preview {
            lines,
            max_rows,
            marker_indent: "",
        }
    }

    pub(super) fn preview_with_marker_indent(
        lines: Vec<Line<'static>>,
        max_rows: usize,
        marker_indent: &'static str,
    ) -> Self {
        Self::Preview {
            lines,
            max_rows,
            marker_indent,
        }
    }

    fn display_lines(&self, width: u16, first_detail: bool) -> Vec<HyperlinkLine> {
        match self {
            Self::Lines(lines) => wrap_detail_lines(lines, width, first_detail),
            Self::Preview {
                lines,
                max_rows,
                marker_indent,
            } => {
                let wrapped = wrap_detail_lines(lines, width, first_detail);
                cap_preview_rows(wrapped, *max_rows, first_detail, marker_indent, width)
            }
        }
    }
}

fn wrap_detail_lines(
    lines: &[Line<'static>],
    width: u16,
    first_detail: bool,
) -> Vec<HyperlinkLine> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let prefix = if first_detail && index == 0 {
            "  └ "
        } else {
            "    "
        };
        // Leave room for a double-width grapheme even in very narrow viewports.
        let indent_width = usize::from(width.saturating_sub(2));
        let options = RtOptions::new(usize::from(width))
            .initial_indent(crate::line_truncation::truncate_line_to_width(
                Line::from(prefix.dim()),
                indent_width,
            ))
            .subsequent_indent(crate::line_truncation::truncate_line_to_width(
                Line::from("    "),
                indent_width,
            ));
        let mut source = annotate_web_urls_in_line(line.clone());
        let mut logical = LogicalLineSource::from_line(line);
        logical.continuation_indent = options.subsequent_indent.clone();
        source.source = Some(logical);
        let mut wrapped = word_wrap_line_with_source(line, options);
        for row in &mut wrapped {
            if crate::line_truncation::line_width(&row.line) > usize::from(width) {
                // A grapheme wider than the entire viewport cannot be displayed. Keep its
                // full logical source, but do not let a second renderer exceed the row budget.
                row.line = crate::line_truncation::truncate_line_to_width(
                    crate::render::line_utils::line_to_static(&row.line),
                    usize::from(width),
                );
                let displayed_bytes = row
                    .line
                    .spans
                    .iter()
                    .map(|span| span.content.len())
                    .sum::<usize>();
                row.range.end = row.range.start + displayed_bytes.saturating_sub(row.prefix_bytes);
            }
        }
        rows.extend(remap_source_wrapped_line(&source, wrapped));
    }
    rows
}

fn cap_preview_rows(
    mut lines: Vec<HyperlinkLine>,
    max_rows: usize,
    first_detail: bool,
    marker_indent: &str,
    width: u16,
) -> Vec<HyperlinkLine> {
    if max_rows == UNLIMITED_AGENT_PREVIEW_ROWS || lines.len() <= max_rows {
        return lines;
    }

    let visible_rows = max_rows.saturating_sub(1);
    let omitted = lines.len().saturating_sub(visible_rows);
    lines.truncate(visible_rows);
    lines.push(HyperlinkLine::new(
        crate::line_truncation::truncate_line_to_width(
            hidden_rows_marker(omitted, first_detail && visible_rows == 0, marker_indent),
            usize::from(width),
        ),
    ));
    lines
}

fn hidden_rows_marker(
    omitted: usize,
    use_initial_prefix: bool,
    marker_indent: &str,
) -> Line<'static> {
    let prefix = if use_initial_prefix { "  └ " } else { "    " };
    let suffix = if omitted == 1 { "row" } else { "rows" };
    vec![
        Span::from(prefix).dim(),
        Span::from(marker_indent.to_string()).dim(),
        Span::from(format!("… +{omitted} {suffix} hidden")).dim(),
    ]
    .into()
}

pub(super) fn wait_complete_agent_lines(
    thread_id: ThreadId,
    metadata: &AgentMetadata,
    status: &CollabAgentState,
    agent_response_preview_lines: usize,
) -> Vec<CollabDetail> {
    let mut spans = agent_label_spans(agent_label(thread_id, metadata));
    spans.push(Span::from(": ").dim());
    spans.extend(status_label_spans(&status.status));

    let message = match status.status {
        CollabAgentStatus::Completed | CollabAgentStatus::Errored => status.message.as_deref(),
        CollabAgentStatus::PendingInit
        | CollabAgentStatus::Running
        | CollabAgentStatus::Interrupted
        | CollabAgentStatus::Shutdown
        | CollabAgentStatus::NotFound => None,
    };
    let message_lines = message
        .map(|message| {
            let trimmed = message.trim_matches(['\n', '\r']);
            if trimmed.is_empty() {
                Vec::new()
            } else {
                trimmed
                    .split('\n')
                    .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
                    .collect::<Vec<_>>()
            }
        })
        .unwrap_or_default();
    if message_lines.is_empty() {
        if matches!(status.status, CollabAgentStatus::Errored) && message.is_none() {
            spans.push(Span::from(" - ").dim());
            spans.push(Span::from("Agent errored"));
        }
        return vec![CollabDetail::line(spans.into())];
    }

    if message_lines.len() == 1 && agent_response_preview_lines == UNLIMITED_AGENT_PREVIEW_ROWS {
        spans.push(Span::from(" - ").dim());
        spans.push(Span::from(message_lines[0].clone()));
        return vec![CollabDetail::line(spans.into())];
    }

    let mut details = vec![CollabDetail::line(spans.into())];
    details.push(CollabDetail::preview_with_marker_indent(
        message_lines
            .into_iter()
            .map(|line| vec![Span::from("  ").dim(), Span::from(line)].into())
            .collect(),
        agent_response_preview_lines,
        "  ",
    ));
    details
}

pub(super) fn preview_source_lines(source: &str) -> Vec<Line<'static>> {
    let trimmed = source.trim_matches(['\n', '\r']);
    if trimmed.trim().is_empty() {
        Vec::new()
    } else {
        trimmed
            .split('\n')
            .map(|line| Line::from(line.strip_suffix('\r').unwrap_or(line).to_string()))
            .collect()
    }
}
