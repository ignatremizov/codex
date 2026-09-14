//! Live and completed `clock.sleep` rendering for `ChatWidget`.

use super::*;
use codex_app_server_protocol::SleepItem;

impl ChatWidget {
    pub(super) fn on_sleep_started(&mut self, item: SleepItem, turn_id: String) {
        if self
            .turn_lifecycle
            .has_handled_sleep_item(turn_id.as_str(), item.id.as_str())
            || self
                .transcript
                .active_cell
                .as_ref()
                .and_then(|cell| cell.as_any().downcast_ref::<history_cell::SleepCell>())
                .is_some_and(|sleep| {
                    sleep.call_id() == item.id.as_str() && sleep.turn_id() == turn_id.as_str()
                })
        {
            return;
        }

        self.flush_answer_stream_with_separator();
        self.flush_active_cell();
        self.transcript.active_cell =
            Some(Box::new(history_cell::new_active_sleep_cell(item, turn_id)));
        self.bump_active_cell_revision();
        self.request_redraw();
    }

    pub(super) fn on_sleep_completed(&mut self, item: SleepItem, turn_id: &str) {
        if !self
            .turn_lifecycle
            .remember_sleep_item(turn_id, item.id.as_str())
        {
            return;
        }

        let completed_active_sleep = self
            .transcript
            .active_cell
            .as_ref()
            .and_then(|cell| cell.as_any().downcast_ref::<history_cell::SleepCell>())
            .is_some_and(|cell| cell.call_id() == item.id.as_str() && cell.turn_id() == turn_id);

        if completed_active_sleep {
            let _ = self.transcript.take_active_cell();
            self.bump_active_cell_revision();
        }

        self.on_async_agent_notice(history_cell::new_compact_sleep_cell(
            item,
            turn_id.to_string(),
        ));
        self.transcript.had_work_activity = true;
        self.request_redraw();
    }

    /// Compact a still-active sleep when its turn ends before the item completion arrives.
    ///
    /// The protocol item contains only the requested duration, so this records no completion
    /// outcome and does not claim that the full duration elapsed.
    pub(super) fn compact_active_sleep_for_turn_end(&mut self) {
        let (turn_id, item) = {
            let Some(sleep) = self
                .transcript
                .active_cell
                .as_ref()
                .and_then(|cell| cell.as_any().downcast_ref::<history_cell::SleepCell>())
            else {
                return;
            };
            let turn_id = sleep.turn_id().to_string();
            (turn_id, sleep.item().clone())
        };
        self.turn_lifecycle
            .remember_sleep_item(turn_id.as_str(), item.id.as_str());
        let _ = self.transcript.take_active_cell();
        self.bump_active_cell_revision();
        self.add_to_history(history_cell::new_compact_sleep_cell(item, turn_id));
        self.transcript.had_work_activity = true;
    }
}
