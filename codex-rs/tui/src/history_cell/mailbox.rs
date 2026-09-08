//! Local acceptance presentation, deliberately not a committed/backtrackable user-history item.

use super::*;
use crate::chatwidget::UserMessage;

#[derive(Debug)]
pub(crate) struct MailboxAcceptanceHistoryCell {
    input: UserHistoryCell,
    status: String,
}

impl MailboxAcceptanceHistoryCell {
    pub(crate) fn new(message: UserMessage, status: String) -> Self {
        Self {
            input: new_user_prompt(
                message.text,
                message.text_elements,
                message
                    .local_images
                    .into_iter()
                    .map(|image| image.path)
                    .collect(),
                message.remote_image_urls,
            ),
            status,
        }
    }
}

impl HistoryCell for MailboxAcceptanceHistoryCell {
    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = self.input.raw_lines();
        lines.push(self.status.clone().into());
        lines
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = self.input.display_lines(width);
        lines.extend(crate::wrapping::word_wrap_lines(
            [Line::from(self.status.clone().dim())],
            crate::wrapping::RtOptions::new(width.max(1) as usize),
        ));
        lines
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = self.input.transcript_lines(width);
        lines.extend(crate::wrapping::word_wrap_lines(
            [Line::from(self.status.clone().dim())],
            crate::wrapping::RtOptions::new(width.max(1) as usize),
        ));
        lines
    }
}
