use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::select;
use tokio::sync::Notify;

use super::GRACEFULL_INTERRUPTION_TIMEOUT_MS;
use crate::state::RunningTask;

#[derive(Default)]
pub(crate) struct TaskStartupState {
    complete: AtomicBool,
    changed: Notify,
}

impl TaskStartupState {
    pub(crate) fn complete(&self) {
        if !self.complete.swap(true, Ordering::AcqRel) {
            // One abort path owns a running task. `notify_one` retains a permit when completion
            // races the waiter's first poll, while `notify_waiters` would lose that wake.
            self.changed.notify_one();
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    pub(crate) async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            if self.is_complete() {
                return;
            }
            changed.await;
        }
    }
}

pub(super) struct TaskStartupCompletionGuard(Arc<TaskStartupState>);

impl TaskStartupCompletionGuard {
    pub(super) fn new(startup: Arc<TaskStartupState>) -> Self {
        Self(startup)
    }
}

impl Drop for TaskStartupCompletionGuard {
    fn drop(&mut self) {
        self.0.complete();
    }
}

pub(super) async fn wait_for_task_completion_with_grace(task: &RunningTask) -> bool {
    if task.handle.is_finished() {
        return true;
    }
    select! {
        _ = task.done.notified() => true,
        _ = tokio::time::sleep(Duration::from_millis(GRACEFULL_INTERRUPTION_TIMEOUT_MS)) => {
            task.handle.is_finished()
        }
    }
}
