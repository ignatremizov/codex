//! Orders public submissions around an exclusive, durable history mutation.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use async_channel::Sender;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::Submission;
use tokio::sync::Mutex;

use super::command_approval::QueuedSubmission;

#[derive(Default)]
pub(crate) struct SubmissionAdmission {
    pub(super) send_lock: Mutex<()>,
    pub(super) state: StdMutex<State>,
    writer_closed: AtomicBool,
    pub(super) completion_closed: AtomicBool,
    pub(super) completion_sealed: AtomicBool,
    pub(super) accepted_completions: std::sync::atomic::AtomicUsize,
    pub(super) completion_publication: StdMutex<Option<String>>,
    pub(super) changed: tokio::sync::Notify,
}

#[derive(Default)]
pub(super) enum State {
    #[default]
    Ready,
    RollbackPending(String),
    ReloadRequired,
}

impl SubmissionAdmission {
    pub(crate) fn check_ready(&self) -> CodexResult<()> {
        match &*self.state.lock().unwrap_or_else(PoisonError::into_inner) {
            State::Ready => Ok(()),
            State::RollbackPending(_) => Err(CodexErr::InvalidRequest(
                "thread rollback is already in progress".to_string(),
            )),
            State::ReloadRequired => Err(CodexErr::InvalidRequest(
                "thread history must be reloaded before accepting more work".to_string(),
            )),
        }
    }

    pub(crate) fn rollback_completed(&self, submission_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(&*state, State::RollbackPending(id) if id == submission_id) {
            *state = State::Ready;
        }
        self.changed.notify_waiters();
    }

    pub(crate) fn rollback_requires_reload(&self) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = State::ReloadRequired;
        self.changed.notify_waiters();
    }

    pub(crate) fn requires_reload(&self) -> bool {
        matches!(
            &*self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::ReloadRequired
        )
    }

    pub(crate) fn acknowledge_writer_closed(&self) {
        self.writer_closed.store(true, Ordering::Release);
    }

    pub(crate) fn can_reload(&self) -> bool {
        self.requires_reload() && self.writer_closed.load(Ordering::Acquire)
    }

    /// Holds public history-only injection ahead of any rollback reservation.
    pub(crate) async fn admit_injection(&self) -> CodexResult<tokio::sync::MutexGuard<'_, ()>> {
        let guard = self.send_lock.lock().await;
        if self.completion_closed.load(Ordering::Acquire)
            || self.completion_sealed.load(Ordering::Acquire)
        {
            return Err(CodexErr::InvalidRequest(
                "thread shutdown is already in progress".to_string(),
            ));
        }
        self.check_ready()?;
        Ok(guard)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "submission order must cover channel acceptance"
    )]
    pub(crate) async fn enqueue(
        self: &Arc<Self>,
        sender: &Sender<QueuedSubmission>,
        submission: impl Into<QueuedSubmission>,
    ) -> CodexResult<()> {
        let submission = submission.into();
        let _order = self.send_lock.lock().await;
        let mut shutdown = super::completion_admission::ShutdownAdmission::for_submission(
            Arc::clone(self),
            &submission.submission,
        );
        let mut reservation = self.reserve(&submission.submission)?;
        sender
            .send(submission)
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        // There is no await between enqueue acceptance and transferring ownership to the loop.
        if let Some(reservation) = reservation.as_mut() {
            reservation.submission_id = None;
        }
        shutdown.commit();
        Ok(())
    }

    /// Fails before acceptance when another sender or a full queue would require waiting.
    pub(crate) fn try_enqueue(
        self: &Arc<Self>,
        sender: &Sender<QueuedSubmission>,
        submission: impl Into<QueuedSubmission>,
    ) -> CodexResult<()> {
        let submission = submission.into();
        let _order = self.send_lock.try_lock().map_err(|_| {
            CodexErr::InvalidRequest(
                "thread submission admission is busy; retry when idle".to_string(),
            )
        })?;
        let mut shutdown = super::completion_admission::ShutdownAdmission::for_submission(
            Arc::clone(self),
            &submission.submission,
        );
        let mut reservation = self.reserve(&submission.submission)?;
        sender.try_send(submission).map_err(|error| match error {
            async_channel::TrySendError::Full(_) => CodexErr::InvalidRequest(
                "thread submission queue is full; retry when idle".to_string(),
            ),
            async_channel::TrySendError::Closed(_) => CodexErr::InternalAgentDied,
        })?;
        if let Some(reservation) = reservation.as_mut() {
            reservation.submission_id = None;
        }
        shutdown.commit();
        Ok(())
    }

    /// The caller owns send order; only an unaccepted matching rollback may release this guard.
    fn reserve(self: &Arc<Self>, submission: &Submission) -> CodexResult<Option<Reservation>> {
        let shutdown = matches!(&submission.op, Op::Shutdown);
        if !shutdown {
            if self.completion_closed.load(Ordering::Acquire)
                || self.completion_sealed.load(Ordering::Acquire)
            {
                return Err(CodexErr::InvalidRequest(
                    "thread shutdown is already in progress".to_string(),
                ));
            }
            self.check_ready()?;
        }
        let rollback = matches!(
            &submission.op,
            Op::ThreadRollback { .. } | Op::ThreadRollbackMaterialized { .. }
        );
        let reservation = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if !shutdown && !matches!(*state, State::Ready) {
                return Err(CodexErr::InvalidRequest(
                    "thread history mutation admission changed".to_string(),
                ));
            }
            if rollback {
                *state = State::RollbackPending(submission.id.clone());
                Some(Reservation {
                    admission: Arc::clone(self),
                    submission_id: Some(submission.id.clone()),
                })
            } else {
                None
            }
        };
        Ok(reservation)
    }
}

#[cfg(test)]
#[path = "submission_admission_tests.rs"]
mod tests;

struct Reservation {
    admission: Arc<SubmissionAdmission>,
    submission_id: Option<String>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(id) = &self.submission_id {
            self.admission.rollback_completed(id);
        }
    }
}
