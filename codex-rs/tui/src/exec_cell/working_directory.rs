//! Command directories are presentation metadata, independent of selectable shell source.
//! Groups share a directory; a transition applies to subsequent commands until it changes again.

use super::model::ExecCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::sanitize_user_text;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::adaptive_wrap_hyperlink_lines;
use crate::terminal_hyperlinks::visible_lines;
use crate::wrapping::RtOptions;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(crate) struct CommandDirectory {
    pub(crate) cwd: LegacyAppPathString,
    base: PathUri,
    changed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum CommandDirectoryDisplay {
    #[default]
    Changes,
    Relative,
    Full,
    Hidden,
}

/// Resolve page boundaries from retained source order, including intervening non-command cells.
pub(crate) fn command_directory_display_after(
    cell: &dyn HistoryCell,
    preceding: &[Arc<dyn HistoryCell>],
) -> CommandDirectoryDisplay {
    let Some(directory) = cell.command_directory() else {
        return CommandDirectoryDisplay::Changes;
    };
    match preceding
        .iter()
        .rev()
        .find_map(|previous| previous.command_directory())
    {
        Some(previous) if directory.matches(previous) => CommandDirectoryDisplay::Hidden,
        Some(_) => CommandDirectoryDisplay::Relative,
        None => CommandDirectoryDisplay::Changes,
    }
}

impl CommandDirectory {
    pub(crate) fn new(
        cwd: LegacyAppPathString,
        base: PathUri,
        previous: Option<&LegacyAppPathString>,
    ) -> Self {
        let previous = previous
            .cloned()
            .unwrap_or_else(|| LegacyAppPathString::from(base.clone()));
        let changed = !same_directory(&cwd, &previous);
        Self { cwd, base, changed }
    }

    pub(crate) fn matches(&self, other: &Self) -> bool {
        same_directory(&self.cwd, &other.cwd)
    }

    pub(crate) fn show_transition(&mut self) {
        self.changed = true;
    }

    pub(crate) fn prepend_lines(
        &self,
        width: u16,
        mut lines: Vec<HyperlinkLine>,
    ) -> Vec<HyperlinkLine> {
        if lines.is_empty() {
            return lines;
        }
        let mut prefix = self.lines(width, CommandDirectoryDisplay::Changes);
        prefix.append(&mut lines);
        prefix
    }

    /// Replace the label while preserving every command's source ranges and output.
    pub(crate) fn replace_lines(
        &self,
        width: u16,
        display: CommandDirectoryDisplay,
        mut lines: Vec<HyperlinkLine>,
    ) -> Vec<HyperlinkLine> {
        if lines.is_empty() {
            return lines;
        }
        let prefix_len = self.lines(width, CommandDirectoryDisplay::Changes).len();
        let mut prefix = self.lines(width, display);
        prefix.extend(lines.drain(prefix_len..));
        prefix
    }

    fn lines(&self, width: u16, display: CommandDirectoryDisplay) -> Vec<HyperlinkLine> {
        match display {
            CommandDirectoryDisplay::Hidden => return Vec::new(),
            CommandDirectoryDisplay::Changes if !self.changed => return Vec::new(),
            CommandDirectoryDisplay::Changes
            | CommandDirectoryDisplay::Relative
            | CommandDirectoryDisplay::Full => {}
        }
        // URI display normalization may discard controls. Preserve their spelling
        // until it can be escaped below, so distinct path names remain distinguishable.
        let has_controls = self.cwd.as_str().chars().any(char::is_control);
        let full = if has_controls {
            self.cwd.as_str().to_string()
        } else {
            self.cwd.render_for_ui()
        };
        let label = if display == CommandDirectoryDisplay::Full || has_controls {
            full
        } else {
            self.cwd
                .to_inferred_path_uri()
                .and_then(|cwd| cwd.relative_path_from(&self.base))
                .map(|relative| {
                    if relative.is_empty() {
                        ".".to_string()
                    } else {
                        relative
                    }
                })
                .unwrap_or(full)
        };
        // Keep path text on one logical line even for names containing terminal controls.
        let label = label
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        let label = sanitize_user_text(label.into()).into_owned();
        let mut lines = adaptive_wrap_hyperlink_lines(
            &[Line::from(vec!["  ↳ cwd ".dim(), label.cyan()]).into()],
            RtOptions::new(usize::from(width.max(/*other*/ 1))),
        );
        for line in &mut lines {
            line.source = None;
        }
        lines
    }
}

/// Legacy/noncanonical and unfinished historical commands keep their audit presentation.
#[derive(Debug)]
pub(crate) struct CommandDirectoryHistoryCell {
    pub(crate) inner: PlainHistoryCell,
    pub(crate) directory: CommandDirectory,
}

impl HistoryCell for CommandDirectoryHistoryCell {
    fn command_directory(&self) -> Option<&CommandDirectory> {
        Some(&self.directory)
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.display_hyperlink_lines(width))
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.directory
            .prepend_lines(width, self.inner.display_hyperlink_lines(width))
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.directory
            .prepend_lines(width, self.inner.transcript_hyperlink_lines(width))
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.transcript_hyperlink_lines(width))
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        self.inner.raw_lines()
    }
}

fn same_directory(left: &LegacyAppPathString, right: &LegacyAppPathString) -> bool {
    match (left.to_inferred_path_uri(), right.to_inferred_path_uri()) {
        (Some(left), Some(right)) => left
            .relative_path_from(&right)
            .is_some_and(|path| path.is_empty()),
        _ => left == right,
    }
}

impl ExecCell {
    pub(crate) fn with_command_directory(mut self, directory: Option<CommandDirectory>) -> Self {
        self.command_directory = directory;
        self
    }

    pub(crate) fn same_command_directory(&self, directory: Option<&CommandDirectory>) -> bool {
        match (self.command_directory.as_ref(), directory) {
            (Some(current), Some(newer)) => current.matches(newer),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        }
    }

    pub(super) fn prepend_command_directory(
        &self,
        width: u16,
        lines: Vec<HyperlinkLine>,
    ) -> Vec<HyperlinkLine> {
        if let Some(directory) = &self.command_directory {
            directory.prepend_lines(width, lines)
        } else {
            lines
        }
    }
}

#[cfg(test)]
#[path = "working_directory_tests.rs"]
mod tests;
