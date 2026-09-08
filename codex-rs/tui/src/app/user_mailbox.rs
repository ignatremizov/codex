//! Typed user-mail acceptance, separate from queued prompts and active-turn steering.

use super::*;
use crate::chatwidget::mailbox::MailboxSubmission;

impl App {
    pub(super) async fn submit_user_mailbox(
        &mut self,
        app_server: &mut AppServerSession,
        submission: MailboxSubmission,
    ) {
        if self.chat_widget.thread_id() != Some(submission.thread_id) {
            if let Some(channel) = self.thread_event_channels.get(&submission.thread_id)
                && let Some(input_state) = channel.store.lock().await.input_state.as_mut()
            {
                input_state.restore_mailbox_submission(submission);
            }
            self.chat_widget.add_error_message(
                "Focus changed before mailbox submission; return to the original thread to retry."
                    .into(),
            );
            return;
        }
        let result = app_server
            .thread_mailbox_add(
                submission.thread_id,
                submission.input.clone(),
                submission.client_user_message_id.clone(),
            )
            .await
            .map_err(|error| error.to_string());
        self.chat_widget
            .finish_mailbox_submission(submission, result);
    }
}
