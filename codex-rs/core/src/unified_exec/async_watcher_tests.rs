use std::sync::Arc;

use super::Buffer;
use super::Emitter;
use super::INCOMPLETE_OUTPUT_WARNING;
use super::TRAILING_OUTPUT_CLOSE_TIMEOUT;
use super::TRAILING_OUTPUT_HARD_TIMEOUT;
use super::spawn_exit_watcher;
use super::start_streaming_output;
use super::utf8_boundary;
use crate::session::tests::make_session_and_context_with_rx;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::NoopSpawnLifecycle;
use crate::unified_exec::process::UnifiedExecProcess;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecOutputStream;
use codex_sandboxing::SandboxType;

use pretty_assertions::assert_eq;
use tokio::time::Duration;
use tokio::time::Instant;

struct StreamingOutputHarness {
    process: Arc<UnifiedExecProcess>,
    stdout_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    exit_tx: tokio::sync::oneshot::Sender<i32>,
    transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
    context: UnifiedExecContext,
    rx_event: async_channel::Receiver<Event>,
}

async fn streaming_output_harness(
    initial_output: Option<&[u8]>,
) -> anyhow::Result<StreamingOutputHarness> {
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
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
            .await?,
    );
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "streaming-output-test".to_string(),
    );
    if let Some(initial_output) = initial_output {
        let output_handles = process.output_handles();
        let output_notified = output_handles.output_notify.notified();
        tokio::pin!(output_notified);
        output_notified.as_mut().enable();
        stdout_tx
            .send(initial_output.to_vec())
            .expect("send initial output");
        tokio::time::timeout(Duration::from_secs(1), output_notified)
            .await
            .expect("initial output should be recorded");
    }
    let transcript = process.output_transcript();
    start_streaming_output(&process, &context);

    Ok(StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        context,
        rx_event,
    })
}

#[tokio::test]
async fn streaming_output_preserves_multibyte_characters_across_chunks() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        rx_event,
        ..
    } = streaming_output_harness(/*initial_output*/ None).await?;
    let output_stream_complete = process.output_stream_completion();

    stdout_tx.send(vec![0xc3]).expect("send UTF-8 lead byte");
    stdout_tx
        .send(vec![0xa9])
        .expect("send UTF-8 continuation byte");
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");
    output_stream_complete.cancelled().await;

    let event = rx_event.recv().await.expect("receive output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "streaming-output-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: "é".as_bytes().to_vec(),
        }
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        "é".as_bytes()
    );
    assert!(rx_event.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn exit_watcher_includes_output_emitted_before_streaming_started() -> anyhow::Result<()> {
    let initial_output = b"EARLY-OUTPUT-MARKER";
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        context,
        rx_event,
    } = streaming_output_harness(Some(initial_output)).await?;

    #[allow(deprecated)]
    let cwd = context.step_context.turn.cwd.clone().into();
    spawn_exit_watcher(
        Arc::clone(&process),
        Arc::clone(&context.session),
        Arc::clone(&context.step_context.turn),
        context.call_id,
        vec!["proof".to_string()],
        cwd,
        /*process_id*/ 123,
        /*plugin_attribution*/ None,
        transcript,
        Instant::now(),
        /*network_denial_monitor*/ None,
        /*plugin_metrics_sidecar*/ None,
    );

    exit_tx.send(0).expect("send exit");
    drop(stdout_tx);

    let mut streamed_output = String::new();
    let completed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = rx_event.recv().await.expect("command event");
            match event.msg {
                EventMsg::ExecCommandOutputDelta(delta) => {
                    streamed_output.push_str(&String::from_utf8_lossy(&delta.chunk));
                }
                EventMsg::ItemCompleted(completed) => break completed,
                _ => {}
            }
        }
    })
    .await
    .expect("command should complete");
    let TurnItem::CommandExecution(item) = completed.item else {
        panic!("expected CommandExecution");
    };
    assert_eq!(
        (
            item.status,
            item.exit_code,
            item.aggregated_output.as_deref()
        ),
        (
            CommandExecutionStatus::Completed,
            Some(0),
            Some("EARLY-OUTPUT-MARKER")
        )
    );
    assert_eq!(streamed_output, "EARLY-OUTPUT-MARKER");

    Ok(())
}

#[tokio::test]
async fn streaming_output_does_not_keep_unstored_process_alive() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        ..
    } = streaming_output_harness(/*initial_output*/ None).await?;
    let weak_process = Arc::downgrade(&process);

    drop(process);

    assert!(weak_process.upgrade().is_none());
    drop(stdout_tx);
    drop(exit_tx);
    Ok(())
}

#[tokio::test]
async fn streaming_output_finishes_on_close_without_waiting_for_grace() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        ..
    } = streaming_output_harness(/*initial_output*/ None).await?;
    let output_stream_complete = process.output_stream_completion();

    tokio::time::pause();
    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stdout_tx
            .send(b"LATE-OUTPUT-MARKER\xc3".to_vec())
            .expect("send late output");
    });

    tokio::time::timeout(Duration::from_secs(1), output_stream_complete.cancelled())
        .await
        .expect("output should drain");
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= Duration::from_millis(50) && elapsed < TRAILING_OUTPUT_CLOSE_TIMEOUT,
        "output close should finish before the close-timeout fallback: {elapsed:?}"
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        b"LATE-OUTPUT-MARKER\xc3"
    );

    Ok(())
}

#[tokio::test]
async fn streaming_output_close_timeout_resets_while_output_is_active() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        ..
    } = streaming_output_harness(/*initial_output*/ None).await?;
    let output_stream_complete = process.output_stream_completion();

    tokio::time::pause();
    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    tokio::spawn(async move {
        for marker in [b"A", b"B"] {
            tokio::time::sleep(Duration::from_millis(750)).await;
            stdout_tx
                .send(marker.to_vec())
                .expect("send active trailing output");
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    });

    tokio::time::timeout(Duration::from_secs(3), output_stream_complete.cancelled())
        .await
        .expect("active output should drain");
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= Duration::from_millis(2_250) && elapsed < Duration::from_millis(2_500),
        "active output should extend the close timeout: {elapsed:?}"
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        b"AB"
    );

    Ok(())
}

#[tokio::test]
async fn streaming_output_has_absolute_post_exit_deadline() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        ..
    } = streaming_output_harness(/*initial_output*/ None).await?;
    let output_stream_complete = process.output_stream_completion();

    tokio::time::pause();
    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    tokio::spawn(async move {
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if stdout_tx.send(b"x".to_vec()).is_err() {
                break;
            }
        }
    });

    tokio::time::timeout(
        TRAILING_OUTPUT_HARD_TIMEOUT + Duration::from_secs(2),
        output_stream_complete.cancelled(),
    )
    .await
    .expect("absolute output close timeout should finish");
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= TRAILING_OUTPUT_HARD_TIMEOUT
            && elapsed <= TRAILING_OUTPUT_HARD_TIMEOUT + Duration::from_millis(10),
        "active inherited output should not extend the hard close timeout: {elapsed:?}"
    );
    assert!(
        process
            .output_transcript()
            .lock()
            .await
            .to_bytes_with_omission_marker()
            .ends_with(INCOMPLETE_OUTPUT_WARNING)
    );

    Ok(())
}

#[tokio::test]
async fn streaming_output_marks_incomplete_output_when_close_times_out() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        rx_event,
        ..
    } = streaming_output_harness(/*initial_output*/ None).await?;
    let output_stream_complete = process.output_stream_completion();

    tokio::time::pause();
    let exited_at = Instant::now();
    stdout_tx.send(vec![0xc3]).expect("send UTF-8 lead byte");
    exit_tx.send(0).expect("send exit");
    tokio::time::timeout(Duration::from_secs(2), output_stream_complete.cancelled())
        .await
        .expect("output close timeout should finish");
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= TRAILING_OUTPUT_CLOSE_TIMEOUT
            && elapsed <= TRAILING_OUTPUT_CLOSE_TIMEOUT + Duration::from_millis(10),
        "missing output close should use the close-timeout fallback: {elapsed:?}"
    );
    let mut expected_output = vec![0xc3];
    expected_output.extend_from_slice(INCOMPLETE_OUTPUT_WARNING);
    assert_eq!(
        process
            .output_transcript()
            .lock()
            .await
            .to_bytes_with_omission_marker(),
        expected_output
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        expected_output
    );
    let event = rx_event.try_recv().expect("receive final output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "streaming-output-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: vec![0xc3],
        }
    );
    assert!(rx_event.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn exit_watcher_waits_for_late_network_denial_before_classifying_end() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        context,
        rx_event,
    } = streaming_output_harness(/*initial_output*/ None).await?;

    tokio::time::pause();
    let process_for_late_denial = Arc::clone(&process);
    let (late_denial_armed_tx, late_denial_armed_rx) = tokio::sync::oneshot::channel();
    let network_denial_monitor = tokio::spawn(async move {
        let sleep = tokio::time::sleep(Duration::from_millis(10));
        tokio::pin!(sleep);
        late_denial_armed_tx.send(()).expect("arm late denial");
        sleep.await;
        process_for_late_denial.fail_and_terminate("LATE_DENIAL".to_string());
    });
    tokio::time::timeout(Duration::from_secs(1), late_denial_armed_rx)
        .await
        .expect("late denial should arm")
        .expect("late denial armed");

    #[allow(deprecated)]
    let cwd = context.step_context.turn.cwd.clone().into();
    spawn_exit_watcher(
        Arc::clone(&process),
        Arc::clone(&context.session),
        Arc::clone(&context.step_context.turn),
        context.call_id,
        vec!["proof".to_string()],
        cwd,
        /*process_id*/ 123,
        /*plugin_attribution*/ None,
        transcript,
        Instant::now(),
        Some(network_denial_monitor),
        /*plugin_metrics_sidecar*/ None,
    );

    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    drop(stdout_tx);

    let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
        .await
        .expect("command should complete")
        .expect("command end event");
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();
    let EventMsg::ItemCompleted(completed) = event.msg else {
        panic!("expected ItemCompleted");
    };
    let TurnItem::CommandExecution(item) = completed.item else {
        panic!("expected CommandExecution");
    };
    assert_eq!(
        (
            item.status,
            item.exit_code,
            item.aggregated_output.as_deref()
        ),
        (
            CommandExecutionStatus::Failed,
            Some(-1),
            Some("LATE_DENIAL")
        )
    );
    assert!(
        elapsed >= Duration::from_millis(10) && elapsed < TRAILING_OUTPUT_CLOSE_TIMEOUT,
        "completion should wait for denial without falling back to the output close timeout: {elapsed:?}"
    );

    Ok(())
}

#[test]
fn utf8_boundary_preserves_complete_characters() {
    assert_eq!(utf8_boundary(b"hello"), 5);

    let bytes = "ééé".as_bytes();
    assert_eq!(utf8_boundary(&bytes[..3]), 2);

    let bytes = "😀".as_bytes();
    assert_eq!(utf8_boundary(bytes), bytes.len());
    for len in 1..bytes.len() {
        assert_eq!(utf8_boundary(&bytes[..len]), 0);
    }
    assert_eq!(utf8_boundary(&[0xf0, 0x9f, 0x98, 0x80, 0xc3]), 4);
}

#[test]
fn utf8_boundary_batches_malformed_output() {
    assert_eq!(utf8_boundary(&[0xff, b'a', b'b']), 3);
    assert_eq!(utf8_boundary(&[0xff, 0xc3]), 1);
    assert_eq!(utf8_boundary(&[0xff, 0xc3, 0xa9]), 3);
    assert_eq!(utf8_boundary(&[0xe0, 0x80]), 2);

    assert_eq!(utf8_boundary(b"a\xffbbb"), 5);
}

#[tokio::test]
async fn streaming_output_bounds_invalid_bytes() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let mut output = Buffer::<8> {
        pending: Vec::new(),
        emitter: Emitter {
            remaining_deltas: 2,
            session,
            turn,
            call_id: "bounded-output-test".to_string(),
        },
    };

    // The first frame splits 😀; the last allowed frame leaves é incomplete.
    let bytes = b"\xff\xff\xff\xff\xff\xff\xf0\x9f\x98\x80\xff\xff\xff\xc3\xa9";
    output.push(bytes.to_vec()).await;
    output.push(vec![0xfe, 0xfe]).await;
    output.finish().await;

    let mut chunks = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
            panic!("expected ExecCommandOutputDelta");
        };
        chunks.push(delta.chunk);
    }
    assert_eq!(
        chunks,
        vec![
            b"\xff\xff\xff\xff\xff\xff".to_vec(),
            b"\xf0\x9f\x98\x80\xff\xff\xff".to_vec(),
        ]
    );
}
