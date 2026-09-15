use super::*;
use pretty_assertions::assert_eq;

fn sleep_item(id: &str, duration_ms: u64) -> AppServerThreadItem {
    AppServerThreadItem::Sleep(codex_app_server_protocol::SleepItem {
        id: id.to_string(),
        duration_ms,
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
            item: sleep_item("sleep-1", 60_000),
        }),
        /*replay_kind*/ None,
    );

    assert_snapshot!(
        sleep_lines(drain_insert_history(&mut rx)).join("\n"),
        @"• Sleep · requested 1m 00s"
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
        /*last_agent_message*/ None, /*duration_ms*/ None, /*from_replay*/ false,
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
        /*last_agent_message*/ None, /*duration_ms*/ None, /*from_replay*/ false,
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
        call_id: "newer".to_string(),
    };
    chat.set_status_countdown_deadline_at_ms(
        newer_owner.clone(),
        current_epoch_ms().saturating_add(120_000),
    );
    chat.set_status_header("Waiting".to_string());

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
