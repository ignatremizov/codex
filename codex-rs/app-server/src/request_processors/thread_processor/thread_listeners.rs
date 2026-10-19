//! Explicit client attachment and parent-scoped observation of newly loaded threads.

use super::ThreadRequestProcessor;
use super::log_listener_attach_result;
use crate::outgoing_message::ConnectionId;
use crate::thread_state::ThreadStateManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::MultiAgentVersion;
use tracing::warn;

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
        self.try_attach_thread_listener(thread_id, connection_ids)
            .await;
    }

    /// Best-effort attachment for explicit client requests and inherited parent viewers.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "listener state creation must be serialized against pending unloads"
    )]
    pub(crate) async fn try_attach_thread_listener(
        &self,
        thread_id: ThreadId,
        connection_ids: Vec<ConnectionId>,
    ) {
        if connection_ids.is_empty() {
            let result = loop {
                let pending = self.pending_thread_unloads.lock().await;
                if let Some(unload) = pending.get(&thread_id) {
                    // A replacement can be published before the old listener finishes cleanup.
                    let mut completion = unload.subscribe();
                    drop(pending);
                    let _ = completion.changed().await;
                    continue;
                }
                let Ok(thread) = self.thread_manager.get_thread(thread_id).await else {
                    return;
                };
                // Only persisted V2 children can reload after idle eviction.
                if thread.multi_agent_version() != Some(MultiAgentVersion::V2)
                    || thread.config().await.ephemeral
                {
                    return;
                }
                self.thread_watch_manager
                    .upsert_thread(&thread_id.to_string())
                    .await;
                let thread_state = self.thread_state_manager.thread_state(thread_id).await;
                let result = self
                    .ensure_listener_task_running(thread_id, thread, thread_state)
                    .await;
                drop(pending);
                break result;
            };
            if let Err(err) = result {
                warn!(
                    "failed to start listener for thread {thread_id}: {message}",
                    message = err.message
                );
            }
            return;
        }

        let mut raw_events_enabled = false;
        if let Ok(thread) = self.thread_manager.get_thread(thread_id).await {
            let config_snapshot = thread.config_snapshot().await;
            if let Some(parent_thread_id) = config_snapshot.parent_thread_id {
                self.thread_watch_manager
                    .upsert_thread(&thread_id.to_string())
                    .await;
                raw_events_enabled = self
                    .thread_state_manager
                    .thread_state(parent_thread_id)
                    .await
                    .lock()
                    .await
                    .experimental_raw_events;
            } else {
                // Root creation RPCs introduce the thread with their response and
                // ThreadStarted notification. This concurrent observer must not
                // announce an initial Idle status before that boundary.
                self.thread_watch_manager
                    .upsert_thread_silently(&thread_id.to_string())
                    .await;
            }
        }

        for connection_id in connection_ids {
            log_listener_attach_result(
                self.ensure_conversation_listener(thread_id, connection_id, raw_events_enabled)
                    .await,
                thread_id,
                connection_id,
                "thread",
            );
        }
    }
}

#[cfg(test)]
#[path = "thread_listeners_tests.rs"]
mod tests;
