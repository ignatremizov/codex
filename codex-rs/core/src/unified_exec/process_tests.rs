use super::process::UnifiedExecProcess;
use crate::unified_exec::UnifiedExecError;
use codex_exec_server::ExecOutputStream;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ExecServerError;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessOutputChunk;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;

struct MockExecProcess {
    process_id: ProcessId,
    write_response: WriteResponse,
    read_responses: Mutex<VecDeque<ReadResponse>>,
    terminate_error: Option<String>,
    wake_tx: watch::Sender<u64>,
}

impl MockExecProcess {
    async fn read(&self) -> Result<ReadResponse, ExecServerError> {
        Ok(self
            .read_responses
            .lock()
            .await
            .pop_front()
            .unwrap_or(ReadResponse {
                chunks: Vec::new(),
                next_seq: 1,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                sandbox_denied: false,
            }))
    }

    async fn terminate(&self) -> Result<(), ExecServerError> {
        if let Some(message) = &self.terminate_error {
            return Err(ExecServerError::Protocol(message.clone()));
        }
        Ok(())
    }
}

impl ExecProcess for MockExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
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
        Box::pin(MockExecProcess::read(self))
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { Ok(self.write_response.clone()) })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(MockExecProcess::terminate(self))
    }
}

pub(super) async fn remote_process(
    write_status: WriteStatus,
    terminate_error: Option<String>,
) -> UnifiedExecProcess {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "test-process".to_string().into(),
            write_response: WriteResponse {
                status: write_status,
            },
            read_responses: Mutex::new(VecDeque::new()),
            terminate_error,
            wake_tx,
        }),
    };

    UnifiedExecProcess::from_exec_server_started(started)
        .await
        .expect("remote process should start")
}

#[tokio::test]
async fn remote_write_unknown_process_marks_process_exited() {
    let process = remote_process(WriteStatus::UnknownProcess, /*terminate_error*/ None).await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn remote_write_closed_stdin_marks_process_exited() {
    let process = remote_process(WriteStatus::StdinClosed, /*terminate_error*/ None).await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn fail_and_terminate_preserves_failure_message() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;

    process.fail_and_terminate("network denied".to_string());
    process.fail_and_terminate("second failure".to_string());

    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("network denied".to_string())
    );
}

#[tokio::test]
async fn remote_terminate_confirmed_updates_state_on_success_only() {
    let process = remote_process(
        WriteStatus::Accepted,
        Some("terminate unavailable".to_string()),
    )
    .await;

    let err = process
        .terminate_confirmed()
        .await
        .expect_err("expected terminate failure");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert!(!process.has_exited());

    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;

    process
        .terminate_confirmed()
        .await
        .expect("terminate should succeed");

    assert!(process.has_exited());
}

#[tokio::test]
async fn exec_server_replay_gap_is_recorded_as_omitted_output() {
    let output_buffer = Arc::new(Mutex::new(
        crate::unified_exec::head_tail_buffer::HeadTailBuffer::new(/*max_bytes*/ 64),
    ));
    let output_transcript = Arc::new(Mutex::new(
        crate::unified_exec::head_tail_buffer::HeadTailBuffer::new(/*max_bytes*/ 64),
    ));
    let output_notify = Arc::new(tokio::sync::Notify::new());
    let (output_tx, mut output_rx) = tokio::sync::mpsc::channel(/*buffer*/ 3);
    let mut next_output_offset = 0;

    super::process::record_exec_server_output_chunk(
        &output_buffer,
        &output_transcript,
        &output_tx,
        &output_notify,
        &mut next_output_offset,
        ProcessOutputChunk {
            seq: 1,
            output_offset: 0,
            stream: ExecOutputStream::Stdout,
            chunk: b"before".to_vec().into(),
        },
    )
    .await;
    super::process::record_exec_server_output_chunk(
        &output_buffer,
        &output_transcript,
        &output_tx,
        &output_notify,
        &mut next_output_offset,
        ProcessOutputChunk {
            seq: 3,
            output_offset: 12,
            stream: ExecOutputStream::Stdout,
            chunk: b"after".to_vec().into(),
        },
    )
    .await;

    let transcript_state = {
        let transcript = output_transcript.lock().await;
        (
            transcript.omitted_bytes(),
            transcript.to_bytes_with_omission_marker(),
            next_output_offset,
        )
    };
    assert_eq!(
        transcript_state,
        (6, b"before\n... 6 bytes omitted ...\nafter".to_vec(), 17,)
    );
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), output_rx.recv())
            .await
            .expect("streamed replay prefix should arrive"),
        Some(b"before".to_vec())
    );
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), output_rx.recv())
            .await
            .expect("streamed replay should arrive"),
        Some(b"\n... 6 bytes omitted ...\n".to_vec())
    );
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), output_rx.recv())
            .await
            .expect("streamed replay tail should arrive"),
        Some(b"after".to_vec())
    );
}
