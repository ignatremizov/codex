//! Directory presentation is local to a transcript view, including retained history pages.

use super::*;
use crate::exec_cell::CommandDirectoryDisplay;

impl TranscriptView {
    pub(crate) fn command_directory_display(&self) -> CommandDirectoryDisplay {
        if self.full_command_paths {
            CommandDirectoryDisplay::Full
        } else {
            CommandDirectoryDisplay::Changes
        }
    }

    pub(crate) fn toggle_command_paths(&mut self) {
        self.full_command_paths = !self.full_command_paths;
        self.set_review_presentation(self.detailed, self.mode);
    }

    pub(super) fn command_directory_presentation(
        &self,
        cell: &dyn HistoryCell,
        cells: &[Arc<dyn HistoryCell>],
        index: usize,
    ) -> CommandDirectoryDisplay {
        if self.mode == HistoryRenderMode::Raw {
            return CommandDirectoryDisplay::Hidden;
        }
        if self.full_command_paths {
            return CommandDirectoryDisplay::Full;
        }
        // Recompute the boundary after prepending pages instead of trusting each page's
        // initial cwd. Intervening prose does not change a command's assumed directory.
        crate::exec_cell::command_directory_display_after(cell, &cells[..index])
    }
}

#[cfg(test)]
#[path = "command_paths_tests.rs"]
mod tests;
