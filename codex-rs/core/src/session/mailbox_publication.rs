//! Quarantines an abandoned or failed mailbox publication before its writer barrier is released.

use super::Session;
use std::sync::Arc;

pub(super) struct MailboxPublicationOutcome {
    pub(super) session: Arc<Session>,
    pub(super) finished: bool,
}

impl Drop for MailboxPublicationOutcome {
    fn drop(&mut self) {
        if !self.finished {
            self.session.quarantine_history(
                "mailbox publication outcome unknown; recover canonical evidence before continuing"
                    .to_string(),
            );
        }
    }
}
