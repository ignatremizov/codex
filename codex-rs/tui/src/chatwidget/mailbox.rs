//! One-shot user mailbox submission. Acceptance is not turn admission or model consumption.

use super::*;
use codex_app_server_protocol::ThreadMailboxAddResponse;
use codex_app_server_protocol::ThreadMailboxMessageState;

/// Frozen typed input and its original rich draft, retained across ambiguous acceptance retries.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MailboxSubmission {
    pub(crate) thread_id: ThreadId,
    pub(crate) user_message: UserMessage,
    pub(crate) input: Vec<UserInput>,
    pub(crate) client_user_message_id: String,
}

impl MailboxSubmission {
    pub(crate) fn restored_message(&self) -> UserMessage {
        let mut message = self.user_message.clone();
        message.text.insert_str(0, "/mail ");
        for element in &mut message.text_elements {
            element.byte_range.start += "/mail ".len();
            element.byte_range.end += "/mail ".len();
        }
        message
    }
}

impl ChatWidget {
    pub(super) fn submit_mailbox_command(&mut self, args: String, text_elements: Vec<TextElement>) {
        let Some(thread_id) = self.thread_id else {
            self.add_error_message("Session is still starting; retry /mail when ready.".into());
            return;
        };
        let prepared = if self.bottom_pane.composer_text().is_empty() {
            Some((args, text_elements))
        } else {
            self.bottom_pane
                .prepare_inline_args_submission(/*record_history*/ false)
        };
        let Some((text, text_elements)) = prepared else {
            return;
        };
        let user_message = self.user_message_from_submission(text, text_elements);
        let input = self.user_inputs_from_message(&user_message);
        if input.is_empty() {
            self.add_info_message(
                "Usage: /mail <message> (attachments supported). Mail does not steer or queue work."
                    .into(),
                Some("An idle agent may wake with an inbox inventory; check_mail selects the payload.".into()),
            );
            return;
        }
        let submission = self
            .input_queue
            .mailbox_retry
            .as_ref()
            .filter(|retry| retry.thread_id == thread_id && retry.user_message == user_message)
            .cloned()
            .unwrap_or_else(|| MailboxSubmission {
                thread_id,
                user_message,
                input,
                client_user_message_id: uuid::Uuid::now_v7().to_string(),
            });
        self.input_queue.mailbox_retry = Some(submission.clone());
        self.app_event_tx
            .send(AppEvent::SubmitUserMailbox(submission));
        self.request_redraw();
    }

    pub(crate) fn finish_mailbox_submission(
        &mut self,
        submission: MailboxSubmission,
        result: Result<ThreadMailboxAddResponse, String>,
    ) {
        match result {
            Ok(response) => {
                self.input_queue.mailbox_retry = None;
                let status = match response.state {
                    ThreadMailboxMessageState::Pending => {
                        "Mailbox: accepted · unread; the agent selects when to consume.".to_string()
                    }
                    ThreadMailboxMessageState::Claimed => {
                        "Mailbox: claimed for delivery · consumption not yet confirmed.".to_string()
                    }
                    ThreadMailboxMessageState::Consumed => {
                        "Mailbox: already consumed · not submitted again.".to_string()
                    }
                    ThreadMailboxMessageState::Rejected => format!(
                        "Mailbox: rejected · {}",
                        response
                            .rejection_reason
                            .as_deref()
                            .unwrap_or("delivery was rejected")
                    ),
                };
                if response.state == ThreadMailboxMessageState::Rejected {
                    self.restore_user_message_to_composer(submission.restored_message());
                }
                self.add_to_history(
                    crate::history_cell::mailbox::MailboxAcceptanceHistoryCell::new(
                        submission.user_message,
                        status,
                    ),
                );
            }
            Err(error) => {
                self.restore_user_message_to_composer(submission.restored_message());
                self.input_queue.mailbox_retry = Some(submission);
                self.add_error_message(format!(
                    "Mailbox acceptance was not confirmed: {error}. Your /mail draft was restored; retry unchanged to reuse its submission ID."
                ));
            }
        }
        self.request_redraw();
    }
}

impl ThreadInputState {
    /// Restore to the original thread if focus changed before its submit event was handled.
    pub(crate) fn restore_mailbox_submission(&mut self, submission: MailboxSubmission) {
        let existing = self.composer.take().unwrap_or_default();
        let message = merge_user_messages(vec![
            submission.restored_message(),
            UserMessage {
                text: existing.text,
                local_images: existing.local_images,
                remote_image_urls: existing.remote_image_urls,
                text_elements: existing.text_elements,
                mention_bindings: existing.mention_bindings,
            },
        ]);
        self.composer = Some(ThreadComposerState {
            text: message.text,
            local_images: message.local_images,
            remote_image_urls: message.remote_image_urls,
            text_elements: message.text_elements,
            mention_bindings: message.mention_bindings,
            pending_pastes: existing.pending_pastes,
        });
        self.mailbox_retry = Some(submission);
    }
}

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
