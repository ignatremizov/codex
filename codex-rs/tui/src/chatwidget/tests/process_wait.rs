use super::*;
use codex_app_server_protocol::TerminalInteractionNotification;
use codex_app_server_protocol::TerminalWait;
use codex_app_server_protocol::TerminalWaitCompletionReason;
use codex_app_server_protocol::TerminalWaitMode;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[tokio::test]
async fn concurrent_process_waits_keep_identity_when_one_exits() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    let _ = drain_insert_history(&mut rx);
    track_process(&mut chat, "call-build", "p1", "cargo build");
    track_process(&mut chat, "call-test", "p2", "cargo test");

    let now_ms = unix_timestamp_ms();
    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w1",
            item_id: "call-build",
            process_id: "p1",
            mode: TerminalWaitMode::UntilExit,
            started_at_ms: now_ms.saturating_sub(133_000),
            deadline_at_ms: None,
        },
    );
    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w2",
            item_id: "call-test",
            process_id: "p2",
            mode: TerminalWaitMode::Timed,
            started_at_ms: now_ms.saturating_sub(33_000),
            deadline_at_ms: Some(now_ms.saturating_add(12_000)),
        },
    );
    chat.refresh_unified_exec_wait_status_at(now_ms);
    chat.bottom_pane.reset_status_timer(Duration::ZERO);

    let concurrent = normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 100));
    assert!(
        concurrent.contains("Waiting on 2 processes")
            && concurrent.contains("cargo build")
            && concurrent.contains("cargo test")
            && concurrent.contains("process p1")
            && concurrent.contains("process p2")
            && concurrent.contains("wait w1")
            && concurrent.contains("wait w2")
            && concurrent.contains("Until exit · 2m 13s elapsed")
            && concurrent.contains("Timed · 12s left"),
        "expected each process wait to render with its own identity:\n{concurrent}"
    );
    assert_chatwidget_snapshot!("concurrent_process_waits_active", concurrent);

    chat.track_unified_exec_process_end("call-build", Some("p1"));
    send_wait_finished(
        &mut chat,
        "turn-1",
        "w1",
        "call-build",
        "p1",
        /*elapsed_ms*/ 138_240,
        TerminalWaitCompletionReason::Exited,
    );
    chat.refresh_unified_exec_wait_status_at(now_ms);
    chat.bottom_pane.reset_status_timer(Duration::ZERO);

    let remaining = normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 100));
    assert!(
        remaining.contains("Waiting for timed process result")
            && remaining.contains("cargo test")
            && remaining.contains("process p2")
            && !remaining.contains("cargo build")
            && !remaining.contains("process p1"),
        "finishing one wait must leave the other wait active:\n{remaining}"
    );
    assert_chatwidget_snapshot!("process_wait_one_exits_other_active", remaining);
    assert_eq!(chat.unified_exec_processes.len(), 1);
    assert_eq!(chat.unified_exec_processes[0].key, "p2");

    let completed = inserted_history_text(drain_insert_history(&mut rx));
    assert!(
        completed.contains("Waited 2m 18.24s")
            && completed.contains("process exited")
            && completed.contains("cargo build")
            && completed.contains("process p1")
            && !completed.contains("wait w1"),
        "expected the matching process exit result, got:\n{completed}"
    );
    assert_chatwidget_snapshot!("process_wait_process_exited_history", completed);
}

#[tokio::test]
async fn timed_process_wait_shows_countdown_then_elapsed_result() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    let _ = drain_insert_history(&mut rx);
    track_process(&mut chat, "call-format", "p3", "cargo fmt --check");
    let now_ms = unix_timestamp_ms();

    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w3",
            item_id: "call-format",
            process_id: "p3",
            mode: TerminalWaitMode::Timed,
            started_at_ms: now_ms,
            deadline_at_ms: Some(now_ms.saturating_add(60_000)),
        },
    );
    chat.refresh_unified_exec_wait_status_at(now_ms);
    chat.bottom_pane.reset_status_timer(Duration::ZERO);

    let waiting = normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 100));
    assert!(
        waiting.contains("Waiting for timed process result")
            && waiting.contains("1m 00s left")
            && waiting.contains("cargo fmt --check"),
        "expected a timed wait countdown and command identity:\n{waiting}"
    );
    assert_chatwidget_snapshot!("timed_process_wait_countdown", waiting);

    send_wait_finished(
        &mut chat,
        "turn-1",
        "w3",
        "call-format",
        "p3",
        /*elapsed_ms*/ 60_240,
        TerminalWaitCompletionReason::Timeout,
    );

    let completed = inserted_history_text(drain_insert_history(&mut rx));
    assert!(
        completed.contains("Waited 1m 00.24s")
            && completed.contains("timed wait ended")
            && completed.contains("cargo fmt --check")
            && !completed.contains("wait w3"),
        "expected timed wait elapsed result, got:\n{completed}"
    );
    assert_chatwidget_snapshot!("timed_process_wait_elapsed_history", completed);
}

#[tokio::test]
async fn concurrent_wait_invocations_for_one_process_remain_distinct() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    let _ = drain_insert_history(&mut rx);
    track_process(&mut chat, "call-build", "p10", "cargo build");
    let now_ms = unix_timestamp_ms();

    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w10",
            item_id: "call-build",
            process_id: "p10",
            mode: TerminalWaitMode::UntilExit,
            started_at_ms: now_ms.saturating_sub(10_000),
            deadline_at_ms: None,
        },
    );
    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w11",
            item_id: "call-build",
            process_id: "p10",
            mode: TerminalWaitMode::UntilExit,
            started_at_ms: now_ms.saturating_sub(5_000),
            deadline_at_ms: None,
        },
    );
    chat.refresh_unified_exec_wait_status_at(now_ms);
    chat.bottom_pane.reset_status_timer(Duration::ZERO);

    let concurrent = normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 110));
    assert!(
        concurrent.contains("Waiting on 1 process")
            && concurrent.contains("process p10")
            && concurrent.contains("wait w10")
            && concurrent.contains("wait w11"),
        "same-process invocations should have distinct wait identities:\n{concurrent}"
    );
    assert_chatwidget_snapshot!("concurrent_wait_invocations_one_process", concurrent);

    send_wait_finished(
        &mut chat,
        "turn-1",
        "w10",
        "call-build",
        "p10",
        10_000,
        TerminalWaitCompletionReason::Exited,
    );
    let remaining = render_bottom_popup(&chat, /*width*/ 110);
    assert!(
        remaining.contains("Waiting for process exit")
            && remaining.contains("wait w11")
            && !remaining.contains("wait w10"),
        "finishing one invocation must not remove the other wait on the same process:\n{remaining}"
    );

    send_wait_finished(
        &mut chat,
        "turn-1",
        "w11",
        "call-build",
        "p10",
        5_000,
        TerminalWaitCompletionReason::Exited,
    );
    let completed = inserted_history_text(drain_insert_history(&mut rx));
    assert_eq!(
        completed,
        "• Waited 10s · process exited · cargo build (process p10)\n\n\
• Waited 5s · process exited · cargo build (process p10)"
    );
}

#[tokio::test]
async fn process_wait_reports_input_release_and_cancellation() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    let _ = drain_insert_history(&mut rx);
    track_process(&mut chat, "call-one", "p4", "make test");
    track_process(&mut chat, "call-two", "p5", "make lint");

    let start_at_ms = unix_timestamp_ms().saturating_add(60_000);
    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w4",
            item_id: "call-one",
            process_id: "p4",
            mode: TerminalWaitMode::UntilExit,
            started_at_ms: start_at_ms,
            deadline_at_ms: None,
        },
    );
    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w5",
            item_id: "call-two",
            process_id: "p5",
            mode: TerminalWaitMode::UntilExit,
            started_at_ms: start_at_ms,
            deadline_at_ms: None,
        },
    );

    send_wait_finished(
        &mut chat,
        "turn-1",
        "w4",
        "call-one",
        "p4",
        133_000,
        TerminalWaitCompletionReason::Input,
    );
    let still_waiting = render_bottom_popup(&chat, /*width*/ 100);
    assert!(
        still_waiting.contains("Waiting for process exit")
            && still_waiting.contains("make lint")
            && !still_waiting.contains("make test"),
        "input release should finish only its own process wait:\n{still_waiting}"
    );

    send_wait_finished(
        &mut chat,
        "turn-1",
        "w5",
        "call-two",
        "p5",
        9_000,
        TerminalWaitCompletionReason::Cancelled,
    );

    assert!(chat.unified_exec_wait_tracker.is_none());
    assert_eq!(chat.status_state.current_status.header, "Working");
    assert_eq!(chat.unified_exec_processes.len(), 2);

    let completed = inserted_history_text(drain_insert_history(&mut rx));
    assert!(
        completed.contains("Waited 2m 13s")
            && completed.contains("interrupted by input")
            && completed.contains("make test")
            && completed.contains("Waited 9s")
            && completed.contains("wait cancelled")
            && completed.contains("make lint"),
        "expected accurate per-invocation wait outcomes, got:\n{completed}"
    );
    assert!(
        !completed.contains("wait w4") && !completed.contains("wait w5"),
        "completion correlation IDs must remain internal:\n{completed}"
    );
    assert_chatwidget_snapshot!("process_wait_input_release_and_cancel", completed);
}

#[tokio::test]
async fn interrupted_turn_clears_a_wait_without_finish_metadata() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    track_process(&mut chat, "call-build", "p6", "cargo build");
    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w6",
            item_id: "call-build",
            process_id: "p6",
            mode: TerminalWaitMode::UntilExit,
            started_at_ms: unix_timestamp_ms(),
            deadline_at_ms: None,
        },
    );

    assert!(chat.unified_exec_wait_tracker.is_some());

    handle_turn_interrupted(&mut chat, "turn-1");

    assert!(chat.unified_exec_wait_tracker.is_none());
    assert_eq!(chat.status_state.current_status.header, "Working");
    assert_chatwidget_snapshot!(
        "process_wait_interrupted_turn_cleanup",
        normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 100))
    );
}

#[tokio::test]
async fn wait_events_are_identity_checked_idempotent_and_deferred_during_streaming() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    let _ = drain_insert_history(&mut rx);
    track_process(&mut chat, "call-live", "p7", "cargo test");

    let now_ms = unix_timestamp_ms();
    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w7",
            item_id: "call-live",
            process_id: "p7",
            mode: TerminalWaitMode::UntilExit,
            started_at_ms: now_ms.saturating_sub(133_000),
            deadline_at_ms: None,
        },
    );
    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w7",
            item_id: "call-live",
            process_id: "p7",
            mode: TerminalWaitMode::UntilExit,
            started_at_ms: now_ms,
            deadline_at_ms: None,
        },
    );
    chat.refresh_unified_exec_wait_status_at(now_ms);
    assert!(
        chat.status_state
            .current_status
            .details
            .as_deref()
            .is_some_and(|details| details.contains("2m 13s elapsed")),
        "a duplicate start must not reset the original wait clock"
    );

    send_wait_finished(
        &mut chat,
        "turn-1",
        "w7",
        "different-call",
        "p7",
        133_000,
        TerminalWaitCompletionReason::Exited,
    );
    assert!(chat.unified_exec_wait_tracker.is_some());
    chat.refresh_unified_exec_wait_status_at(now_ms);
    assert_eq!(
        chat.status_state.current_status.header,
        "Waiting for process exit"
    );
    assert!(
        chat.status_state
            .current_status
            .details
            .as_deref()
            .is_some_and(|details| details.contains("wait w7")),
        "a mismatched call identity must leave the active wait visible"
    );
    assert!(
        inserted_history_text(drain_insert_history(&mut rx)).is_empty(),
        "a mismatched completion must not insert history"
    );

    send_wait_finished(
        &mut chat,
        "turn-1",
        "w7",
        "call-live",
        "p7",
        133_000,
        TerminalWaitCompletionReason::Exited,
    );
    let first_completion = inserted_history_text(drain_insert_history(&mut rx));
    assert!(first_completion.contains("Waited 2m 13s"));
    send_wait_finished(
        &mut chat,
        "turn-1",
        "w7",
        "call-live",
        "p7",
        133_000,
        TerminalWaitCompletionReason::Exited,
    );
    assert!(
        inserted_history_text(drain_insert_history(&mut rx)).is_empty(),
        "a duplicate completion must not insert history twice"
    );

    track_process(&mut chat, "call-old", "p8", "cargo build");
    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-1",
            interaction_id: "w8",
            item_id: "call-old",
            process_id: "p8",
            mode: TerminalWaitMode::UntilExit,
            started_at_ms: unix_timestamp_ms(),
            deadline_at_ms: None,
        },
    );
    handle_turn_interrupted(&mut chat, "turn-1");
    handle_turn_started(&mut chat, "turn-2");
    let _ = drain_insert_history(&mut rx);
    send_wait_finished(
        &mut chat,
        "turn-1",
        "w8",
        "call-old",
        "p8",
        9_000,
        TerminalWaitCompletionReason::Exited,
    );
    assert!(
        inserted_history_text(drain_insert_history(&mut rx)).is_empty(),
        "a late completion from an interrupted turn must not enter the next turn"
    );

    track_process(&mut chat, "call-stream", "p9", "cargo fmt");
    let wait_started_at_ms = unix_timestamp_ms();
    send_wait_started(
        &mut chat,
        WaitStart {
            turn_id: "turn-2",
            interaction_id: "w9",
            item_id: "call-stream",
            process_id: "p9",
            mode: TerminalWaitMode::Timed,
            started_at_ms: wait_started_at_ms,
            deadline_at_ms: Some(wait_started_at_ms.saturating_add(30_000)),
        },
    );
    chat.on_agent_message_delta("Assistant output is still streaming.\n".to_string());
    send_wait_finished(
        &mut chat,
        "turn-2",
        "w9",
        "call-stream",
        "p9",
        5_000,
        TerminalWaitCompletionReason::Timeout,
    );
    assert!(chat.stream_controller.is_some());
    let while_streaming = inserted_history_text(drain_insert_history(&mut rx));
    assert!(
        !while_streaming.contains("Waited 5s"),
        "wait history must remain in the presentation FIFO until the answer stream completes"
    );

    chat.finalize_completed_assistant_message(
        Some("Assistant output is still streaming.\n"),
        /*phase*/ None,
    );
    let completed = inserted_history_text(drain_insert_history(&mut rx));
    assert!(
        completed.contains("Waited 5s") && completed.contains("timed wait ended"),
        "the deferred wait result should be inserted after the natural stream completion:\n{completed}"
    );
}

struct WaitStart<'a> {
    turn_id: &'a str,
    interaction_id: &'a str,
    item_id: &'a str,
    process_id: &'a str,
    mode: TerminalWaitMode,
    started_at_ms: i64,
    deadline_at_ms: Option<i64>,
}

fn send_wait_started(chat: &mut ChatWidget, start: WaitStart<'_>) {
    send_terminal_interaction(
        chat,
        start.turn_id,
        start.item_id,
        start.process_id,
        start.deadline_at_ms,
        Some(TerminalWait::Started {
            interaction_id: start.interaction_id.to_string(),
            started_at_ms: start.started_at_ms,
            mode: start.mode,
        }),
    );
}

fn track_process(chat: &mut ChatWidget, call_id: &str, process_id: &str, command_display: &str) {
    chat.unified_exec_processes.push(UnifiedExecProcessSummary {
        key: process_id.to_string(),
        call_id: call_id.to_string(),
        command_display: command_display.to_string(),
        user_shell_response_handling: None,
        recent_chunks: Vec::new(),
    });
    chat.sync_unified_exec_footer();
}

fn send_wait_finished(
    chat: &mut ChatWidget,
    turn_id: &str,
    interaction_id: &str,
    item_id: &str,
    process_id: &str,
    elapsed_ms: u64,
    reason: TerminalWaitCompletionReason,
) {
    send_terminal_interaction(
        chat,
        turn_id,
        item_id,
        process_id,
        None,
        Some(TerminalWait::Finished {
            interaction_id: interaction_id.to_string(),
            elapsed_ms,
            reason,
        }),
    );
}

fn send_terminal_interaction(
    chat: &mut ChatWidget,
    turn_id: &str,
    item_id: &str,
    process_id: &str,
    deadline_at_ms: Option<i64>,
    wait: Option<TerminalWait>,
) {
    chat.handle_server_notification(
        ServerNotification::TerminalInteraction(TerminalInteractionNotification {
            thread_id: chat.thread_id.map(|id| id.to_string()).unwrap_or_default(),
            turn_id: turn_id.to_string(),
            item_id: item_id.to_string(),
            process_id: process_id.to_string(),
            stdin: String::new(),
            deadline_at_ms,
            wait,
        }),
        /*replay_kind*/ None,
    );
}

fn inserted_history_text(cells: Vec<Vec<ratatui::text::Line<'static>>>) -> String {
    cells
        .into_iter()
        .map(|cell| {
            cell.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn unix_timestamp_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the unix epoch")
            .as_millis(),
    )
    .expect("unix timestamp milliseconds should fit into i64")
}
