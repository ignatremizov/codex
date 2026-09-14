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

#[tokio::test]
async fn live_sleep_shows_active_duration_then_compact_requested_duration() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    drain_insert_history(&mut rx);

    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            started_at_ms: 0,
            deadline_at_ms: None,
            item: sleep_item("sleep-1", 1_250),
        }),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            started_at_ms: 0,
            deadline_at_ms: None,
            item: sleep_item("sleep-1", 1_250),
        }),
        /*replay_kind*/ None,
    );

    let active = chat
        .transcript
        .active_cell
        .as_ref()
        .expect("sleep should be visible while active")
        .display_lines(/*width*/ 80);
    assert_snapshot!(lines_to_single_string(&active).trim(), @"• Sleeping · 1.25s");

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1_250,
            item: sleep_item("sleep-1", 1_250),
        }),
        /*replay_kind*/ None,
    );

    assert_eq!(
        sleep_lines(drain_insert_history(&mut rx)),
        vec!["• Sleep · requested 1.25s"]
    );
    assert!(chat.transcript.active_cell.is_none());

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: String::new(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1_251,
            item: sleep_item("sleep-1", 1_250),
        }),
        /*replay_kind*/ None,
    );
    assert!(drain_insert_history(&mut rx).is_empty());
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
            started_at_ms: 0,
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
