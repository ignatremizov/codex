use super::OutputCollectionInterrupts;
use super::collect_output_until_deadline;
use super::extend_deadline;
use super::wait_for_interrupt_while_paused;
use super::wait_for_pause_change;
use crate::unified_exec::UserInputWait;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::OutputHandles;
use codex_protocol::protocol::TerminalWaitCompletionReason;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn output_handles() -> OutputHandles<16> {
    OutputHandles {
        output_buffer: Arc::new(Mutex::new(HeadTailBuffer::default())),
        output_notify: Arc::new(Notify::new()),
        output_closed: Arc::new(AtomicBool::new(/*v*/ false)),
        output_closed_notify: Arc::new(Notify::new()),
        cancellation_token: CancellationToken::new(),
    }
}

#[test]
fn pause_deadline_extensions_use_platform_checked_arithmetic() {
    let start = Instant::now();
    for initial in [None, Some(start)] {
        for extension in [Duration::from_secs(/*secs*/ 1), Duration::MAX] {
            let mut deadline = initial;
            extend_deadline(&mut deadline, extension);
            assert_eq!(
                deadline,
                initial.and_then(|deadline| deadline.checked_add(extension))
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn paused_polling_extends_only_finite_deadlines() {
    for finite_deadline in [None, Some(Duration::from_secs(/*secs*/ 5))] {
        let start = Instant::now();
        let (sender, receiver) = watch::channel(/*init*/ true);
        let mut pause_state = Some(receiver);
        let mut deadline = finite_deadline.map(|wait| start + wait);
        let mut post_exit_deadline = Some(start + Duration::from_secs(/*secs*/ 1));
        {
            let mut interrupts = None;
            let paused = wait_for_interrupt_while_paused(
                &mut pause_state,
                &mut deadline,
                &mut post_exit_deadline,
                &mut interrupts,
            );
            tokio::pin!(paused);
            assert!(futures::poll!(&mut paused).is_pending());
            tokio::time::advance(Duration::from_secs(/*secs*/ 10)).await;
            assert!(futures::poll!(&mut paused).is_pending());
            sender.send_replace(/*value*/ false);
            assert_eq!(paused.await, None);
        }
        assert_eq!(
            (deadline, post_exit_deadline),
            (
                finite_deadline.map(|wait| start + wait + Duration::from_secs(/*secs*/ 10)),
                Some(start + Duration::from_secs(/*secs*/ 11)),
            )
        );
    }
}

#[tokio::test]
async fn pause_changes_are_consumed_and_closed_channels_are_retired() {
    let (sender, receiver) = watch::channel(/*init*/ false);
    let mut pause_state = Some(receiver);
    sender.send_replace(/*value*/ true);
    wait_for_pause_change(&mut pause_state).await;
    {
        let next_change = wait_for_pause_change(&mut pause_state);
        tokio::pin!(next_change);
        assert!(futures::poll!(&mut next_change).is_pending());
        sender.send_replace(/*value*/ false);
        next_change.await;
    }
    drop(sender);
    wait_for_pause_change(&mut pause_state).await;
    assert!(pause_state.is_none());
}

#[tokio::test(start_paused = true)]
async fn unbounded_wait_remains_unbounded_after_approval_pause() {
    let output = output_handles();
    let (sender, receiver) = watch::channel(/*init*/ true);
    let collect = collect_output_until_deadline(
        &output,
        Some(receiver),
        /*deadline*/ None,
        /*interrupts*/ None,
    );
    tokio::pin!(collect);
    assert!(futures::poll!(&mut collect).is_pending());
    tokio::time::advance(Duration::from_secs(/*secs*/ 10)).await;
    sender.send_replace(/*value*/ false);
    assert!(futures::poll!(&mut collect).is_pending());
    tokio::time::advance(Duration::from_secs(/*secs*/ 600)).await;
    assert!(futures::poll!(&mut collect).is_pending());
    output
        .output_buffer
        .lock()
        .await
        .push_chunk(b"final output");
    output.output_closed.store(true, Ordering::Release);
    output.cancellation_token.cancel();
    let result = collect.await;
    assert_eq!(
        (
            result.collected.to_bytes_with_omission_marker(),
            result.completion_reason,
        ),
        (
            b"final output".to_vec(),
            TerminalWaitCompletionReason::Exited
        )
    );
}

#[tokio::test(start_paused = true)]
async fn approval_pause_preserves_post_exit_close_budget() {
    let output = output_handles();
    output.cancellation_token.cancel();
    let (sender, receiver) = watch::channel(/*init*/ false);
    let collect = collect_output_until_deadline(
        &output,
        Some(receiver),
        /*deadline*/ None,
        /*interrupts*/ None,
    );
    tokio::pin!(collect);
    assert!(futures::poll!(&mut collect).is_pending());
    sender.send_replace(/*value*/ true);
    assert!(futures::poll!(&mut collect).is_pending());
    tokio::time::advance(Duration::from_secs(/*secs*/ 10)).await;
    sender.send_replace(/*value*/ false);
    assert!(futures::poll!(&mut collect).is_pending());
    output.output_buffer.lock().await.push_chunk(b"late output");
    output.output_notify.notify_one();
    assert!(futures::poll!(&mut collect).is_pending());
    tokio::time::advance(Duration::from_secs(/*secs*/ 1)).await;
    let result = collect.await;
    assert_eq!(
        (
            result.collected.to_bytes_with_omission_marker(),
            result.completion_reason,
        ),
        (
            b"late output".to_vec(),
            TerminalWaitCompletionReason::Exited
        )
    );
}

#[tokio::test]
async fn interruption_releases_a_paused_wait_without_signalling_process_exit() {
    for reason in [
        TerminalWaitCompletionReason::Input,
        TerminalWaitCompletionReason::Cancelled,
    ] {
        let output = output_handles();
        let (_sender, receiver) = watch::channel(/*init*/ true);
        let (steer, steer_activity_rx) = watch::channel(/*init*/ 0);
        let cancellation = CancellationToken::new();
        let collect = collect_output_until_deadline(
            &output,
            Some(receiver),
            /*deadline*/ None,
            Some(OutputCollectionInterrupts {
                cancellation_token: cancellation.clone(),
                user_input_wait: Some(UserInputWait {
                    steer_activity_rx,
                    pending_steer: false,
                }),
            }),
        );
        tokio::pin!(collect);
        assert!(futures::poll!(&mut collect).is_pending());
        if reason == TerminalWaitCompletionReason::Input {
            steer.send_replace(/*value*/ 1);
        } else {
            cancellation.cancel();
        }
        let result = collect.await;
        assert_eq!(result.completion_reason, reason);
        assert!(!output.cancellation_token.is_cancelled());
    }
}

#[tokio::test(start_paused = true)]
async fn closed_paused_channel_releases_the_finite_wait() {
    let output = output_handles();
    let (sender, receiver) = watch::channel(/*init*/ true);
    let collect = collect_output_until_deadline(
        &output,
        Some(receiver),
        Some(Instant::now() + Duration::from_secs(/*secs*/ 1)),
        /*interrupts*/ None,
    );
    tokio::pin!(collect);
    assert!(futures::poll!(&mut collect).is_pending());
    tokio::time::advance(Duration::from_secs(/*secs*/ 10)).await;
    drop(sender);
    assert!(futures::poll!(&mut collect).is_pending());
    tokio::time::advance(Duration::from_secs(/*secs*/ 1)).await;
    assert_eq!(
        collect.await.completion_reason,
        TerminalWaitCompletionReason::Timeout
    );
}
