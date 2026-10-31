//! Actor-ordered idle reservation for explicit ownership changes, cancelled by its caller.

use super::SessionIo;
use super::command_approval::QueuedSubmission;
use super::session::Session;
use async_channel::Receiver;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Op;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;
use uuid::Uuid;

impl SessionIo {
    /// Keep the returned sender until shutdown has accepted ownership of this actor.
    pub(crate) async fn prepare_ownership_handoff(&self) -> CodexResult<oneshot::Sender<()>> {
        let (reply, result) = oneshot::channel();
        let (reservation, release) = oneshot::channel();
        let (submit_finished, submitted) = oneshot::channel();
        self.submit(Op::PrepareOwnershipHandoff {
            submitted,
            reply,
            release,
        })
        .await?;
        // The actor must not mistake this operation's own enqueue lock for another
        // producer admitting work. Cancellation drops both cancellation channels.
        let _ = submit_finished.send(());
        tokio::select! {
            result = result => result.map_err(|_| CodexErr::InternalAgentDied)??,
            () = self.session_loop_termination.clone() => {
                return Err(CodexErr::InternalAgentDied);
            }
        }
        Ok(reservation)
    }
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "idle checks and the seal must share public submission order"
)]
pub(super) async fn prepare(
    session: &Arc<Session>,
    queued: &Receiver<QueuedSubmission>,
    mut release: oneshot::Receiver<()>,
) -> CodexResult<()> {
    let admission = &session.submission_admission;
    let _order = admission.send_lock.try_lock().map_err(|_| {
        CodexErr::InvalidRequest("branch is busy admitting input; retry detach when idle".into())
    })?;
    admission.check_ready()?;
    if matches!(
        release.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ) {
        return Err(CodexErr::TurnAborted);
    }
    let code_mode_busy = {
        let tasks = session
            .code_mode_message_tasks
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        !tasks.is_empty()
    };
    // Background mailbox wakes also reserve active_turn. Keep it through sealing so
    // their reservation cannot slip between the idle check and the ownership boundary.
    let active = session.active_turn.lock().await;
    // This runs in the submission actor, after every preceding accepted turn start.
    // Holding send order excludes enqueueing between the empty-queue check and sealing.
    if !queued.is_empty()
        || active.is_some()
        || session.has_outstanding_durable_sleep()
        || session.conversation.running_state().await.is_some()
        || code_mode_busy
        || session.input_queue.has_queued_turn_trigger().await
        || session.input_queue.has_trigger_turn_mailbox_items().await
        || !session
            .services
            .unified_exec_manager
            .list_processes()
            .await
            .is_empty()
    {
        return Err(CodexErr::InvalidRequest(
            "branch has active or queued work; retry detach when idle".into(),
        ));
    }
    let id = Uuid::now_v7();
    {
        let mut handoff = admission
            .ownership_handoff
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *handoff = Some(id);
        admission
            .subtree_unload_pending
            .store(/*val*/ true, Ordering::Release);
    }
    drop(active);
    let admission = Arc::downgrade(admission);
    let session = Arc::downgrade(session);
    tokio::spawn(async move {
        let _ = release.await;
        let Some(admission) = admission.upgrade() else {
            return;
        };
        let released = {
            let mut handoff = admission
                .ownership_handoff
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if *handoff == Some(id) && !admission.completion_sealed.load(Ordering::Acquire) {
                *handoff = None;
                admission
                    .subtree_unload_pending
                    .store(/*val*/ false, Ordering::Release);
                true
            } else {
                false
            }
        };
        if released {
            admission.changed.notify_waiters();
            // A late accepted completion may have attempted its wake while preparation
            // was sealed. A cancelled handoff must restore that pending work as well.
            if let Some(session) = session.upgrade() {
                session.maybe_start_turn_for_pending_work().await;
            }
        }
    });
    Ok(())
}
