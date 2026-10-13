//! Preserve the final conversation view while restoring the primary terminal for the shell.

use super::Tui;
use std::io;

impl Tui {
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if !self.is_owned_screen() || !self.is_alt_screen_active() {
            // Inline history is already in terminal scrollback; clear only its live composer.
            return self.terminal.clear();
        }

        let frame = self.terminal.previous_buffer().clone();
        self.set_owned_screen(/*owned*/ false)?;
        self.terminal.append_frame_to_scrollback(&frame)
    }
}

#[cfg(all(test, unix))]
#[path = "exit_tests.rs"]
mod tests;
