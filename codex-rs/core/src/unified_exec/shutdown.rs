//! Session-owned process and producer drain, independent of the resumable-process inventory.

use super::UnifiedExecError;
use super::UnifiedExecProcessManager;
use super::process::UnifiedExecProcess;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

type ProducerCompletion = Shared<BoxFuture<'static, Result<(), String>>>;

#[derive(Default)]
struct State {
    closed: bool,
    processes: Vec<Arc<UnifiedExecProcess>>,
    producers: Vec<ProducerCompletion>,
}

#[derive(Default)]
pub(super) struct ShutdownBarrier {
    state: Mutex<State>,
    changed: Notify,
}

enum Admission {
    NewExecution,
    AcceptedProducer,
}

impl ShutdownBarrier {
    fn spawn<F>(
        &self,
        future: F,
        admission: Admission,
        completion_status: fn(&F::Output) -> Result<(), String>,
    ) -> Result<JoinHandle<F::Output>, UnifiedExecError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed && matches!(admission, Admission::NewExecution) {
            return Err(UnifiedExecError::process_failed(
                "thread process shutdown is in progress".to_string(),
            ));
        }
        state
            .producers
            .retain(|completion| !matches!(completion.clone().now_or_never(), Some(Ok(()))));
        let (done, completion) = oneshot::channel();
        state.producers.push(
            async move {
                completion
                    .await
                    .map_err(|_| "execution producer terminated without completing".to_string())
                    .and_then(std::convert::identity)
            }
            .boxed()
            .shared(),
        );
        // Register before spawning, so even immediate completion or publication participates
        // in the manager's drain. A panic/abort drops `done` and remains a drain failure.
        let handle = tokio::spawn(async move {
            let output = future.await;
            let _ = done.send(completion_status(&output));
            output
        });
        self.changed.notify_waiters();
        Ok(handle)
    }
}

impl UnifiedExecProcessManager {
    pub(crate) fn start_execution<F>(
        &self,
        future: F,
    ) -> Result<JoinHandle<F::Output>, UnifiedExecError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.shutdown
            .spawn(future, Admission::NewExecution, |_| Ok(()))
    }

    pub(super) fn track_producer<F>(&self, future: F)
    where
        F: Future<Output = Result<(), String>> + Send + 'static,
    {
        // Accepted producers may register while shutdown is draining a previously admitted
        // launch. They are not new execution and cannot be silently discarded.
        let _ = self
            .shutdown
            .spawn(future, Admission::AcceptedProducer, Clone::clone);
    }

    pub(super) fn retain_process_for_shutdown(&self, process: Arc<UnifiedExecProcess>) {
        let mut state = self
            .shutdown
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state
            .processes
            .retain(|process| !process.actual_exit_confirmed());
        state.processes.push(process);
        self.shutdown.changed.notify_waiters();
    }

    pub(crate) fn begin_durable_shutdown(&self) {
        self.shutdown
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
        self.shutdown.changed.notify_waiters();
    }

    pub(crate) async fn shutdown_durably(&self) -> Result<(), UnifiedExecError> {
        self.begin_durable_shutdown();
        {
            let mut store = self.process_store.lock().await;
            for entry in store.user_shell_commands.values() {
                entry.cancellation_token.cancel();
            }
            store.pending_user_shell_submissions.clear();
        }
        self.user_shell_submission_changed.notify_waiters();
        let drain = async {
            loop {
                let changed = self.shutdown.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let (processes, producers) = {
                    let state = self
                        .shutdown
                        .state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    (state.processes.clone(), state.producers.clone())
                };
                // No process-store, interaction, or observation lock is held while a process
                // exits or while its output/watcher task persists its terminal event.
                for process in processes {
                    process.terminate_and_wait_for_actual_exit().await?;
                }
                let completed = async {
                    for producer in producers {
                        producer.await.map_err(UnifiedExecError::process_failed)?;
                    }
                    Ok::<(), UnifiedExecError>(())
                };
                tokio::select! {
                    biased;
                    _ = &mut changed => continue,
                    result = completed => result?,
                }
                let state = self
                    .shutdown
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if state
                    .processes
                    .iter()
                    .all(|process| process.actual_exit_confirmed())
                    && state
                        .producers
                        .iter()
                        .all(|producer| matches!(producer.clone().now_or_never(), Some(Ok(()))))
                {
                    return Ok(());
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), drain)
            .await
            .map_err(|_| {
                UnifiedExecError::process_failed(
                    "timed out confirming process exit and final execution events; retry shutdown"
                        .to_string(),
                )
            })??;
        // Producers are done and admission stays closed. Legacy cleanup may now retire the
        // process inventory without losing ownership of an unconfirmed process.
        self.terminate_all_processes().await;
        let mut state = self
            .shutdown
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.processes.clear();
        state.producers.clear();
        Ok(())
    }
}

#[cfg(test)]
#[path = "shutdown_tests.rs"]
mod tests;
