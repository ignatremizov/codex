use std::sync::Arc;

use super::INCOMPLETE_OUTPUT_WARNING;
use super::TRAILING_OUTPUT_CLOSE_TIMEOUT;
use super::TRAILING_OUTPUT_HARD_TIMEOUT;
use super::spawn_exit_watcher;
use super::split_valid_utf8_prefix_with_max;
use super::start_streaming_output;
use crate::session::tests::make_session_and_context_with_rx;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::NoopSpawnLifecycle;
use crate::unified_exec::process::UnifiedExecProcess;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
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
    let context = UnifiedExecContext::new(session, turn, "streaming-output-test".to_string());
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
    let cwd = context.turn.cwd.clone().into();
    spawn_exit_watcher(
        Arc::clone(&process),
        Arc::clone(&context.session),
        Arc::clone(&context.turn),
        context.call_id,
        vec!["proof".to_string()],
        cwd,
        /*process_id*/ 123,
        /*plugin_attribution*/ None,
        transcript,
        Instant::now(),
        /*network_denial_monitor*/ None,
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
            .send(b"LATE-OUTPUT-MARKER".to_vec())
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
        b"LATE-OUTPUT-MARKER"
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
        stdout_tx: _stdout_tx,
        exit_tx,
        ..
    } = streaming_output_harness(/*initial_output*/ None).await?;
    let output_stream_complete = process.output_stream_completion();

    tokio::time::pause();
    let exited_at = Instant::now();
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
    assert_eq!(
        process
            .output_transcript()
            .lock()
            .await
            .to_bytes_with_omission_marker(),
        INCOMPLETE_OUTPUT_WARNING
    );

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
    let cwd = context.turn.cwd.clone().into();
    spawn_exit_watcher(
        Arc::clone(&process),
        Arc::clone(&context.session),
        Arc::clone(&context.turn),
        context.call_id,
        vec!["proof".to_string()],
        cwd,
        /*process_id*/ 123,
        /*plugin_attribution*/ None,
        transcript,
        Instant::now(),
        Some(network_denial_monitor),
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
fn split_valid_utf8_prefix_respects_max_bytes_for_ascii() {
    let mut buf = b"hello word!".to_vec();

    let first =
        split_valid_utf8_prefix_with_max(&mut buf, /*max_bytes*/ 5).expect("expected prefix");
    assert_eq!(first, b"hello".to_vec());
    assert_eq!(buf, b" word!".to_vec());

    let second =
        split_valid_utf8_prefix_with_max(&mut buf, /*max_bytes*/ 5).expect("expected prefix");
    assert_eq!(second, b" word".to_vec());
    assert_eq!(buf, b"!".to_vec());
}

#[test]
fn split_valid_utf8_prefix_avoids_splitting_utf8_codepoints() {
    // "é" is 2 bytes in UTF-8. With a max of 3 bytes, we should only emit 1 char (2 bytes).
    let mut buf = "ééé".as_bytes().to_vec();

    let first =
        split_valid_utf8_prefix_with_max(&mut buf, /*max_bytes*/ 3).expect("expected prefix");
    assert_eq!(std::str::from_utf8(&first).unwrap(), "é");
    assert_eq!(buf, "éé".as_bytes().to_vec());
}

#[test]
fn split_valid_utf8_prefix_makes_progress_on_invalid_utf8() {
    let mut buf = vec![0xff, b'a', b'b'];

    let first =
        split_valid_utf8_prefix_with_max(&mut buf, /*max_bytes*/ 2).expect("expected prefix");
    assert_eq!(first, vec![0xff]);
    assert_eq!(buf, b"ab".to_vec());
}
