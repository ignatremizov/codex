//! Exact-instance reservations for out-of-band completion publication.

use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::atomic::Ordering;

use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::Submission;

use super::SubmissionAdmission;
use super::submission_admission::State;

pub(crate) struct AcceptedCompletionDelivery {
    admission: Arc<SubmissionAdmission>,
}

impl Drop for AcceptedCompletionDelivery {
    fn drop(&mut self) {
        self.admission
            .accepted_completions
            .fetch_sub(1, Ordering::AcqRel);
        self.admission.changed.notify_waiters();
    }
}

/// Cancelling a shutdown before queue acceptance does not close future delivery.
pub(super) struct ShutdownAdmission {
    admission: Arc<SubmissionAdmission>,
    restore: bool,
}

impl ShutdownAdmission {
    pub(super) fn for_submission(
        admission: Arc<SubmissionAdmission>,
        submission: &Submission,
    ) -> Self {
        let restore = matches!(&submission.op, Op::Shutdown)
            && !admission.completion_closed.swap(true, Ordering::AcqRel);
        admission.changed.notify_waiters();
        Self { admission, restore }
    }

    pub(super) fn commit(&mut self) {
        self.restore = false;
    }
}

impl Drop for ShutdownAdmission {
    fn drop(&mut self) {
        if self.restore
            && !self.admission.completion_sealed.load(Ordering::Acquire)
            && !self.admission.requires_reload()
        {
            self.admission
                .completion_closed
                .store(false, Ordering::Release);
            self.admission.changed.notify_waiters();
        }
    }
}

impl SubmissionAdmission {
    pub(crate) fn try_accept_completion_delivery(
        self: &Arc<Self>,
    ) -> Option<AcceptedCompletionDelivery> {
        if self.completion_closed.load(Ordering::Acquire)
            || self.completion_sealed.load(Ordering::Acquire)
            || self.requires_reload()
        {
            return None;
        }
        self.accepted_completions.fetch_add(1, Ordering::AcqRel);
        let reservation = AcceptedCompletionDelivery {
            admission: Arc::clone(self),
        };
        if self.completion_closed.load(Ordering::Acquire)
            || self.completion_sealed.load(Ordering::Acquire)
            || self.requires_reload()
        {
            return None;
        }
        Some(reservation)
    }

    /// Only an already accepted delivery may cross ordinary shutdown admission.
    pub(super) async fn admit_completion(
        &self,
        reservation: &AcceptedCompletionDelivery,
    ) -> CodexResult<tokio::sync::MutexGuard<'_, ()>> {
        if !std::ptr::eq(self, Arc::as_ptr(&reservation.admission)) {
            return Err(CodexErr::InvalidRequest(
                "completion belongs to another session".to_string(),
            ));
        }
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let order = self.send_lock.lock().await;
            let ready = match &*self.state.lock().unwrap_or_else(PoisonError::into_inner) {
                State::Ready => true,
                State::RollbackPending(_) => false,
                State::ReloadRequired => {
                    return Err(CodexErr::InvalidRequest(
                        "completion requires canonical reload".to_string(),
                    ));
                }
            };
            if ready
                && self
                    .completion_publication
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_none()
            {
                return Ok(order);
            }
            drop(order);
            changed.await;
        }
    }

    pub(super) fn begin_rollback_publication(&self, id: &str) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(&*state, State::RollbackPending(pending) if pending == id) {
            *self
                .completion_publication
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(id.to_string());
        }
    }

    pub(super) fn finish_rollback_publication(&self, id: &str) {
        let mut pending = self
            .completion_publication
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if pending.as_deref() == Some(id) {
            *pending = None;
        }
        self.changed.notify_waiters();
    }

    pub(crate) fn close_completion_admission(&self) {
        self.completion_sealed.store(true, Ordering::Release);
        self.completion_closed.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub(super) async fn drain_accepted_completions(&self) {
        self.close_completion_admission();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.accepted_completions.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}

#[cfg(test)]
#[path = "completion_admission_tests.rs"]
mod tests;
