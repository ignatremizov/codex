//! True process-exit acknowledgement is separate from UI cancellation and synthetic exit state.

use super::ProcessHandle;
use super::UnifiedExecError;
use super::UnifiedExecProcess;
use tokio::sync::watch;

#[derive(Clone, Copy)]
pub(super) enum ActualExit {
    Pending,
    Exited,
    Unknown,
}

pub(super) struct ExitConfirmationGuard(pub(super) watch::Sender<ActualExit>);

impl Drop for ExitConfirmationGuard {
    fn drop(&mut self) {
        self.0.send_if_modified(|state| {
            if matches!(state, ActualExit::Pending) {
                *state = ActualExit::Unknown;
                true
            } else {
                false
            }
        });
    }
}

impl UnifiedExecProcess {
    pub(in crate::unified_exec) fn actual_exit_confirmed(&self) -> bool {
        matches!(*self.actual_exit.borrow(), ActualExit::Exited)
    }

    pub(in crate::unified_exec) async fn terminate_and_wait_for_actual_exit(
        &self,
    ) -> Result<(), UnifiedExecError> {
        if self.actual_exit_confirmed() {
            return Ok(());
        }
        match &self.process_handle {
            // Preserve readers and the real exit waiter until the child has exited.
            ProcessHandle::Local(process) => process.request_terminate(),
            ProcessHandle::ExecServer(process) => {
                process
                    .terminate()
                    .await
                    .map_err(|error| UnifiedExecError::process_failed(error.to_string()))?;
                let response = process
                    .read(
                        /*after_seq*/ None,
                        /*max_bytes*/ Some(1),
                        /*wait_ms*/ Some(0),
                    )
                    .await
                    .map_err(|error| UnifiedExecError::process_failed(error.to_string()))?;
                if response.exited {
                    self.actual_exit.send_replace(ActualExit::Exited);
                }
            }
        }
        let mut exit = self.actual_exit.subscribe();
        loop {
            let state = *exit.borrow_and_update();
            match state {
                ActualExit::Exited => return Ok(()),
                ActualExit::Pending => {}
                ActualExit::Unknown => {
                    // An output reader may have failed or been retired before an exit event.
                    // A fresh authoritative remote read permits retry without guessing from
                    // cancellation tokens, closed streams, or a successful terminate RPC.
                    if let ProcessHandle::ExecServer(process) = &self.process_handle {
                        let response = process
                            .read(
                                /*after_seq*/ None,
                                /*max_bytes*/ Some(1),
                                /*wait_ms*/ Some(0),
                            )
                            .await
                            .map_err(|error| UnifiedExecError::process_failed(error.to_string()))?;
                        if response.exited {
                            self.actual_exit.send_replace(ActualExit::Exited);
                            return Ok(());
                        }
                    }
                    return Err(UnifiedExecError::process_failed(
                        "process exit was not confirmed; retain ownership and retry shutdown"
                            .to_string(),
                    ));
                }
            }
            exit.changed().await.map_err(|_| {
                UnifiedExecError::process_failed(
                    "process exit acknowledgement was lost".to_string(),
                )
            })?;
        }
    }
}
