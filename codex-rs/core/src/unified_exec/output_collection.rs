use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::turn_timing::now_unix_timestamp_ms;
use crate::unified_exec::HeadTailBuffer;
use crate::unified_exec::UserInputWait;
use crate::unified_exec::WriteStdinInteractionEvent;
use crate::unified_exec::process::OutputHandles;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_protocol::protocol::TerminalWaitCompletionReason;
use codex_protocol::protocol::TerminalWaitEvent;
use codex_protocol::protocol::TerminalWaitMode;

pub(super) struct TerminalWaitReporter<'event, 'request> {
    event: &'event WriteStdinInteractionEvent<'request>,
    command_call_id: String,
    process_id: i32,
    input: &'request str,
    started_at: Instant,
    started_at_ms: i64,
    mode: TerminalWaitMode,
    deadline_at_ms: Option<i64>,
}

impl<'event, 'request> TerminalWaitReporter<'event, 'request> {
    pub(super) fn new(
        event: &'event WriteStdinInteractionEvent<'request>,
        command_call_id: String,
        process_id: i32,
        input: &'request str,
        mode: TerminalWaitMode,
        deadline_at_ms: Option<i64>,
    ) -> Self {
        Self {
            event,
            command_call_id,
            process_id,
            input,
            started_at: Instant::now(),
            started_at_ms: now_unix_timestamp_ms(),
            mode,
            deadline_at_ms,
        }
    }

    pub(super) async fn started(&self) {
        self.event
            .session
            .send_event(
                self.event.turn.as_ref(),
                EventMsg::TerminalInteraction(TerminalInteractionEvent {
                    call_id: self.command_call_id.clone(),
                    process_id: self.process_id.to_string(),
                    stdin: String::new(),
                    deadline_at_ms: self.deadline_at_ms,
                    wait: Some(TerminalWaitEvent::Started {
                        interaction_id: self.event.interaction_id.to_string(),
                        started_at_ms: self.started_at_ms,
                        mode: self.mode,
                    }),
                }),
            )
            .await;
    }

    pub(super) async fn finished(&self, reason: TerminalWaitCompletionReason) {
        let elapsed_ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.event
            .session
            .send_event(
                self.event.turn.as_ref(),
                EventMsg::TerminalInteraction(TerminalInteractionEvent {
                    call_id: self.command_call_id.clone(),
                    process_id: self.process_id.to_string(),
                    stdin: self.input.to_string(),
                    deadline_at_ms: None,
                    wait: Some(TerminalWaitEvent::Finished {
                        interaction_id: self.event.interaction_id.to_string(),
                        elapsed_ms,
                        reason,
                    }),
                }),
            )
            .await;
    }
}

pub(super) struct OutputCollection<const MAX_BYTES: usize> {
    pub collected: HeadTailBuffer<MAX_BYTES>,
    pub completion_reason: TerminalWaitCompletionReason,
}

pub(super) struct OutputCollectionInterrupts {
    pub cancellation_token: CancellationToken,
    pub user_input_wait: Option<UserInputWait>,
}

pub(super) async fn collect_output_until_deadline<const MAX_BYTES: usize>(
    output: &OutputHandles<MAX_BYTES>,
    mut pause_state: Option<watch::Receiver<bool>>,
    mut deadline: Option<Instant>,
    mut interrupts: Option<OutputCollectionInterrupts>,
) -> OutputCollection<MAX_BYTES> {
    const POST_EXIT_CLOSE_WAIT_CAP: Duration = Duration::from_secs(1);

    let OutputHandles {
        output_buffer,
        output_notify,
        output_closed,
        output_closed_notify,
        cancellation_token,
    } = output;
    let mut collected = HeadTailBuffer::default();
    let mut completion_reason = if cancellation_token.is_cancelled() {
        TerminalWaitCompletionReason::Exited
    } else {
        TerminalWaitCompletionReason::Timeout
    };
    let mut exit_signal_received = cancellation_token.is_cancelled();
    let mut post_exit_deadline: Option<Instant> = None;

    'collect: loop {
        if let Some(reason) = wait_for_interrupt_while_paused(
            &mut pause_state,
            &mut deadline,
            &mut post_exit_deadline,
            &mut interrupts,
        )
        .await
        {
            completion_reason = reason;
            break;
        }

        let drained_output: HeadTailBuffer<MAX_BYTES>;
        let has_drained_output: bool;
        let mut wait_for_output = None;
        {
            let mut guard = output_buffer.lock().await;
            drained_output = std::mem::take(&mut *guard);
            has_drained_output =
                drained_output.retained_bytes() > 0 || drained_output.omitted_bytes() > 0;
            if !has_drained_output {
                wait_for_output = Some(output_notify.notified());
            }
        }

        collected.push_buffer(drained_output);

        exit_signal_received |= cancellation_token.is_cancelled();
        if exit_signal_received {
            completion_reason = TerminalWaitCompletionReason::Exited;
            if output_closed.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }

            let now = Instant::now();
            let close_wait_deadline = *post_exit_deadline.get_or_insert_with(|| {
                let remaining = deadline
                    .map(|deadline| deadline.saturating_duration_since(now))
                    .unwrap_or(POST_EXIT_CLOSE_WAIT_CAP);
                now + if remaining == Duration::ZERO {
                    POST_EXIT_CLOSE_WAIT_CAP
                } else {
                    remaining.min(POST_EXIT_CLOSE_WAIT_CAP)
                }
            });
            let close_wait_remaining = close_wait_deadline.saturating_duration_since(now);
            if close_wait_remaining == Duration::ZERO {
                break;
            }
            if has_drained_output {
                continue;
            }
            let notified = wait_for_output.unwrap_or_else(|| output_notify.notified());
            let closed = output_closed_notify.notified();
            tokio::pin!(notified);
            tokio::pin!(closed);
            tokio::select! {
                _ = &mut notified => {}
                _ = &mut closed => {}
                _ = tokio::time::sleep(close_wait_remaining) => break,
                _ = wait_for_pause_change(pause_state.as_ref()) => {}
            }
            continue;
        }

        if let Some(reason) = take_ready_interrupt(&mut interrupts) {
            completion_reason = reason;
            break;
        }

        if has_drained_output {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                completion_reason = TerminalWaitCompletionReason::Timeout;
                break;
            }
            continue;
        }

        let notified = wait_for_output.unwrap_or_else(|| output_notify.notified());
        tokio::pin!(notified);
        let exit_notified = cancellation_token.cancelled();
        tokio::pin!(exit_notified);
        if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining == Duration::ZERO {
                completion_reason = TerminalWaitCompletionReason::Timeout;
                break;
            }
            if interrupts.is_some() {
                let wait_for_interrupt = wait_for_interrupt(&mut interrupts);
                tokio::pin!(wait_for_interrupt);
                tokio::select! {
                    _ = &mut exit_notified => {
                        exit_signal_received = true;
                    }
                    reason = &mut wait_for_interrupt => {
                        completion_reason = reason;
                        break 'collect;
                    }
                    _ = &mut notified => {}
                    _ = tokio::time::sleep(remaining) => {
                        completion_reason = TerminalWaitCompletionReason::Timeout;
                        break;
                    }
                    _ = wait_for_pause_change(pause_state.as_ref()) => {}
                }
            } else {
                tokio::select! {
                    _ = &mut notified => {}
                    _ = &mut exit_notified => exit_signal_received = true,
                    _ = tokio::time::sleep(remaining) => break,
                    _ = wait_for_pause_change(pause_state.as_ref()) => {}
                }
            }
        } else if interrupts.is_some() {
            let wait_for_interrupt = wait_for_interrupt(&mut interrupts);
            tokio::pin!(wait_for_interrupt);
            tokio::select! {
                _ = &mut exit_notified => {
                    exit_signal_received = true;
                }
                reason = &mut wait_for_interrupt => {
                    completion_reason = reason;
                    break 'collect;
                }
                _ = &mut notified => {}
                _ = wait_for_pause_change(pause_state.as_ref()) => {}
            }
        } else {
            tokio::select! {
                _ = &mut notified => {}
                _ = &mut exit_notified => exit_signal_received = true,
                _ = wait_for_pause_change(pause_state.as_ref()) => {}
            }
        }
    }

    {
        let mut guard = output_buffer.lock().await;
        collected.push_buffer(guard.drain());
    }

    OutputCollection {
        collected,
        completion_reason,
    }
}

async fn wait_for_interrupt_while_paused(
    pause_state: &mut Option<watch::Receiver<bool>>,
    deadline: &mut Option<Instant>,
    post_exit_deadline: &mut Option<Instant>,
    interrupts: &mut Option<OutputCollectionInterrupts>,
) -> Option<TerminalWaitCompletionReason> {
    let Some(receiver) = pause_state.as_mut() else {
        return take_ready_interrupt(interrupts);
    };
    if !*receiver.borrow() {
        return take_ready_interrupt(interrupts);
    }

    let paused_at = Instant::now();
    while *receiver.borrow() {
        let wait_for_interrupt = wait_for_interrupt(interrupts);
        tokio::pin!(wait_for_interrupt);
        tokio::select! {
            biased;
            reason = &mut wait_for_interrupt => return Some(reason),
            changed = receiver.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }

    let paused_for = paused_at.elapsed();
    if let Some(deadline) = deadline.as_mut()
        && let Some(extended_deadline) = deadline.checked_add(paused_for)
    {
        *deadline = extended_deadline;
    }
    if let Some(post_exit_deadline) = post_exit_deadline.as_mut()
        && let Some(extended_deadline) = post_exit_deadline.checked_add(paused_for)
    {
        *post_exit_deadline = extended_deadline;
    }

    take_ready_interrupt(interrupts)
}

fn take_ready_interrupt(
    interrupts: &mut Option<OutputCollectionInterrupts>,
) -> Option<TerminalWaitCompletionReason> {
    let interrupts = interrupts.as_mut()?;
    if let Some(user_input_wait) = interrupts.user_input_wait.as_mut() {
        if user_input_wait.pending_steer {
            return Some(TerminalWaitCompletionReason::Input);
        }
        if matches!(user_input_wait.steer_activity_rx.has_changed(), Ok(true)) {
            let _ = user_input_wait.steer_activity_rx.borrow_and_update();
            return Some(TerminalWaitCompletionReason::Input);
        }
    }
    interrupts
        .cancellation_token
        .is_cancelled()
        .then_some(TerminalWaitCompletionReason::Cancelled)
}

async fn wait_for_interrupt(
    interrupts: &mut Option<OutputCollectionInterrupts>,
) -> TerminalWaitCompletionReason {
    let Some(interrupts) = interrupts.as_mut() else {
        return std::future::pending().await;
    };

    tokio::select! {
        biased;
        _ = wait_for_user_input(interrupts.user_input_wait.as_mut()) => {
            TerminalWaitCompletionReason::Input
        }
        _ = interrupts.cancellation_token.cancelled() => {
            TerminalWaitCompletionReason::Cancelled
        }
    }
}

async fn wait_for_user_input(user_input_wait: Option<&mut UserInputWait>) {
    let Some(user_input_wait) = user_input_wait else {
        return std::future::pending().await;
    };

    if user_input_wait.pending_steer {
        return;
    }

    if user_input_wait.steer_activity_rx.changed().await.is_err() {
        return std::future::pending().await;
    }
}

async fn wait_for_pause_change(pause_state: Option<&watch::Receiver<bool>>) {
    match pause_state {
        Some(pause_state) => {
            let mut receiver = pause_state.clone();
            let _ = receiver.changed().await;
        }
        None => std::future::pending::<()>().await,
    }
}
