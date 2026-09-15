use super::*;
use pretty_assertions::assert_eq;

fn sleep_item(id: &str, duration_ms: u64) -> AppServerThreadItem {
    AppServerThreadItem::Sleep(codex_app_server_protocol::SleepItem {
        id: id.to_string(),
        duration_ms,
        outcome: None,
        elapsed_ms: None,
    })
}

fn completed_sleep_item(
    id: &str,
    duration_ms: u64,
    outcome: codex_app_server_protocol::SleepOutcome,
    elapsed_ms: u64,
) -> AppServerThreadItem {
    AppServerThreadItem::Sleep(codex_app_server_protocol::SleepItem {
        id: id.to_string(),
        duration_ms,
        outcome: Some(outcome),
        elapsed_ms: Some(elapsed_ms),
    })
}

fn sleep_lines(cells: Vec<Vec<ratatui::text::Line<'static>>>) -> Vec<String> {
    cells
        .into_iter()
        .flatten()
        .map(|line| line.to_string())
        .filter(|line| line.contains("Sleep"))
        .collect()
}

fn sleep_lifecycle_events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> Vec<String> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::ConsolidateAgentMessage { source, .. } => {
                Some(format!("answer: {}", source.trim_end()))
            }
            AppEvent::InsertHistoryCell(cell)
                if cell.as_any().is::<crate::history_cell::SleepCell>() =>
            {
                Some(
                    cell.display_lines(/*width*/ 80)
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
            _ => None,
        })
        .collect()
}

fn current_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_millis()
        .try_into()
        .expect("epoch milliseconds should fit in i64")
}

#[tokio::test]
async fn live_sleep_shows_active_duration_then_compact_requested_duration() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    drain_insert_history(&mut rx);
    let started_at_ms = current_epoch_ms();

    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            started_at_ms,
            deadline_at_ms: None,
            item: sleep_item("sleep-1", 60_000),
        }),
        /*replay_kind*/ None,
    );
    assert!(chat.transcript.active_cell.is_none());
    assert_eq!(chat.status_state.current_status.header, "Sleeping");
    assert_eq!(
        chat.status_state.countdown_owner,
        Some(StatusCountdownOwner::Sleep {
            turn_id: "turn-1".to_string(),
            item_id: "sleep-1".to_string(),
        })
    );
    chat.bottom_pane.reset_status_timer(Duration::ZERO);
    assert_chatwidget_snapshot!(
        "sleep_status_countdown",
        render_bottom_popup(&chat, /*width*/ 100)
    );

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1_250,
            item: completed_sleep_item(
                "sleep-1",
                /*duration_ms*/ 60_000,
                codex_app_server_protocol::SleepOutcome::Completed,
                /*elapsed_ms*/ 1_250,
            ),
        }),
        /*replay_kind*/ None,
    );

    assert_snapshot!(
        sleep_lines(drain_insert_history(&mut rx)).join("\n"),
        @"• Sleep · completed after 1.25s · requested 1m 00s"
    );
    assert!(chat.transcript.active_cell.is_none());

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1_251,
            item: sleep_item("sleep-1", 60_000),
        }),
        /*replay_kind*/ None,
    );
    assert!(drain_insert_history(&mut rx).is_empty());
    assert_eq!(chat.status_state.current_status.header, "Working");
    assert!(chat.status_state.countdown_owner.is_none());
    assert!(chat.turn_lifecycle.active_sleep.is_none());
}

#[tokio::test]
async fn interrupted_sleep_is_compacted_and_late_duplicate_completions_are_ignored() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    drain_insert_history(&mut rx);

    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            started_at_ms: current_epoch_ms(),
            deadline_at_ms: None,
            item: sleep_item("sleep-2", 300_000),
        }),
        /*replay_kind*/ None,
    );
    drain_insert_history(&mut rx);

    handle_turn_interrupted(&mut chat, "turn-1");
    assert_eq!(
        sleep_lines(drain_insert_history(&mut rx)),
        vec!["• Sleep · requested 5m 00s"]
    );
    assert!(chat.transcript.active_cell.is_none());
    assert_eq!(chat.status_state.current_status.header, "Working");
    assert!(chat.status_state.countdown_owner.is_none());
    assert!(chat.turn_lifecycle.active_sleep.is_none());

    for _ in 0..2 {
        chat.handle_server_notification(
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id: String::new(),
                turn_id: "turn-1".to_string(),
                completed_at_ms: 1,
                item: sleep_item("sleep-2", 300_000),
            }),
            /*replay_kind*/ None,
        );
    }

    assert!(drain_insert_history(&mut rx).is_empty());

    handle_turn_started(&mut chat, "turn-2");
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: String::new(),
            turn_id: "turn-2".to_string(),
            completed_at_ms: 2,
            item: sleep_item("sleep-2", 300_000),
        }),
        /*replay_kind*/ None,
    );
    assert_eq!(
        sleep_lines(drain_insert_history(&mut rx)),
        vec!["• Sleep · requested 5m 00s"]
    );
}

#[tokio::test]
async fn replayed_sleep_is_rendered_as_compact_history() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.replay_thread_item(
        sleep_item("sleep-3", 60_025),
        "turn-1".to_string(),
        ReplayKind::ThreadSnapshot,
    );

    assert_eq!(
        sleep_lines(drain_insert_history(&mut rx)),
        vec!["• Sleep · requested 1m 00.025s"]
    );
    assert!(chat.transcript.active_cell.is_none());
    assert!(chat.turn_lifecycle.active_sleep.is_none());
}

#[tokio::test]
async fn replayed_interrupted_sleep_shows_actual_elapsed_time() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.replay_thread_item(
        completed_sleep_item(
            "sleep-interrupted",
            /*duration_ms*/ 60_000,
            codex_app_server_protocol::SleepOutcome::Interrupted,
            /*elapsed_ms*/ 19_940,
        ),
        "turn-1".to_string(),
        ReplayKind::ThreadSnapshot,
    );

    assert_snapshot!(
        sleep_lines(drain_insert_history(&mut rx)).join("\n"),
        @"• Sleep · interrupted after 19.94s · requested 1m 00s"
    );
    assert!(chat.transcript.active_cell.is_none());
    assert!(chat.turn_lifecycle.active_sleep.is_none());
}

#[tokio::test]
async fn replayed_sleep_error_and_partial_metadata_remain_distinct() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    for item in [
        completed_sleep_item(
            "sleep-error",
            /*duration_ms*/ 60_000,
            codex_app_server_protocol::SleepOutcome::Error,
            /*elapsed_ms*/ 250,
        ),
        AppServerThreadItem::Sleep(codex_app_server_protocol::SleepItem {
            id: "sleep-no-elapsed".to_string(),
            duration_ms: 60_000,
            outcome: Some(codex_app_server_protocol::SleepOutcome::Interrupted),
            elapsed_ms: None,
        }),
        AppServerThreadItem::Sleep(codex_app_server_protocol::SleepItem {
            id: "sleep-no-outcome".to_string(),
            duration_ms: 60_000,
            outcome: None,
            elapsed_ms: Some(250),
        }),
    ] {
        chat.replay_thread_item(item, "turn-1".to_string(), ReplayKind::ThreadSnapshot);
    }

    assert_snapshot!(
        sleep_lines(drain_insert_history(&mut rx)).join("\n"),
        @"
    • Sleep · error after 250ms · requested 1m 00s
    • Sleep · requested 1m 00s
    • Sleep · requested 1m 00s
    "
    );
    assert!(chat.transcript.active_cell.is_none());
    assert!(chat.turn_lifecycle.active_sleep.is_none());
}

#[tokio::test]
async fn late_sleep_notice_does_not_finalize_an_unrelated_answer_stream() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    drain_insert_history(&mut rx);
    handle_agent_message_delta(&mut chat, "Answer still streaming");
    assert!(chat.stream_controller.is_some());

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1,
            item: sleep_item("late-sleep", 1_000),
        }),
        /*replay_kind*/ None,
    );

    assert!(chat.stream_controller.is_some());
    assert!(drain_insert_history(&mut rx).is_empty());
}

#[tokio::test]
async fn terminal_sleep_cleanup_preserves_stream_before_compacting_sleep() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    drain_insert_history(&mut rx);
    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            started_at_ms: current_epoch_ms(),
            deadline_at_ms: None,
            item: sleep_item("terminal-sleep", 60_000),
        }),
        /*replay_kind*/ None,
    );
    handle_agent_message_delta(&mut chat, "Answer before missing sleep completion");

    chat.on_task_complete(
        /*last_agent_message*/ None, /*completion*/ None, /*from_replay*/ false,
    );

    assert_eq!(
        sleep_lifecycle_events(&mut rx),
        vec![
            "answer: Answer before missing sleep completion".to_string(),
            "• Sleep · requested 1m 00s".to_string(),
        ]
    );
}

#[tokio::test]
async fn replacing_sleep_during_stream_defers_prior_sleep_history() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    drain_insert_history(&mut rx);

    for (item_id, duration_ms) in [("sleep-a", 60_000), ("sleep-b", 120_000)] {
        chat.handle_server_notification(
            ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id: String::new(),
                turn_id: "turn-1".to_string(),
                started_at_ms: current_epoch_ms(),
                deadline_at_ms: None,
                item: sleep_item(item_id, duration_ms),
            }),
            /*replay_kind*/ None,
        );
        if item_id == "sleep-a" {
            handle_agent_message_delta(&mut chat, "Answer between sleeps");
        }
    }

    chat.on_task_complete(
        /*last_agent_message*/ None, /*completion*/ None, /*from_replay*/ false,
    );

    assert_eq!(
        sleep_lifecycle_events(&mut rx),
        vec![
            "answer: Answer between sleeps".to_string(),
            "• Sleep · requested 1m 00s".to_string(),
            "• Sleep · requested 2m 00s".to_string(),
        ]
    );
}

#[tokio::test]
async fn sleep_completion_does_not_clear_newer_status_countdown() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    drain_insert_history(&mut rx);

    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            started_at_ms: current_epoch_ms(),
            deadline_at_ms: None,
            item: sleep_item("sleep-4", 60_000),
        }),
        /*replay_kind*/ None,
    );
    let newer_owner = StatusCountdownOwner::CollabWait {
        turn_id: "turn-1".to_string(),
        call_id: "newer".to_string(),
    };
    // A new header retires the previous countdown; install its owner afterward.
    chat.set_status_header("Waiting".to_string());
    chat.set_status_countdown_deadline_at_ms(
        newer_owner.clone(),
        current_epoch_ms().saturating_add(120_000),
    );

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1,
            item: sleep_item("sleep-4", 60_000),
        }),
        /*replay_kind*/ None,
    );

    assert_eq!(chat.status_state.current_status.header, "Waiting");
    assert_eq!(chat.status_state.countdown_owner, Some(newer_owner));
    assert_eq!(
        sleep_lines(drain_insert_history(&mut rx)),
        vec!["• Sleep · requested 1m 00s"]
    );
}

#[tokio::test]
async fn sleep_starts_require_the_current_live_model_turn() {
    for state in ["idle", "replay", "ended_but_mcp_busy", "newer_turn"] {
        let (mut chat, mut events, _operations) =
            make_chatwidget_manual(/*model_override*/ None).await;
        if state != "idle" {
            handle_turn_started(&mut chat, "turn-1");
        }
        if state == "ended_but_mcp_busy" || state == "newer_turn" {
            chat.finalize_turn();
            if state == "ended_but_mcp_busy" {
                chat.bottom_pane.set_task_running(true);
            } else {
                handle_turn_started(&mut chat, "turn-2");
            }
        }
        let _ = drain_insert_history(&mut events);
        let expected = (
            chat.status_state.current_status.clone(),
            chat.status_state.countdown_owner.clone(),
        );
        chat.on_sleep_started(
            codex_app_server_protocol::SleepItem {
                id: "stale-sleep".into(),
                duration_ms: 60_000,
                outcome: None,
                elapsed_ms: None,
            },
            "turn-1".into(),
            current_epoch_ms(),
            state == "replay",
        );
        assert_eq!(
            (
                chat.status_state.current_status.clone(),
                chat.status_state.countdown_owner.clone()
            ),
            expected,
            "{state} must not acquire a live sleep status"
        );
        assert!(chat.turn_lifecycle.active_sleep.is_none());
        assert!(drain_insert_history(&mut events).is_empty());
    }
}

#[tokio::test]
async fn sleep_activity_retains_history_without_replacing_foreground_status() {
    for foreground in ["guardian", "retry", "compaction", "collaboration"] {
        let (mut chat, mut events, _operations) =
            make_chatwidget_manual(/*model_override*/ None).await;
        handle_turn_started(&mut chat, "turn-1");
        match foreground {
            "guardian" => {
                chat.status_state
                    .pending_guardian_review_status
                    .start_or_update("review".into(), "another command".into());
                chat.set_status_header("Reviewing approval request".into());
            }
            "retry" => {
                chat.status_state.retry_status_header = Some("Reconnecting".into());
                chat.set_status_header("Reconnecting".into());
            }
            "compaction" => chat.on_context_compaction_started(
                "compact".into(),
                "turn-1".into(),
                Duration::ZERO,
            ),
            "collaboration" => {
                chat.set_status_header("Waiting for Ada".into());
                chat.set_status_countdown_deadline_at_ms(
                    StatusCountdownOwner::CollabWait {
                        turn_id: "turn-1".into(),
                        call_id: "collab-wait".into(),
                    },
                    current_epoch_ms().saturating_add(60_000),
                );
            }
            _ => unreachable!(),
        }
        let _ = drain_insert_history(&mut events);
        let expected = (
            chat.status_state.current_status.clone(),
            chat.status_state.countdown_owner.clone(),
        );
        let item = codex_app_server_protocol::SleepItem {
            id: "overlapping-sleep".into(),
            duration_ms: 60_000,
            outcome: None,
            elapsed_ms: None,
        };
        chat.on_sleep_started(
            item.clone(),
            "turn-1".into(),
            current_epoch_ms(),
            /*from_replay*/ false,
        );
        assert_eq!(
            (
                chat.status_state.current_status.clone(),
                chat.status_state.countdown_owner.clone()
            ),
            expected
        );
        assert_eq!(
            chat.turn_lifecycle
                .active_sleep
                .as_ref()
                .map(|sleep| (sleep.turn_id.as_str(), &sleep.item)),
            Some(("turn-1", &item))
        );
        chat.on_sleep_completed(item, "turn-1", /*from_replay*/ false);
        assert_eq!(
            (
                chat.status_state.current_status.clone(),
                chat.status_state.countdown_owner.clone()
            ),
            expected
        );
        assert!(chat.turn_lifecycle.active_sleep.is_none());
        assert_eq!(
            sleep_lines(drain_insert_history(&mut events)),
            vec!["• Sleep · requested 1m 00s"]
        );
    }
}

#[tokio::test]
async fn sleep_and_model_finalization_preserve_an_independent_user_shell_cell() {
    enum Completion {
        TurnEnd,
        Live,
        Replay,
    }
    for completion in [Completion::TurnEnd, Completion::Live, Completion::Replay] {
        let (mut chat, mut events, _operations) =
            make_chatwidget_manual(/*model_override*/ None).await;
        handle_turn_started(&mut chat, "turn-1");
        let _shell = begin_exec_with_source_and_process_id(
            &mut chat,
            "independent-shell",
            "sleep 60",
            ExecCommandSource::UserShell,
            Some("5151"),
        );
        let sleep = codex_app_server_protocol::SleepItem {
            id: "clock-sleep".into(),
            duration_ms: 60_000,
            outcome: None,
            elapsed_ms: None,
        };
        chat.on_sleep_started(
            sleep.clone(),
            "turn-1".into(),
            current_epoch_ms(),
            /*from_replay*/ false,
        );
        let _ = drain_insert_history(&mut events);
        match completion {
            Completion::TurnEnd => {}
            Completion::Live => {
                chat.on_sleep_completed(sleep, "turn-1", /*from_replay*/ false)
            }
            Completion::Replay => {
                chat.on_sleep_completed(sleep, "turn-1", /*from_replay*/ true)
            }
        }
        chat.finalize_turn();
        let cell = chat
            .transcript
            .active_cell
            .as_ref()
            .expect("independent live command")
            .as_any()
            .downcast_ref::<ExecCell>()
            .expect("shell command cell");
        assert!(cell.is_active());
        assert_eq!(
            cell.group
                .calls
                .iter()
                .map(|call| (call.call_id.as_str(), call.duration))
                .collect::<Vec<_>>(),
            vec![("independent-shell", None)]
        );
        assert!(chat.running_commands.contains_key("independent-shell"));
        assert_eq!(
            chat.unified_exec_processes
                .iter()
                .map(|process| process.key.as_str())
                .collect::<Vec<_>>(),
            vec!["5151"]
        );
        assert!(chat.turn_lifecycle.active_sleep.is_none());
        assert_eq!(
            sleep_lines(drain_insert_history(&mut events)),
            vec!["• Sleep · requested 1m 00s"]
        );
    }
}
