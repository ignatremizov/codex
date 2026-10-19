//! Live wait state retained by the listener for atomic resume/subscription handoffs.
//!
//! These notifications describe currently running interactions, independently of saved history.

use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::item_event_to_server_notification;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_protocol::protocol::TerminalWaitEvent;
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct ActiveTerminalWaits {
    turn_id: Option<String>,
    waits: BTreeMap<String, TerminalInteractionEvent>,
}

impl ActiveTerminalWaits {
    pub(crate) fn observe(&mut self, turn_id: &str, event: &EventMsg) {
        match event {
            EventMsg::TurnStarted(_) if self.turn_id.as_deref() != Some(turn_id) => {
                self.waits.clear();
                self.turn_id = Some(turn_id.to_owned());
            }
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
                if self.turn_id.as_deref() == Some(turn_id) =>
            {
                self.waits.clear();
                self.turn_id = None;
            }
            EventMsg::TerminalInteraction(interaction)
                if self.turn_id.as_deref() == Some(turn_id) =>
            {
                match &interaction.wait {
                    Some(TerminalWaitEvent::Started { interaction_id, .. }) => {
                        self.waits
                            .entry(interaction_id.clone())
                            .or_insert_with(|| interaction.clone());
                    }
                    Some(TerminalWaitEvent::Finished { interaction_id, .. })
                        if self.waits.get(interaction_id).is_some_and(|started| {
                            started.call_id == interaction.call_id
                                && started.process_id == interaction.process_id
                        }) =>
                    {
                        self.waits.remove(interaction_id);
                    }
                    Some(TerminalWaitEvent::Finished { .. }) | None => {}
                }
            }
            _ => {}
        }
    }

    pub(crate) fn snapshot(&self, thread_id: ThreadId) -> Vec<ServerNotification> {
        let Some(turn_id) = self.turn_id.as_deref() else {
            return Vec::new();
        };
        let thread_id = thread_id.to_string();
        self.waits
            .values()
            .map(|wait| {
                item_event_to_server_notification(
                    EventMsg::TerminalInteraction(wait.clone()),
                    &thread_id,
                    turn_id,
                )
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "terminal_waits_tests.rs"]
mod tests;
