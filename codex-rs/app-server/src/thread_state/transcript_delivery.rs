//! Cancellation-owned suppression for a resuming connection only.

use super::ThreadStateManager;
use crate::outgoing_message::ConnectionId;
use codex_protocol::ThreadId;
use std::sync::Arc;
use tracing::error;

/// Dropping the last owner immediately ends this pause, including on cancellation.
/// Weak registrations never require an asynchronous cleanup task.
/// Events suppressed for this connection remain in canonical history. A cancelled
/// or failed resume requires another successful resume before treating its delta
/// stream as complete; this lease is not a replay buffer or delivery receipt.
pub(crate) struct TranscriptResumePause {
    _lease: Arc<()>,
}

impl ThreadStateManager {
    pub(crate) async fn pause_typed_transcript_for_resume(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
    ) -> Option<TranscriptResumePause> {
        let delivery_gate = {
            let mut state = self.state.lock().await;
            if !state.live_connections.contains_key(&connection_id) {
                return None;
            }
            Arc::clone(
                &state
                    .threads
                    .entry(thread_id)
                    .or_default()
                    .typed_transcript_delivery_gate,
            )
        };
        let Ok(_delivery_permit) = delivery_gate.acquire_owned().await else {
            error!("typed transcript delivery semaphore closed unexpectedly");
            return None;
        };
        let mut state = self.state.lock().await;
        if !state.live_connections.contains_key(&connection_id) {
            return None;
        }
        let pauses = &mut state
            .threads
            .entry(thread_id)
            .or_default()
            .paused_typed_transcript_connections;
        pauses.retain(|_, leases| {
            leases.retain(|lease| lease.strong_count() != 0);
            !leases.is_empty()
        });
        let leases = pauses.entry(connection_id).or_default();
        let lease = Arc::new(());
        leases.push(Arc::downgrade(&lease));
        Some(TranscriptResumePause { _lease: lease })
    }

    pub(crate) async fn typed_transcript_connection_ids(
        &self,
        thread_id: ThreadId,
    ) -> Vec<ConnectionId> {
        let state = self.state.lock().await;
        state
            .threads
            .get(&thread_id)
            .map(|thread_entry| {
                thread_entry
                    .connection_ids
                    .iter()
                    .filter(|connection_id| {
                        !thread_entry
                            .paused_typed_transcript_connections
                            .get(connection_id)
                            .is_some_and(|leases| {
                                leases.iter().any(|lease| lease.strong_count() != 0)
                            })
                    })
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) async fn acquire_typed_transcript_delivery_permit(
        &self,
        thread_id: ThreadId,
    ) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let delivery_gate = {
            let mut state = self.state.lock().await;
            Arc::clone(
                &state
                    .threads
                    .entry(thread_id)
                    .or_default()
                    .typed_transcript_delivery_gate,
            )
        };
        match delivery_gate.acquire_owned().await {
            Ok(permit) => Some(permit),
            Err(_) => {
                error!("typed transcript delivery semaphore closed unexpectedly");
                None
            }
        }
    }
}
