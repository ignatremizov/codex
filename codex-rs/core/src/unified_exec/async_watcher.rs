use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::Instant;

use super::UnifiedExecContext;
use super::process::OutputHandles;
use super::process::UnifiedExecProcess;
use crate::exec::MAX_EXEC_OUTPUT_DELTAS_PER_CALL;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventFailure;
use crate::tools::events::ToolEventStage;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use codex_core_plugins::PluginCommandAttribution;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ExecOutputStream;
use codex_utils_path_uri::PathUri;

pub(crate) const TRAILING_OUTPUT_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
pub(crate) const TRAILING_OUTPUT_HARD_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const INCOMPLETE_OUTPUT_WARNING: &[u8] =
    b"\nWarning: the process output stream did not close; trailing output may be missing.\n";

/// Upper bound for a single ExecCommandOutputDelta chunk emitted by unified exec.
///
/// The unified exec output buffer already caps *retained* output (see
/// `UNIFIED_EXEC_OUTPUT_MAX_BYTES`), but we also cap per-event payload size so
/// downstream event consumers (especially app-server JSON-RPC) don't have to
/// process arbitrarily large delta payloads.
const UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES: usize = 8192;
const OUTPUT_DELIVERY_YIELD_INTERVAL: usize = 64;

/// Spawn a background task that continuously reads from the PTY and emits
/// ExecCommandOutputDelta events on UTF‑8 boundaries.
pub(crate) fn start_streaming_output(
    process: &Arc<UnifiedExecProcess>,
    context: &UnifiedExecContext,
) {
    let output_stream_complete = process.output_stream_completion();
    let Some(mut receiver) = process.take_output_receiver() else {
        output_stream_complete.cancel();
        return;
    };
    let output_task_abort_handle = process.output_task_abort_handle();
    let exit_token = process.cancellation_token();
    let OutputHandles {
        output_buffer,
        output_notify,
        output_closed,
        output_closed_notify,
        ..
    } = process.output_handles();
    let output_transcript = process.output_transcript();

    let session_ref = Arc::clone(&context.session);
    let turn_ref = Arc::clone(&context.turn);
    let call_id = context.call_id.clone();

    tokio::spawn(async move {
        let _output_stream_completion_guard = output_stream_complete.drop_guard();
        let mut pending = Vec::<u8>::new();
        let mut emitted_deltas: usize = 0;

        let output_closed_notified = output_closed_notify.notified();
        tokio::pin!(output_closed_notified);
        let close_deadline = tokio::time::sleep(TRAILING_OUTPUT_CLOSE_TIMEOUT);
        tokio::pin!(close_deadline);
        let hard_close_deadline = tokio::time::sleep(TRAILING_OUTPUT_HARD_TIMEOUT);
        tokio::pin!(hard_close_deadline);
        let mut waiting_for_output_close = false;
        let mut hard_close_at = None;
        let mut output_incomplete = false;

        loop {
            // Register before checking the atomic so a close between the check
            // and the select cannot miss the notification.
            output_closed_notified.as_mut().enable();
            if waiting_for_output_close && output_closed.load(Ordering::Acquire) {
                break;
            }

            tokio::select! {
                biased;

                _ = exit_token.cancelled(), if !waiting_for_output_close => {
                    waiting_for_output_close = true;
                    let now = Instant::now();
                    let hard_deadline = now + TRAILING_OUTPUT_HARD_TIMEOUT;
                    hard_close_at = Some(hard_deadline);
                    close_deadline.as_mut().reset(
                        now + TRAILING_OUTPUT_CLOSE_TIMEOUT,
                    );
                    hard_close_deadline.as_mut().reset(hard_deadline);
                }

                _ = &mut hard_close_deadline, if waiting_for_output_close => {
                    output_incomplete = !output_closed.load(Ordering::Acquire);
                    break;
                }

                _ = &mut close_deadline, if waiting_for_output_close => {
                    output_incomplete = !output_closed.load(Ordering::Acquire);
                    break;
                }

                _ = &mut output_closed_notified, if waiting_for_output_close => {
                    output_closed_notified.set(output_closed_notify.notified());
                }

                received = receiver.recv() => {
                    let Some(chunk) = received else {
                        break;
                    };

                    let delivery = process_chunk(
                        &mut pending,
                        &call_id,
                        &session_ref,
                        &turn_ref,
                        &mut emitted_deltas,
                        chunk,
                    );
                    tokio::pin!(delivery);
                    if waiting_for_output_close {
                        close_deadline.as_mut().reset(
                            Instant::now() + TRAILING_OUTPUT_CLOSE_TIMEOUT,
                        );
                        tokio::select! {
                            biased;

                            _ = &mut hard_close_deadline => {
                                output_incomplete = !output_closed.load(Ordering::Acquire);
                                break;
                            }

                            _ = &mut close_deadline => {
                                output_incomplete = !output_closed.load(Ordering::Acquire);
                                break;
                            }

                            _ = &mut delivery => {}
                        }
                    } else {
                        tokio::select! {
                            biased;

                            _ = exit_token.cancelled() => {
                                waiting_for_output_close = true;
                                let now = Instant::now();
                                let hard_deadline = now + TRAILING_OUTPUT_HARD_TIMEOUT;
                                hard_close_at = Some(hard_deadline);
                                close_deadline.as_mut().reset(
                                    now + TRAILING_OUTPUT_CLOSE_TIMEOUT,
                                );
                                hard_close_deadline.as_mut().reset(hard_deadline);
                            }

                            _ = &mut delivery => {}
                        }
                    }
                }
            }
        }

        if !output_incomplete {
            // A closed producer can leave bounded chunks queued for live delta
            // delivery. The transcript already contains those bytes, so stop
            // flushing deltas at the absolute post-exit deadline rather than
            // delaying the terminal event indefinitely.
            while let Ok(chunk) = receiver.try_recv() {
                let delivery = process_chunk(
                    &mut pending,
                    &call_id,
                    &session_ref,
                    &turn_ref,
                    &mut emitted_deltas,
                    chunk,
                );
                match hard_close_at {
                    Some(hard_close_at) => {
                        if tokio::time::timeout_at(hard_close_at, delivery)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => delivery.await,
                }
            }
        }

        if output_incomplete {
            if let Some(output_task_abort_handle) = output_task_abort_handle {
                output_task_abort_handle.abort();
                let output_closed_wait = output_closed_notify.notified();
                tokio::pin!(output_closed_wait);
                output_closed_wait.as_mut().enable();
                if !output_closed.load(Ordering::Acquire) {
                    let _ = tokio::time::timeout(Duration::from_secs(1), output_closed_wait).await;
                }
            }
            output_buffer
                .lock()
                .await
                .push_chunk(INCOMPLETE_OUTPUT_WARNING.to_vec());
            output_transcript
                .lock()
                .await
                .push_chunk(INCOMPLETE_OUTPUT_WARNING.to_vec());
            output_notify.notify_waiters();
        }
    });
}

/// Spawn a background watcher that waits for the PTY to exit and then emits a
/// single ExecCommandEnd event with the aggregated transcript.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_exit_watcher(
    process: Arc<UnifiedExecProcess>,
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    process_id: i32,
    plugin_attribution: Option<PluginCommandAttribution>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    started_at: Instant,
    network_denial_monitor: Option<tokio::task::JoinHandle<()>>,
) {
    let exit_token = process.cancellation_token();
    let output_stream_complete = process.output_stream_completion();
    let interaction_lock = process.interaction_lock();

    tokio::spawn(async move {
        exit_token.cancelled().await;
        output_stream_complete.cancelled().await;
        // Deferred network denial deliberately remains observable for a short
        // window after process exit. Do not classify the terminal event until
        // that monitor has settled, even when output closes immediately.
        if let Some(network_denial_monitor) = network_denial_monitor {
            let _ = network_denial_monitor.await;
        }
        let _interaction_guard = interaction_lock.lock_owned().await;

        let duration = Instant::now().saturating_duration_since(started_at);
        if let Some(message) = process.failure_message() {
            emit_failed_exec_end_for_unified_exec(
                session_ref,
                turn_ref,
                call_id,
                command,
                cwd,
                Some(process_id.to_string()),
                plugin_attribution,
                transcript,
                String::new(),
                message,
                duration,
            )
            .await;
        } else {
            let exit_code = process.exit_code().unwrap_or(-1);
            emit_exec_end_for_unified_exec(
                session_ref,
                turn_ref,
                call_id,
                command,
                cwd,
                Some(process_id.to_string()),
                plugin_attribution,
                transcript,
                String::new(),
                exit_code,
                duration,
            )
            .await;
        }
    });
}

async fn process_chunk(
    pending: &mut Vec<u8>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &mut usize,
    chunk: Vec<u8>,
) {
    pending.extend_from_slice(&chunk);
    let mut processed_prefixes: usize = 0;
    while let Some(prefix) = split_valid_utf8_prefix(pending) {
        if *emitted_deltas < MAX_EXEC_OUTPUT_DELTAS_PER_CALL {
            let event = ExecCommandOutputDeltaEvent {
                call_id: call_id.to_string(),
                stream: ExecOutputStream::Stdout,
                chunk: prefix,
            };
            session_ref
                .send_event(turn_ref.as_ref(), EventMsg::ExecCommandOutputDelta(event))
                .await;
            *emitted_deltas += 1;
        }

        processed_prefixes += 1;
        if processed_prefixes.is_multiple_of(OUTPUT_DELIVERY_YIELD_INTERVAL) {
            tokio::task::yield_now().await;
        }
    }
}

/// Emit an ExecCommandEnd event for a unified exec session, using the complete
/// initial-call output when available and otherwise the streaming transcript.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_exec_end_for_unified_exec(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    process_id: Option<String>,
    plugin_attribution: Option<PluginCommandAttribution>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    fallback_output: String,
    exit_code: i32,
    duration: Duration,
) {
    let aggregated_output = resolve_aggregated_output(&transcript, fallback_output).await;
    let output = ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(aggregated_output.clone()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out: false,
    };
    let event_ctx = ToolEventCtx::new(
        session_ref.as_ref(),
        turn_ref.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    let emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        ExecCommandSource::UnifiedExecStartup,
        process_id,
        plugin_attribution,
        /*deadline_at_ms*/ None,
    );
    emitter
        .emit(
            event_ctx,
            ToolEventStage::Success {
                output,
                applied_patch_delta: None,
            },
        )
        .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_failed_exec_end_for_unified_exec(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    process_id: Option<String>,
    plugin_attribution: Option<PluginCommandAttribution>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    fallback_output: String,
    message: String,
    duration: Duration,
) {
    let stdout = if fallback_output.is_empty() {
        resolve_aggregated_output(&transcript, fallback_output).await
    } else {
        fallback_output
    };
    let aggregated_output = if stdout.is_empty() {
        message.clone()
    } else {
        format!("{stdout}\n{message}")
    };
    let output = ExecToolCallOutput {
        exit_code: -1,
        stdout: StreamOutput::new(stdout),
        stderr: StreamOutput::new(message),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out: false,
    };
    let event_ctx = ToolEventCtx::new(
        session_ref.as_ref(),
        turn_ref.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    let emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        ExecCommandSource::UnifiedExecStartup,
        process_id,
        plugin_attribution,
        /*deadline_at_ms*/ None,
    );
    emitter
        .emit(
            event_ctx,
            ToolEventStage::Failure(ToolEventFailure::Output(output)),
        )
        .await;
}

fn split_valid_utf8_prefix(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    split_valid_utf8_prefix_with_max(buffer, UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES)
}

fn split_valid_utf8_prefix_with_max(buffer: &mut Vec<u8>, max_bytes: usize) -> Option<Vec<u8>> {
    if buffer.is_empty() {
        return None;
    }

    let max_len = buffer.len().min(max_bytes);
    let mut split = max_len;
    while split > 0 {
        if std::str::from_utf8(&buffer[..split]).is_ok() {
            let prefix = buffer[..split].to_vec();
            buffer.drain(..split);
            return Some(prefix);
        }

        if max_len - split > 4 {
            break;
        }
        split -= 1;
    }

    // If no valid UTF-8 prefix was found, emit the first byte so the stream
    // keeps making progress and the transcript reflects all bytes.
    let byte = buffer.drain(..1).collect();
    Some(byte)
}

async fn resolve_aggregated_output(
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    fallback: String,
) -> String {
    if !fallback.is_empty() {
        return fallback;
    }

    let guard = transcript.lock().await;
    String::from_utf8_lossy(&guard.to_bytes_with_omission_marker()).to_string()
}

#[cfg(test)]
#[path = "async_watcher_tests.rs"]
mod tests;
