//! Display-only local storage location from the resolved TUI configuration.

use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;
use ratatui::style::Stylize;
use ratatui::text::Line;

#[derive(Debug, Clone)]
pub(crate) struct StatusStorageDisplay {
    pub(crate) local_codex_home: String,
}

impl StatusStorageDisplay {
    pub(crate) fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let indent = if width > 2 { "  " } else { "" };
        word_wrap_lines(
            [Line::from(vec![
                "Codex home (local TUI): ".dim(),
                self.local_codex_home.clone().into(),
            ])],
            RtOptions::new(width).subsequent_indent(indent.dim().into()),
        )
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
