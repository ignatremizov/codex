use super::*;
use crate::unified_exec::process::NoopSpawnLifecycle;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ExecServerError;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use codex_sandboxing::SandboxType;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::sync::watch;

#[tokio::test]
async fn synthetic_termination_does_not_release_actual_exit_barrier() {
    let (writer_tx, _writer_rx) = mpsc::channel(1);
    let (_stdout_tx, stdout_rx) = tokio::sync::broadcast::channel(/*capacity*/ 1);
    let (exit_tx, exit_rx) = oneshot::channel();
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let process = Arc::new(
        UnifiedExecProcess::from_spawned(spawned, SandboxType::None, Box::new(NoopSpawnLifecycle))
            .await
            .expect("managed process"),
    );
    process.terminate_confirmed().await.expect("kill request");
    assert!(process.has_exited(), "legacy synthetic state is observable");
    let manager = UnifiedExecProcessManager::default();
    manager.retain_process_for_shutdown(Arc::clone(&process));
    let mut drain = Box::pin(manager.shutdown_durably());
    assert!(drain.as_mut().now_or_never().is_none());
    assert!(!process.actual_exit_confirmed());
    exit_tx.send(0).expect("confirm actual process exit");
    drain.await.expect("actual exit permits shutdown");
}

#[tokio::test]
async fn observed_output_close_does_not_release_a_running_raw_producer() {
    let (writer_tx, _writer_rx) = mpsc::channel(/*buffer*/ 1);
    let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel(/*capacity*/ 1);
    let (exit_tx, exit_rx) = oneshot::channel();
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let process = Arc::new(
        UnifiedExecProcess::from_spawned(spawned, SandboxType::None, Box::new(NoopSpawnLifecycle))
            .await
            .expect("managed process"),
    );
    // A consumer has claimed the stream; shutdown must not abort its producer.
    let _output = process.take_output_receiver().expect("output receiver");
    exit_tx.send(0).expect("confirm actual process exit");
    tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 1), async {
        while !process.actual_exit_confirmed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual exit");
    // Remote output may announce closed before its task has returned.
    process
        .output_handles()
        .output_closed
        .store(true, Ordering::Release);
    let manager = UnifiedExecProcessManager::default();
    manager.retain_process_for_shutdown(Arc::clone(&process));
    let mut drain = Box::pin(manager.shutdown_durably());
    assert!(drain.as_mut().now_or_never().is_none());
    assert!(!process.output_task_completion().is_cancelled());
    drop(stdout_tx);
    tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 1), drain)
        .await
        .expect("raw producer completes")
        .expect("durable drain");
    assert!(process.output_task_completion().is_cancelled());
}

#[tokio::test]
async fn producer_completion_is_required_after_process_inventory_is_empty() {
    let manager = UnifiedExecProcessManager::default();
    let (finish, unfinished) = oneshot::channel();
    let persisted = Arc::new(AtomicBool::new(false));
    let writer = Arc::clone(&persisted);
    manager.track_producer(async move {
        unfinished.await.map_err(|error| error.to_string())?;
        writer.store(true, Ordering::Release);
        Ok(())
    });
    let mut drain = Box::pin(manager.shutdown_durably());
    assert!(drain.as_mut().now_or_never().is_none());
    assert!(!persisted.load(Ordering::Acquire));
    assert!(manager.start_execution(async {}).is_err());
    finish.send(()).expect("allow final event persistence");
    drain.await.expect("producer completes before shutdown");
    assert!(persisted.load(Ordering::Acquire));
}

#[tokio::test]
async fn failed_producer_is_not_mistaken_for_successful_drain() {
    let manager = UnifiedExecProcessManager::default();
    manager.track_producer(async { Err("terminal event failed".to_string()) });
    assert!(manager.shutdown_durably().await.is_err());
    assert!(manager.shutdown_durably().await.is_err());
}

struct RetryableRemoteProcess {
    id: ProcessId,
    fail_terminate: AtomicBool,
    exited: AtomicBool,
    wake: watch::Sender<u64>,
}

impl ExecProcess for RetryableRemoteProcess {
    fn process_id(&self) -> &ProcessId {
        &self.id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(async move {
            Ok(ReadResponse {
                chunks: Vec::new(),
                next_seq: 1,
                exited: self.exited.load(Ordering::Acquire),
                exit_code: Some(0),
                closed: false,
                failure: None,
                sandbox_denied: false,
            })
        })
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async {
            Ok(WriteResponse {
                status: WriteStatus::Accepted,
            })
        })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(async move {
            if self.fail_terminate.load(Ordering::Acquire) {
                Err(ExecServerError::Protocol(
                    "termination unavailable".to_string(),
                ))
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn failed_termination_retains_exact_process_for_retry() {
    let remote = Arc::new(RetryableRemoteProcess {
        id: "retry-process".to_string().into(),
        fail_terminate: AtomicBool::new(true),
        exited: AtomicBool::new(false),
        wake: watch::channel(0).0,
    });
    let process = Arc::new(
        UnifiedExecProcess::from_exec_server_started(StartedExecProcess {
            process: remote.clone(),
            sandbox_type: Some(SandboxType::None),
        })
        .await
        .expect("remote process"),
    );
    let manager = UnifiedExecProcessManager::default();
    manager.retain_process_for_shutdown(process);
    assert!(manager.shutdown_durably().await.is_err());
    assert_eq!(
        manager
            .shutdown
            .state
            .lock()
            .expect("shutdown state")
            .processes
            .len(),
        1
    );
    remote.fail_terminate.store(false, Ordering::Release);
    remote.exited.store(true, Ordering::Release);
    manager
        .shutdown_durably()
        .await
        .expect("retry confirmed exit");
    assert_eq!(
        manager
            .shutdown
            .state
            .lock()
            .expect("shutdown state")
            .processes
            .len(),
        0
    );
}
