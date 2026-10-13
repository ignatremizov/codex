//! Retain the last fullscreen frame in primary-screen scrollback before the shell resumes.

use super::Terminal;
use super::diff_buffers;
use super::draw;
use crate::insert_history::ResetScrollRegion;
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Print;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellWidth;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use std::io;
use std::io::Write;

impl<B> Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    /// Append an already-rendered frame below the restored shell history, leaving a fresh line
    /// for exit messages. Reuse cell rendering to preserve styles, hyperlinks and wide glyphs.
    pub(crate) fn append_frame_to_scrollback(&mut self, frame: &Buffer) -> io::Result<()> {
        let size = self.size()?;
        if frame.area.is_empty() || size.width == 0 || size.height == 0 {
            return self.clear();
        }

        let width = frame.area.width.min(size.width);
        let mut row = self.viewport_area.top().min(size.height - 1);
        queue!(self.backend_mut(), ResetScrollRegion)?;
        self.clear_after_position(Position::new(/*x*/ 0, row))?;
        for source_row in frame.area.top()..frame.area.bottom() {
            let mut line = Buffer::empty(Rect::new(/*x*/ 0, row, width, /*height*/ 1));
            let mut column = 0;
            while column < width {
                let cell = &frame[(frame.area.x + column, source_row)];
                let cell_width = cell.cell_width().max(/*other*/ 1);
                // A resize just before exit must not wrap half of a wide glyph onto another row.
                if cell_width > width - column {
                    break;
                }
                line[(column, row)] = cell.clone();
                column += cell_width;
            }
            let blank = Buffer::empty(line.area);
            draw(self.backend_mut(), diff_buffers(&blank, &line).into_iter())?;
            // Explicit positioning cancels pending autowrap after a full-width row. Full-screen
            // newlines retain scrollback on terminals that discard rows from partial regions.
            queue!(self.backend_mut(), MoveTo(/*x*/ 0, row), Print("\r\n"))?;
            row = row.saturating_add(/*rhs*/ 1).min(size.height - 1);
        }
        // Later cleanup must not clear the retained frame or replay it a second time.
        self.set_viewport_area(Rect::new(/*x*/ 0, row, size.width, /*height*/ 0));
        self.set_cursor_position(Position::new(/*x*/ 0, row))?;
        Backend::flush(self.backend_mut())
    }
}

#[cfg(test)]
#[path = "exit_tests.rs"]
mod tests;
