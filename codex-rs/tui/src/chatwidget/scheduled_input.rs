use super::*;
use crate::scheduled_input::ScheduleAction;
use crate::scheduled_input::parse_schedule_action;

impl ChatWidget {
    pub(super) fn reject_blocked_schedule_command(&mut self, args: &str) -> bool {
        matches!(
            parse_schedule_action(args),
            Ok(ScheduleAction::Create { .. } | ScheduleAction::Cancel(_))
        ) && self.reject_blocked_input()
    }

    pub(super) fn submit_schedule_command(&mut self, message: UserMessage) {
        let result = if !message.local_images.is_empty() || !message.remote_image_urls.is_empty() {
            Err("Scheduled prompts support text only.".to_string())
        } else {
            parse_schedule_action(&message.text)
        };
        if matches!(
            &result,
            Ok(ScheduleAction::Create { .. } | ScheduleAction::Cancel(_))
        ) && self.reject_blocked_input()
        {
            self.restore_schedule_message(message);
            return;
        }
        match (self.thread_id, result) {
            (Some(thread_id), Ok(action)) => {
                self.app_event_tx.send(AppEvent::ScheduleInput {
                    thread_id,
                    backend_id: self.question_backend_id,
                    action,
                });
            }
            (_, result) => {
                let error = result.err().unwrap_or_else(|| {
                    "Session is still starting; retry /schedule when ready.".into()
                });
                self.restore_schedule_message(message);
                self.add_error_message(error);
            }
        }
    }

    fn restore_schedule_message(&mut self, mut message: UserMessage) {
        message.text.insert_str(0, "/schedule ");
        for element in &mut message.text_elements {
            element.byte_range.start += "/schedule ".len();
            element.byte_range.end += "/schedule ".len();
        }
        self.restore_user_message_to_composer(message);
    }

    pub(crate) fn set_scheduled_input_preview(&mut self, items: Vec<String>) {
        self.bottom_pane.set_scheduled_input_preview(items);
    }
}
