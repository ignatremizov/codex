//! Bind committed receipts to optimistic prompts without replacing selected cells.

use super::*;
use crate::history_cell::UserMessageIdentity;
use codex_app_server_protocol::UserInput;

impl App {
    pub(crate) fn attach_user_message_identity(
        &mut self,
        thread_id: ThreadId,
        identity: UserMessageIdentity,
        client_id: Option<&str>,
        content: &[UserInput],
    ) {
        if self.chat_widget.thread_id() != Some(thread_id) {
            return;
        }
        attach_identity(&self.transcript_cells, identity, client_id, content);
    }
}

fn attach_identity(
    cells: &[Arc<dyn crate::history_cell::HistoryCell>],
    identity: UserMessageIdentity,
    client_id: Option<&str>,
    content: &[UserInput],
) {
    let display = ChatWidget::user_message_display_from_inputs(content);
    let mut candidates = cells.iter().filter_map(|cell| {
        let cell = cell.as_any().downcast_ref::<UserHistoryCell>()?;
        if cell.identity.get().is_some() {
            return None;
        }
        let matches = match client_id {
            Some(client_id) => cell.client_id.as_deref() == Some(client_id),
            None => {
                cell.message == display.message
                    && cell.text_elements == display.text_elements
                    && cell.local_image_paths == display.local_images
                    && cell.remote_image_urls == display.remote_image_urls
            }
        };
        matches.then_some(cell)
    });
    if let Some(cell) = candidates.next()
        && candidates.next().is_none()
    {
        let _ = cell.identity.set(identity);
    }
}

#[cfg(test)]
#[path = "user_identity_tests.rs"]
mod tests;
