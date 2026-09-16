use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::TaskTrackerToken;

use super::session_registry::CellOwner;

struct CleanupInner {
    complete: CancellationToken,
    callbacks: TaskTracker,
    host_exited: CancellationToken,
    close_confirmed: AtomicBool,
    callback_panicked: AtomicBool,
}

#[derive(Clone)]
pub(in crate::remote_session) struct SessionCleanup {
    inner: Arc<CleanupInner>,
}

impl SessionCleanup {
    pub(in crate::remote_session) fn new(host_exited: CancellationToken) -> Self {
        Self {
            inner: Arc::new(CleanupInner {
                complete: CancellationToken::new(),
                callbacks: TaskTracker::new(),
                host_exited,
                close_confirmed: AtomicBool::new(false),
                callback_panicked: AtomicBool::new(false),
            }),
        }
    }

    pub(super) fn fail(&self, cells: Vec<CellOwner>) {
        for owner in cells {
            self.notify_cell_closed(&owner.delegate, &owner.cell_id);
        }
        self.connection_failed();
    }

    pub(in crate::remote_session) fn connection_failed(&self) {
        self.inner.complete.cancel();
    }

    pub(super) fn notify_cell_closed(
        &self,
        delegate: &Arc<dyn CodeModeSessionDelegate>,
        cell_id: &CellId,
    ) {
        if std::panic::catch_unwind(AssertUnwindSafe(|| delegate.cell_closed(cell_id))).is_err() {
            self.inner.callback_panicked.store(true, Ordering::Release);
        }
    }

    pub(in crate::remote_session) async fn wait(&self) {
        self.inner.complete.cancelled().await;
    }

    pub(in crate::remote_session) fn is_durably_complete(&self) -> bool {
        self.inner.complete.is_cancelled()
            && (self.inner.close_confirmed.load(Ordering::Acquire)
                || self.inner.host_exited.is_cancelled())
            && self.inner.callbacks.is_empty()
            && !self.inner.callback_panicked.load(Ordering::Acquire)
    }

    pub(super) fn accepted_callback(&self) -> AcceptedCallback {
        AcceptedCallback {
            _token: self.inner.callbacks.token(),
            inner: Arc::clone(&self.inner),
        }
    }

    pub(in crate::remote_session) fn close(&self) {
        self.inner.close_confirmed.store(true, Ordering::Release);
        self.inner.complete.cancel();
    }

    pub(in crate::remote_session) async fn wait_durably(&self) -> Result<(), String> {
        self.wait().await;
        if !self.inner.close_confirmed.load(Ordering::Acquire) {
            self.inner.host_exited.cancelled().await;
        }
        self.inner.callbacks.close();
        self.inner.callbacks.wait().await;
        if self.inner.callback_panicked.load(Ordering::Acquire) {
            Err("accepted code-mode provider callback panicked".to_string())
        } else {
            Ok(())
        }
    }
}

pub(super) struct AcceptedCallback {
    _token: TaskTrackerToken,
    inner: Arc<CleanupInner>,
}

impl Drop for AcceptedCallback {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.inner.callback_panicked.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod tests;
