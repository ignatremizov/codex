use super::session::Session;
use tokio::sync::watch;

impl Session {
    /// Subscribes to newly committed passive finals without replaying older deliveries.
    pub(crate) fn subscribe_passive_final_delivery_activity(&self) -> watch::Receiver<()> {
        self.passive_final_delivery_activity.subscribe()
    }

    /// Wakes active `clock.sleep` handlers without admitting an idle turn.
    pub(crate) fn notify_passive_final_delivery_activity(&self) {
        self.passive_final_delivery_activity.send_replace(());
    }
}
