//! Live-only presentation bypasses model context, persistence and response observers.

use super::Session;
use codex_protocol::protocol::Event;

impl Session {
    pub(crate) async fn deliver_agent_audit_event(&self, event: Event) {
        if let Err(error) = self.tx_event.send(event).await {
            tracing::debug!(%error, "agent audit event stream is closed");
        }
    }
}
