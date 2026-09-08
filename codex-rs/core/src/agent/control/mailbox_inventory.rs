use super::AgentControl;
use codex_extension_api::ThreadIdleCause;
use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use tokio::sync::OwnedMutexGuard;

impl AgentControl {
    /// Called after durable acceptance and after releasing admission/permission locks.
    /// Only loaded receivers are signalled; resume reads the durable inventory.
    pub(crate) async fn notify_mailbox_activity(&self, receiver: ThreadId) {
        let Ok(manager) = self.upgrade() else {
            return;
        };
        let Ok(thread) = manager.get_thread(receiver).await else {
            return;
        };
        thread.session.notify_mailbox_activity();
        // Await existing idle arbitration instead of spawning a task per acceptance.
        thread
            .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
            .await;
    }

    /// Serializes the inventory idle reservation with queued agent admission.
    pub(crate) async fn mailbox_inventory_lifecycle_lease(
        &self,
        receiver: ThreadId,
    ) -> CodexResult<Option<OwnedMutexGuard<()>>> {
        let manager = self.upgrade()?;
        let guard = manager.agent_lifecycle_lock(receiver).lock_owned().await;
        if manager.agent_turn_queue.has_pending(receiver) {
            return Ok(None);
        }
        Ok(Some(guard))
    }
}
