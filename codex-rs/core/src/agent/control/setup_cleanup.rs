use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use tokio::sync::oneshot;
use tracing::warn;

use crate::thread_manager::ThreadManagerState;

type SetupCleanupFuture = Pin<Box<dyn Future<Output = CodexResult<()>> + Send + 'static>>;

/// Owns asynchronous rollback for setup state that must not survive a cancelled caller.
///
/// Tool dispatch can abort the future performing a spawn or resume. Rust drops its local values
/// synchronously, so this guard moves the remaining asynchronous rollback into an independent
/// task. Successful setup must explicitly disarm it.
pub(crate) struct SetupCleanupGuard {
    operation: &'static str,
    cleanup: Option<SetupCleanupFuture>,
    cancellation_lifecycle: Option<(Arc<ThreadManagerState>, ThreadId)>,
}

impl SetupCleanupGuard {
    pub(crate) fn new(
        operation: &'static str,
        cleanup: impl Future<Output = CodexResult<()>> + Send + 'static,
    ) -> Self {
        Self {
            operation,
            cleanup: Some(Box::pin(cleanup)),
            cancellation_lifecycle: None,
        }
    }

    pub(crate) fn new_with_agent_lifecycle(
        operation: &'static str,
        state: Arc<ThreadManagerState>,
        thread_id: ThreadId,
        cleanup: impl Future<Output = CodexResult<()>> + Send + 'static,
    ) -> Self {
        Self {
            operation,
            cleanup: Some(Box::pin(cleanup)),
            cancellation_lifecycle: Some((state, thread_id)),
        }
    }

    pub(crate) fn disarm(mut self) {
        self.cleanup = None;
    }

    pub(crate) async fn rollback(mut self) -> CodexResult<()> {
        let Some(cleanup) = self.cleanup.take() else {
            return Ok(());
        };
        let operation = self.operation;
        let (cancellation_tx, cancellation_rx) = oneshot::channel();
        let cleanup_task = tokio::spawn(run_explicit_cleanup(
            cleanup,
            self.cancellation_lifecycle.take(),
            cancellation_rx,
        ));
        let result = cleanup_task.await.map_err(|err| {
            CodexErr::Fatal(format!(
                "{operation} rollback task failed before cleanup: {err}"
            ))
        })?;
        drop(cancellation_tx);
        result
    }
}

impl Drop for SetupCleanupGuard {
    fn drop(&mut self) {
        let Some(cleanup) = self.cleanup.take() else {
            return;
        };
        schedule_cleanup(self.operation, cleanup, self.cancellation_lifecycle.take());
    }
}

fn schedule_cleanup(
    operation: &'static str,
    cleanup: SetupCleanupFuture,
    cancellation_lifecycle: Option<(Arc<ThreadManagerState>, ThreadId)>,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        warn!(
            operation,
            "could not schedule cancelled agent setup rollback"
        );
        return;
    };
    runtime.spawn(async move {
        if let Err(err) = run_cleanup(cleanup, cancellation_lifecycle).await {
            warn!(operation, %err, "cancelled agent setup rollback failed");
        }
    });
}

async fn run_cleanup(
    cleanup: SetupCleanupFuture,
    cancellation_lifecycle: Option<(Arc<ThreadManagerState>, ThreadId)>,
) -> CodexResult<()> {
    let _lifecycle_guard = match cancellation_lifecycle {
        Some((state, thread_id)) => Some(state.agent_lifecycle_lock(thread_id).lock_owned().await),
        None => None,
    };
    cleanup.await
}

async fn run_explicit_cleanup(
    mut cleanup: SetupCleanupFuture,
    cancellation_lifecycle: Option<(Arc<ThreadManagerState>, ThreadId)>,
    mut cancellation_rx: oneshot::Receiver<()>,
) -> CodexResult<()> {
    tokio::select! {
        result = cleanup.as_mut() => result,
        _ = &mut cancellation_rx => run_cleanup(cleanup, cancellation_lifecycle).await,
    }
}

#[cfg(test)]
#[path = "setup_cleanup_tests.rs"]
mod tests;
