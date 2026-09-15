//! Session-local hints emitted by acknowledged passive-final publication workers.

use super::session::Session;
use tokio::sync::watch;

impl Session {
    /// Subscribes to newly committed passive finals without replaying older deliveries.
    pub(crate) fn subscribe_passive_final_delivery_activity(&self) -> watch::Receiver<()> {
        self.passive_final_delivery_activity.subscribe()
    }
}
