//! A bounded snapshot/subscription handoff. Failure requires a fresh canonical resume.

use crate::error_code::internal_error;
use crate::outgoing_message::ConnectionId;
use crate::thread_state::ThreadListenerCommand;
use crate::thread_state::ThreadState;
use crate::thread_state::ThreadStateManager;
use crate::thread_state::TranscriptResumePause;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::CodexThread;
use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Event;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub(super) struct ResumeBoundary {
    pub(super) listener_command_tx: mpsc::UnboundedSender<ThreadListenerCommand>,
    pub(super) listener_generation: u64,
    pub(super) publication_permit: OwnedSemaphorePermit,
    pub(super) transcript_pause: TranscriptResumePause,
    pub(super) release_tx: oneshot::Sender<()>,
}

pub(super) enum ResumeDrainOutcome {
    Complete,
    Interrupted,
}

/// Only the owning listener may dequeue. Recheck ownership between every event.
/// The one already-dequeued handler always settles its state and notification
/// effects, matching ordinary listener delivery during replacement or shutdown.
pub(super) async fn drain_events_before_resume<F: Future<Output = ()>>(
    pending_event_count: usize,
    listener_generation: u64,
    thread_state: &Mutex<ThreadState>,
    cancel_rx: &mut oneshot::Receiver<()>,
    mut next_event: impl FnMut() -> CodexResult<Option<Event>>,
    mut process_event: impl FnMut(Event) -> F,
) -> ResumeDrainOutcome {
    for _ in 0..pending_event_count {
        let generation = tokio::select! {
            biased;
            _ = &mut *cancel_rx => return ResumeDrainOutcome::Interrupted,
            state = thread_state.lock() => state.listener_generation,
        };
        if generation != listener_generation {
            return ResumeDrainOutcome::Interrupted;
        }
        let event = match next_event() {
            Ok(Some(event)) => event,
            Ok(None) => return ResumeDrainOutcome::Interrupted,
            Err(error) => {
                tracing::warn!("failed to drain thread events before resume: {error}");
                return ResumeDrainOutcome::Interrupted;
            }
        };
        process_event(event).await;
    }
    let generation = tokio::select! {
        biased;
        _ = &mut *cancel_rx => return ResumeDrainOutcome::Interrupted,
        state = thread_state.lock() => state.listener_generation,
    };
    if generation == listener_generation {
        ResumeDrainOutcome::Complete
    } else {
        ResumeDrainOutcome::Interrupted
    }
}

pub(super) fn resume_retry_error(mut error: JSONRPCErrorError) -> JSONRPCErrorError {
    error
        .message
        .push_str("; retry thread/resume to recover the complete canonical history");
    error
}

#[cfg(test)]
#[path = "thread_resume_boundary_tests.rs"]
mod tests;

/// Core's unbounded event sender never waits for this listener. Take publication
/// first, then briefly synchronize with typed delivery to register the pause.
/// Dropping this preparation releases both owners and the listener handshake.
pub(super) async fn begin_running_resume(
    thread_id: ThreadId,
    connection_id: ConnectionId,
    conversation: &CodexThread,
    manager: &ThreadStateManager,
    thread_state: &Arc<Mutex<ThreadState>>,
) -> Result<ResumeBoundary, JSONRPCErrorError> {
    async {
        let (listener_command_tx, listener_generation) = {
            let thread_state = thread_state.lock().await;
            (thread_state.listener_command_tx(), thread_state.listener_generation)
        };
        let Some(listener_command_tx) = listener_command_tx else {
            return Err(internal_error(format!(
                "failed to enqueue running thread resume for thread {thread_id}: thread listener is not running"
            )));
        };
        let publication_permit = match conversation
            .acquire_history_publication_barrier()
            .await
        {
            Ok(permit) => permit,
            Err(err) => {
                return Err(internal_error(format!(
                    "failed to establish running thread resume boundary for thread {thread_id}: {err}"
                )));
            }
        };
        let Some(transcript_pause) = manager
            .pause_typed_transcript_for_resume(thread_id, connection_id)
            .await
        else {
            tracing::debug!(
                thread_id = %thread_id,
                connection_id = ?connection_id,
                "skipping running thread resume for closed connection"
            );
            return Err(internal_error("connection closed during thread/resume"));
        };
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        let (barrier_release_tx, barrier_release_rx) = tokio::sync::oneshot::channel();
        if listener_command_tx
            .send(
                crate::thread_state::ThreadListenerCommand::DrainPendingEventsForResume {
                    listener_generation,
                    completion_tx,
                    release_rx: barrier_release_rx,
                },
            )
            .is_err()
        {
            return Err(internal_error(format!(
                "failed to establish running thread resume boundary for thread {thread_id}: thread listener command channel is closed"
            )));
        }
        if !matches!(
            tokio::time::timeout(Duration::from_secs(10), completion_rx).await,
            Ok(Ok(true))
        ) {
            return Err(internal_error(format!(
                "failed to drain running thread events before resuming thread {thread_id}"
            )));
        }

        Ok(ResumeBoundary {
            listener_command_tx,
            listener_generation,
            publication_permit,
            transcript_pause,
            release_tx: barrier_release_tx,
        })
    }.await.map_err(resume_retry_error)
}
