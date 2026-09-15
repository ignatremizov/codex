//! Live and completed `clock.sleep` rendering for `ChatWidget`.

use super::*;
use codex_app_server_protocol::SleepItem;

#[derive(Debug, Clone)]
pub(super) struct ActiveSleep {
    pub(super) turn_id: String,
    pub(super) item: SleepItem,
}

impl ChatWidget {
    pub(super) fn on_sleep_started(
        &mut self,
        item: SleepItem,
        turn_id: String,
        started_at_ms: i64,
        from_replay: bool,
    ) {
        if from_replay {
            return;
        }
        if self
            .turn_lifecycle
            .has_handled_sleep_item(turn_id.as_str(), item.id.as_str())
            || self
                .turn_lifecycle
                .active_sleep
                .as_ref()
                .is_some_and(|sleep| sleep.item.id == item.id && sleep.turn_id == turn_id)
        {
            return;
        }

        if self.turn_lifecycle.active_sleep.is_some() {
            self.compact_active_sleep(/*defer_to_stream*/ true);
        }
        let item_id = item.id.clone();
        let duration_ms = i64::try_from(item.duration_ms).unwrap_or(i64::MAX);
        self.turn_lifecycle.active_sleep = Some(ActiveSleep {
            turn_id: turn_id.clone(),
            item,
        });
        self.bottom_pane.ensure_status_indicator();
        self.bottom_pane
            .set_interrupt_hint_visible(/*visible*/ true);
        self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Working;
        self.set_status(
            "Sleeping".to_string(),
            /*details*/ None,
            StatusDetailsCapitalization::Preserve,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        );
        let owner = StatusCountdownOwner::Sleep { turn_id, item_id };
        self.set_status_countdown_deadline_at_ms(owner, started_at_ms.saturating_add(duration_ms));
        self.request_redraw();
    }

    pub(super) fn on_sleep_completed(&mut self, item: SleepItem, turn_id: &str, from_replay: bool) {
        if !self
            .turn_lifecycle
            .remember_sleep_item(turn_id, item.id.as_str())
        {
            return;
        }

        let completed_active_sleep = self
            .turn_lifecycle
            .active_sleep
            .as_ref()
            .is_some_and(|sleep| sleep.turn_id == turn_id && sleep.item.id == item.id);
        if completed_active_sleep {
            self.turn_lifecycle.active_sleep = None;
            let owner = StatusCountdownOwner::Sleep {
                turn_id: turn_id.to_string(),
                item_id: item.id.clone(),
            };
            let owns_status = self.status_state.countdown_owner.as_ref() == Some(&owner);
            if owns_status {
                self.clear_status_countdown();
                if self.status_state.current_status.header == "Sleeping" {
                    self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Working;
                    self.set_status_header("Working".to_string());
                }
            }
        }

        let cell = history_cell::new_compact_sleep_cell(item);
        if from_replay {
            self.add_to_history(cell);
        } else {
            self.on_async_agent_notice(cell);
        }
        self.transcript.had_work_activity = true;
        self.request_redraw();
    }

    /// Compact a still-active sleep when its turn ends before the item completion arrives.
    ///
    /// The protocol item contains only the requested duration, so this records no completion
    /// outcome and does not claim that the full duration elapsed.
    pub(super) fn compact_active_sleep(&mut self, defer_to_stream: bool) {
        let Some(active_sleep) = self.turn_lifecycle.active_sleep.take() else {
            return;
        };
        let ActiveSleep { turn_id, item } = active_sleep;
        self.turn_lifecycle
            .remember_sleep_item(turn_id.as_str(), item.id.as_str());
        let owner = StatusCountdownOwner::Sleep {
            turn_id: turn_id.clone(),
            item_id: item.id.clone(),
        };
        let owns_status = self.status_state.countdown_owner.as_ref() == Some(&owner);
        if owns_status {
            self.clear_status_countdown();
            if self.status_state.current_status.header == "Sleeping" {
                self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Working;
                self.set_status_header("Working".to_string());
            }
        }
        let cell = history_cell::new_compact_sleep_cell(item);
        if defer_to_stream {
            self.on_async_agent_notice(cell);
        } else {
            self.add_to_history(cell);
        }
        self.transcript.had_work_activity = true;
    }
}
