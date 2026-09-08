//! Receiver-local change hints. Durable mailbox storage remains authoritative.

use super::session::Session;
use tokio::sync::watch;

#[cfg(test)]
#[path = "mailbox_activity_tests.rs"]
mod tests;

impl Session {
    /// Subscribe before reading durable mail, then re-read after every change.
    pub(crate) fn subscribe_mailbox_activity(&self) -> watch::Receiver<()> {
        self.mailbox_activity.subscribe()
    }

    /// Publishes no payload and does not grant permission or request a turn.
    pub(crate) fn notify_mailbox_activity(&self) {
        self.mailbox_activity.send_replace(());
    }
}
