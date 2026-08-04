//! Shared lifecycle fencing for response observation.
//!
//! Close epochs revoke future subscriptions in this process. They are not durable
//! identities and must never authorize consumption of historical response receipts.

use super::*;

impl ThreadManagerState {
    /// The existing restoration, eviction and explicit-close gate is also the
    /// response-observation lifecycle boundary. There is no second lock authority.
    pub(crate) fn v2_spawn_resume_lock(&self, thread_id: ThreadId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .v2_spawn_resume_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&thread_id).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(thread_id, Arc::downgrade(&lock));
        lock
    }

    pub(crate) fn agent_lifecycle_lock(&self, thread_id: ThreadId) -> Arc<tokio::sync::Mutex<()>> {
        self.v2_spawn_resume_lock(thread_id)
    }

    /// Pin a live endpoint. Callers holding another endpoint's gate must use a
    /// nonblocking acquisition there instead of introducing a parent/child cycle.
    pub(crate) async fn acquire_live_agent_lifecycle(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<tokio::sync::OwnedMutexGuard<()>> {
        let guard = self.agent_lifecycle_lock(thread_id).lock_owned().await;
        let thread = self.get_thread(thread_id).await?;
        if !thread.is_running() {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        thread.session.submission_admission.check_ready()?;
        self.check_restoration_fence(thread_id)?;
        Ok(guard)
    }

    /// Capture an epoch while holding the shared lifecycle guard. Observation
    /// setup and explicit close must use the same guard to order acceptance.
    pub(crate) fn agent_lifecycle_generation(&self, thread_id: ThreadId) -> u64 {
        self.agent_lifecycle_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .copied()
            .unwrap_or_default()
    }

    /// Explicit close only, under the shared lifecycle guard; silent residency
    /// retirement and failed-restoration cleanup must not revoke future recovery.
    pub(crate) fn advance_agent_lifecycle_generation(&self, thread_id: ThreadId) {
        {
            let mut generations = self
                .agent_lifecycle_generations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let generation = generations.entry(thread_id).or_default();
            *generation = generation.wrapping_add(1);
        }
        self.agent_lifecycle_changed.notify_waiters();
    }

    pub(crate) fn agent_lifecycle_generation_is_current(
        &self,
        thread_id: ThreadId,
        generation: u64,
    ) -> bool {
        self.agent_lifecycle_generation(thread_id) == generation
    }

    /// Order a synchronous acceptance against explicit close. Callbacks may mutate their
    /// observation registry but must not acquire another lifecycle or epoch lock.
    pub(crate) fn with_current_agent_lifecycle_generation<R>(
        &self,
        thread_id: ThreadId,
        generation: u64,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        let generations = self
            .agent_lifecycle_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if generations.get(&thread_id).copied().unwrap_or_default() != generation {
            return None;
        }
        Some(action())
    }

    /// Create before checking the epoch so a concurrent close cannot be missed.
    pub(crate) fn wait_for_agent_lifecycle_change(&self) -> tokio::sync::futures::OwnedNotified {
        Arc::clone(&self.agent_lifecycle_changed).notified_owned()
    }

    pub(crate) fn subscribe_thread_created(&self) -> broadcast::Receiver<ThreadId> {
        self.thread_created_tx.subscribe()
    }
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
