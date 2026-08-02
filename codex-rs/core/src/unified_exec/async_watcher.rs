use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::Instant;

use super::SharedPluginMetricsSidecar;
use super::UnifiedExecContext;
use super::process::OutputHandles;
use super::process::UnifiedExecProcess;
use super::take_plugin_metrics_sidecar;
use crate::exec::MAX_EXEC_OUTPUT_DELTAS_PER_CALL;
use crate::plugins::metrics::finish_and_track_measurements;
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

struct Emitter {
    remaining_deltas: usize,
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: String,
}

struct Buffer<const MAX_BYTES: usize = UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES> {
    pending: Vec<u8>,
    emitter: Emitter,
}

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
    } = process.output_handles().clone();
    let output_transcript = process.output_transcript();

    let emitter = Emitter {
        remaining_deltas: MAX_EXEC_OUTPUT_DELTAS_PER_CALL,
        session: Arc::clone(&context.session),
        turn: Arc::clone(&context.step_context.turn),
        call_id: context.call_id.clone(),
    };

    tokio::spawn(async move {
        let mut output: Buffer = Buffer {
            pending: Vec::new(),
            emitter,
        };
        let _output_stream_completion_guard = output_stream_complete.drop_guard();

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

                    let delivery = output.push(chunk);
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
                let delivery = output.push(chunk);
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

        let finish = output.finish();
        match hard_close_at {
            Some(hard_close_at) => {
                let _ = tokio::time::timeout_at(hard_close_at, finish).await;
            }
            None => finish.await,
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
                .push_chunk(INCOMPLETE_OUTPUT_WARNING);
            output_transcript
                .lock()
                .await
                .push_chunk(INCOMPLETE_OUTPUT_WARNING);
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
    plugin_metrics_sidecar: Option<SharedPluginMetricsSidecar>,
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
        let plugin_metrics_sidecar = plugin_metrics_sidecar
            .as_ref()
            .and_then(take_plugin_metrics_sidecar);
        if let Some(message) = process.failure_message() {
            drop(plugin_metrics_sidecar);
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
            let timed_out = process.timed_out();
            finish_and_track_measurements(
                plugin_metrics_sidecar,
                exit_code,
                &session_ref,
                &turn_ref,
                &call_id,
            )
            .await;
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
                timed_out,
            )
            .await;
        }
    });
}

impl<const MAX_BYTES: usize> Buffer<MAX_BYTES> {
    async fn push(&mut self, mut bytes: Vec<u8>) {
        const {
            assert!(
                MAX_BYTES >= char::MAX.len_utf8(),
                "a frame must fit one UTF-8 scalar"
            )
        };
        let Self { pending, emitter } = self;

        // Reuse a producer chunk when it fits, retaining only an incomplete
        // UTF-8 suffix for the next push.
        if pending.is_empty() && bytes.len() <= MAX_BYTES {
            emitter
                .emit(|| {
                    let complete = utf8_boundary(&bytes);
                    pending.extend(bytes.drain(complete..));
                    bytes
                })
                .await;
            return;
        }

        let mut bytes = bytes.as_slice();
        let mut next_chunk = || {
            let space = MAX_BYTES.saturating_sub(pending.len());
            let (prefix, rest) = bytes.split_at_checked(space).unwrap_or((bytes, &[]));
            let mut chunk = Vec::with_capacity(pending.len().saturating_add(prefix.len()));
            chunk.append(pending); // Empties pending.
            chunk.extend_from_slice(prefix);
            bytes = rest;

            let complete = utf8_boundary(&chunk);
            // Only the incomplete suffix passes through pending.
            pending.extend(chunk.drain(complete..));
            chunk
        };
        let mut emitted_frames = 0;
        while emitter.emit(&mut next_chunk).await {
            emitted_frames += 1;
            if emitted_frames % OUTPUT_DELIVERY_YIELD_INTERVAL == 0 {
                tokio::task::yield_now().await;
            }
        }
    }

    async fn finish(self) {
        let Self {
            pending,
            mut emitter,
        } = self;
        debug_assert!(
            pending.len() < char::MAX.len_utf8(),
            "only an incomplete UTF-8 scalar can remain"
        );
        emitter.emit(|| pending).await;
    }
}

impl Emitter {
    /// Build a frame only while quota remains. Returns whether a nonempty frame was sent.
    async fn emit(&mut self, make_chunk: impl FnOnce() -> Vec<u8>) -> bool {
        let Self {
            remaining_deltas,
            session,
            turn,
            call_id,
        } = self;
        let Some(remaining) = remaining_deltas.checked_sub(1) else {
            return false;
        };
        let chunk = make_chunk();
        let emit = !chunk.is_empty();
        if emit {
            let event = ExecCommandOutputDeltaEvent {
                call_id: call_id.clone(),
                stream: ExecOutputStream::Stdout,
                chunk,
            };
            session
                .send_event(turn.as_ref(), EventMsg::ExecCommandOutputDelta(event))
                .await;
            *remaining_deltas = remaining;
        }
        emit
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
    timed_out: bool,
) {
    let aggregated_output = resolve_aggregated_output(&transcript, fallback_output).await;
    let output = ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(aggregated_output.clone()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out,
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

/// Keep only a potentially incomplete UTF-8 suffix; malformed bytes remain raw.
/// A UTF-8 scalar spans at most four bytes, so its incomplete tail fits in three.
fn utf8_boundary(bytes: &[u8]) -> usize {
    let mut boundary = bytes.len().saturating_sub(char::MAX.len_utf8() - 1);
    while boundary < bytes.len() {
        match std::str::from_utf8(&bytes[boundary..]) {
            Ok(s) => return boundary + s.len(),
            Err(error) => {
                boundary += error.valid_up_to();
                if let Some(invalid_len) = error.error_len() {
                    boundary += invalid_len;
                } else {
                    return boundary;
                }
            }
        }
    }
    bytes.len()
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
