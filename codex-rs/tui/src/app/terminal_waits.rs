//! Retain live wait identities across bounded event eviction and widget reconstruction.

use super::ThreadEventStore;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::TerminalWait;

impl ThreadEventStore {
    pub(super) fn track_terminal_wait(&mut self, notification: &ServerNotification) {
        let ServerNotification::TerminalInteraction(wait) = notification else {
            return;
        };
        if self.active_turn_id.as_deref() != Some(wait.turn_id.as_str()) {
            return;
        }
        match &wait.wait {
            Some(TerminalWait::Started { interaction_id, .. }) => {
                self.terminal_waits
                    .entry(interaction_id.clone())
                    .or_insert_with(|| wait.clone());
            }
            Some(TerminalWait::Finished { interaction_id, .. })
                if self
                    .terminal_waits
                    .get(interaction_id)
                    .is_some_and(|started| {
                        started.item_id == wait.item_id && started.process_id == wait.process_id
                    }) =>
            {
                self.terminal_waits.remove(interaction_id);
            }
            Some(TerminalWait::Finished { .. }) | None => {}
        }
    }
}
