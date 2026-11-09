//! Preserve command directories across live events and completion-only replay.

use super::*;
use crate::exec_cell::CommandDirectory;
use crate::exec_cell::CommandDirectoryDisplay;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;

impl ChatWidget {
    pub(super) fn command_directory_for(
        &self,
        cwd: LegacyAppPathString,
        source: ExecCommandSource,
    ) -> Option<CommandDirectory> {
        // Writing to an existing process does not establish a new cwd. Inherit
        // the established directory so interactions can stay in the active group.
        let cwd = if source == ExecCommandSource::UnifiedExecInteraction {
            self.transcript
                .last_command_cwd
                .clone()
                .unwrap_or_else(|| self.config.cwd.clone().into())
        } else {
            cwd
        };
        if cwd.as_str().is_empty() {
            return None;
        }
        Some(CommandDirectory::new(
            cwd,
            PathUri::from_abs_path(&self.config.cwd),
            self.transcript.last_command_cwd.as_ref(),
        ))
    }

    pub(super) fn observe_command_directory(
        &mut self,
        cwd: LegacyAppPathString,
        source: ExecCommandSource,
    ) -> Option<CommandDirectory> {
        let directory = self.command_directory_for(cwd, source)?;
        self.transcript.last_command_cwd = Some(directory.cwd.clone());
        Some(directory)
    }

    /// The overlay chooses a path presentation without changing the ordinary conversation.
    pub(crate) fn active_cell_lines_with_command_paths(
        &self,
        width: u16,
        detailed: bool,
        directory_display: CommandDirectoryDisplay,
    ) -> Option<Vec<HyperlinkLine>> {
        self.active_cell_hyperlink_lines_with(width, |cell, width| {
            let lines = if self.history_render_mode() == HistoryRenderMode::Raw {
                cell.display_hyperlink_lines_for_mode(width, HistoryRenderMode::Raw)
            } else if detailed {
                cell.transcript_hyperlink_lines(width)
            } else {
                cell.display_hyperlink_lines(width)
            };
            if self.history_render_mode() == HistoryRenderMode::Rich
                && let Some(directory) = cell.command_directory()
            {
                directory.replace_lines(width, directory_display, lines)
            } else {
                lines
            }
        })
    }
}
