//! Coalesce process-wide glibc trimming outside the async runtime workers.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::Instant;

const SETTLE_DELAY: Duration = Duration::from_secs(/*secs*/ 5);
const MIN_INTERVAL: Duration = Duration::from_secs(/*secs*/ 60);

static RECLAIMER: LazyLock<Arc<Reclaimer>> = LazyLock::new(|| Arc::new(Reclaimer::default()));

#[derive(Default)]
struct Reclaimer {
    state: Mutex<ReclaimState>,
}

#[derive(Default)]
struct ReclaimState {
    scheduled: bool,
    last_completed: Option<Instant>,
}

// The reservation follows the blocking task if its async waiter is cancelled.
// Dropping a waiting task also releases admission, including during runtime shutdown.
struct Reservation {
    reclaimer: Arc<Reclaimer>,
    completed_at: Option<Instant>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self
            .reclaimer
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.scheduled = false;
        if let Some(completed_at) = self.completed_at {
            state.last_completed = Some(completed_at);
        }
    }
}

impl Reclaimer {
    fn schedule(
        self: &Arc<Self>,
        reclaim: impl FnOnce() + Send + 'static,
    ) -> Option<JoinHandle<()>> {
        let ready_at = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.scheduled {
                return None;
            }
            state.scheduled = true;
            let settled = Instant::now() + SETTLE_DELAY;
            state
                .last_completed
                .map_or(settled, |last| settled.max(last + MIN_INTERVAL))
        };
        let reservation = Reservation {
            reclaimer: Arc::clone(self),
            completed_at: None,
        };
        Some(tokio::spawn(async move {
            tokio::time::sleep_until(ready_at).await;
            if let Err(error) = tokio::task::spawn_blocking(move || {
                let mut reservation = reservation;
                reclaim();
                reservation.completed_at = Some(Instant::now());
            })
            .await
            {
                tracing::warn!(%error, "allocator memory reclamation task failed");
            }
        }))
    }
}

pub(super) fn request() {
    // Dropping the handle detaches best-effort maintenance from the requesting thread.
    drop(RECLAIMER.schedule(|| {
        // SAFETY: glibc's thread-safe malloc_trim accepts no pointers and releases only
        // whole free pages. This module is excluded from musl and other allocators.
        let released = unsafe {
            libc::malloc_trim(/*__pad*/ 0)
        } != 0;
        tracing::debug!(released, "trimmed free glibc allocator pages");
    }));
}

#[cfg(test)]
#[path = "glibc_tests.rs"]
mod tests;
