//! Informational, warning, update, and policy notice history cells.

use super::*;

#[cfg_attr(debug_assertions, allow(dead_code))]
#[derive(Debug)]
pub(crate) struct UpdateAvailableHistoryCell {
    latest_version: String,
    update_action: Option<UpdateAction>,
}

#[cfg_attr(debug_assertions, allow(dead_code))]
impl UpdateAvailableHistoryCell {
    pub(crate) fn new(latest_version: String, update_action: Option<UpdateAction>) -> Self {
        Self {
            latest_version,
            update_action,
        }
    }
}

impl HistoryCell for UpdateAvailableHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        use ratatui_macros::line;
        use ratatui_macros::text;
        let update_instruction = if let Some(update_action) = self.update_action {
            line!["Run ", update_action.command_str().cyan(), " to update."]
        } else {
            line![
                "See ",
                "https://github.com/openai/codex".cyan().underlined(),
                " for installation options."
            ]
        };

        let content = text![
            line![
                padded_emoji("✨").bold().cyan(),
                "Update available!".bold().cyan(),
                " ",
                format!("{CODEX_CLI_VERSION_FOR_DISPLAY} -> {}", self.latest_version).bold(),
            ],
            update_instruction,
            "",
            "See full release notes:",
            "https://github.com/openai/codex/releases/latest"
                .cyan()
                .underlined(),
        ];

        let inner_width = content
            .width()
            .min(usize::from(width.saturating_sub(4)))
            .max(1);
        let lines = adaptive_wrap_lines(content.lines, RtOptions::new(inner_width));
        with_border_with_inner_width(lines, inner_width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let update_instruction = if let Some(update_action) = self.update_action {
            format!("Run {} to update.", update_action.command_str())
        } else {
            "See https://github.com/openai/codex for installation options.".to_string()
        };
        vec![
            Line::from("Update available!"),
            Line::from(format!(
                "{CODEX_CLI_VERSION_FOR_DISPLAY} -> {}",
                self.latest_version
            )),
            Line::from(update_instruction),
            Line::from(""),
            Line::from("See full release notes:"),
            Line::from("https://github.com/openai/codex/releases/latest"),
        ]
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        crate::terminal_hyperlinks::annotate_web_urls(self.display_lines(width))
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
    }
}
#[allow(clippy::disallowed_methods)]
pub(crate) fn new_warning_event(message: String) -> PrefixedWrappedHistoryCell {
    PrefixedWrappedHistoryCell::new(message.yellow(), "⚠ ".yellow(), "  ")
}

const TRUSTED_ACCESS_FOR_CYBER_URL: &str = "https://chatgpt.com/cyber";

#[derive(Debug)]
pub(crate) struct CyberPolicyNoticeCell;

pub(crate) fn new_cyber_policy_error_event() -> CyberPolicyNoticeCell {
    CyberPolicyNoticeCell
}

impl HistoryCell for CyberPolicyNoticeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(
            vec![
                "ⓘ ".cyan(),
                "This chat was flagged for possible cybersecurity risk".bold(),
            ]
            .into(),
        );

        let wrap_width = width.saturating_sub(2).max(1) as usize;
        let body = Line::from(vec![
            "  If this seems wrong, try rephrasing your request. To get authorized for security work, join the "
                .dim(),
            "Trusted Access for Cyber".cyan().underlined(),
            " program.".dim(),
        ]);
        let wrapped = adaptive_wrap_line(
            &body,
            RtOptions::new(wrap_width).subsequent_indent("  ".into()),
        );
        push_owned_lines(&wrapped, &mut lines);
        lines.push(
            vec![
                "  ".into(),
                TRUSTED_ACCESS_FOR_CYBER_URL.cyan().underlined(),
            ]
            .into(),
        );

        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![
            Line::from("This chat was flagged for possible cybersecurity risk"),
            Line::from(
                "If this seems wrong, try rephrasing your request. To get authorized for security work, join the Trusted Access for Cyber program.",
            ),
            Line::from(TRUSTED_ACCESS_FOR_CYBER_URL),
        ]
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        crate::terminal_hyperlinks::annotate_web_urls(self.display_lines(width))
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
    }
}

#[derive(Debug)]
pub(crate) struct DeprecationNoticeCell {
    summary: String,
    details: Option<String>,
}

pub(crate) fn new_deprecation_notice(
    summary: String,
    details: Option<String>,
) -> DeprecationNoticeCell {
    DeprecationNoticeCell { summary, details }
}

impl HistoryCell for DeprecationNoticeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(vec!["⚠ ".red().bold(), self.summary.clone().red()].into());

        let wrap_width = width.saturating_sub(4).max(1) as usize;

        if let Some(details) = &self.details {
            let detail_line = Line::from(details.clone().dim());
            let wrapped = adaptive_wrap_line(&detail_line, RtOptions::new(wrap_width));
            push_owned_lines(&wrapped, &mut lines);
        }

        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(self.summary.clone())];
        if let Some(details) = &self.details {
            lines.extend(raw_lines_from_source(details));
        }
        lines
    }
}

fn wrap_plain_text(source: &str, width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for line in source.lines() {
        if line.trim().is_empty() {
            wrapped.push(String::new());
            continue;
        }

        for chunk in textwrap::wrap(line, width) {
            wrapped.push(chunk.into_owned());
        }
    }
    wrapped
}

#[derive(Debug)]
pub(crate) struct CompactionSummaryCell {
    summary: String,
}

pub(crate) fn new_compaction_summary(summary: String) -> CompactionSummaryCell {
    CompactionSummaryCell { summary }
}

impl HistoryCell for CompactionSummaryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let mut lines = Vec::new();
        lines.push(vec!["• ".dim(), "Compacted summary".bold()].into());

        let summary = self.summary.trim();
        if summary.is_empty() {
            lines.push(vec!["  ".into(), "(summary was empty)".dim()].into());
            return lines;
        }

        let wrap_width = width.saturating_sub(2).max(1) as usize;
        let wrapped_lines = wrap_plain_text(summary, wrap_width)
            .into_iter()
            .map(Line::from)
            .collect();
        lines.extend(prefix_lines(wrapped_lines, "  ".into(), "  ".into()));

        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from("Compacted summary")];
        lines.extend(raw_lines_from_source(self.summary.trim()));
        lines
    }
}

#[derive(Debug)]
pub(crate) struct CompactionPromptCell {
    message: String,
}

pub(crate) fn new_compaction_prompt(message: String) -> CompactionPromptCell {
    CompactionPromptCell { message }
}

impl HistoryCell for CompactionPromptCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let mut lines = Vec::new();
        lines.push(vec!["• ".dim(), "Compacted prompt".bold()].into());

        let message = self.message.trim();
        if message.is_empty() {
            lines.push(vec!["  ".into(), "(prompt was empty)".dim()].into());
            return lines;
        }

        let wrap_width = width.saturating_sub(2).max(1) as usize;
        let wrapped_lines = wrap_plain_text(message, wrap_width)
            .into_iter()
            .map(Line::from)
            .collect();
        lines.extend(prefix_lines(wrapped_lines, "  ".into(), "  ".into()));

        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from("Compacted prompt")];
        lines.extend(raw_lines_from_source(self.message.trim()));
        lines
    }
}

pub(crate) fn new_info_event(message: String, hint: Option<String>) -> PlainHistoryCell {
    let mut line = vec!["• ".dim(), message.into()];
    if let Some(hint) = hint {
        line.push(" ".into());
        line.push(hint.dark_gray());
    }
    let lines: Vec<Line<'static>> = vec![line.into()];
    PlainHistoryCell { lines }
}

pub(crate) fn new_error_event(message: String) -> PlainHistoryCell {
    // Use a hair space (U+200A) to create a subtle, near-invisible separation
    // before the text. VS16 is intentionally omitted to keep spacing tighter
    // in terminals like Ghostty.
    let lines: Vec<Line<'static>> = vec![vec![format!("■ {message}").red()].into()];
    PlainHistoryCell { lines }
}
