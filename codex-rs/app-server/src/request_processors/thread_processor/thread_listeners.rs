//! Parent-scoped observation delegates attachment with the exact created runtime.

use super::ThreadRequestProcessor;
use crate::outgoing_message::ConnectionId;
use crate::thread_state::ThreadStateManager;
use codex_protocol::ThreadId;

async fn inherited_connection_ids(
    manager: &ThreadStateManager,
    parent_thread_id: Option<ThreadId>,
) -> Vec<ConnectionId> {
    match parent_thread_id {
        Some(parent_thread_id) => manager.subscribed_connection_ids(parent_thread_id).await,
        None => Vec::new(),
    }
}

impl ThreadRequestProcessor {
    /// Observe creation without granting unrelated clients a subscription.
    pub(crate) async fn try_attach_created_thread_listener(&self, thread_id: ThreadId) {
        let Ok(thread) = self.thread_manager.get_thread(thread_id).await else {
            return;
        };
        let parent_thread_id = thread.config_snapshot().await.parent_thread_id;
        let connection_ids =
            inherited_connection_ids(&self.thread_state_manager, parent_thread_id).await;
        // Root RPCs subscribe their requesting connection explicitly. Spawned children inherit
        // only the parent's viewers, including when other TUIs share this daemon.
        self.try_attach_thread_listener(thread_id, thread, connection_ids)
            .await;
    }
}

#[cfg(test)]
#[path = "thread_listeners_tests.rs"]
mod tests;
