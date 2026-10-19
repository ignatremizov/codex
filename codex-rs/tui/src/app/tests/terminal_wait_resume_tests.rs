use super::*;
use codex_app_server_protocol::TerminalInteractionNotification;
use codex_app_server_protocol::TerminalWait;
use codex_app_server_protocol::TerminalWaitCompletionReason;
use codex_app_server_protocol::TerminalWaitMode;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn live_wait_survives_buffer_eviction_and_restores_after_reasoning_replay() {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let thread_id = ThreadId::new();
    let session = test_thread_session(thread_id, app.config.cwd.to_path_buf());
    let mut store = ThreadEventStore::new_with_session(
        /*capacity*/ 1,
        session,
        vec![test_turn(
            "turn",
            TurnStatus::InProgress,
            vec![ThreadItem::Reasoning {
                id: "reasoning".into(),
                summary: vec!["**Analyzing tool time constraints**".into()],
                content: Vec::new(),
            }],
        )],
    );
    let started = TerminalInteractionNotification {
        thread_id: thread_id.to_string(),
        turn_id: "turn".into(),
        item_id: "command".into(),
        process_id: "123".into(),
        stdin: String::new(),
        deadline_at_ms: Some(1_000),
        wait: Some(TerminalWait::Started {
            interaction_id: "wait".into(),
            started_at_ms: 0,
            mode: TerminalWaitMode::Timed,
        }),
    };
    store.push_notification(ServerNotification::TerminalInteraction(started.clone()));
    store.push_notification(ServerNotification::ReasoningSummaryTextDelta(
        codex_app_server_protocol::ReasoningSummaryTextDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn".into(),
            item_id: "reasoning".into(),
            summary_index: 0,
            delta: "Still analyzing".into(),
        },
    ));
    assert_eq!(store.buffer.len(), 1);
    app.replay_thread_snapshot(store.snapshot(), /*resume_restored_queue*/ false);
    let rendered = render_bottom_popup(&app.chat_widget, /*width*/ 100);
    let status = rendered
        .lines()
        .filter(|line| line.contains("Waiting for timed") || line.contains("Timed ·"))
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(status, @"
    • Waiting for timed process result (0s left • esc to interrupt)
      └ Timed · background terminal · process 123 · wait wait
    ");

    let (mut historical, _history_events, _history_ops) = make_test_app_with_channels().await;
    historical.replay_thread_snapshot_with_kind(
        store.snapshot(),
        /*resume_restored_queue*/ false,
        ReplayKind::ReplayOnlyThreadSnapshot,
    );
    assert!(
        !render_bottom_popup(&historical.chat_widget, /*width*/ 100).contains("Waiting for timed")
    );

    let finished = ServerNotification::TerminalInteraction(TerminalInteractionNotification {
        deadline_at_ms: None,
        wait: Some(TerminalWait::Finished {
            interaction_id: "wait".into(),
            elapsed_ms: 600_000,
            reason: TerminalWaitCompletionReason::Timeout,
        }),
        ..started
    });
    store.push_notification(finished.clone());
    app.chat_widget
        .handle_server_notification(finished, /*replay_kind*/ None);
    let active = store
        .snapshot()
        .events
        .into_iter()
        .filter_map(|event| match event {
            ThreadBufferedEvent::Notification(notification) => match *notification {
                ServerNotification::TerminalInteraction(wait) => Some(wait),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(active, Vec::new());
    assert!(!render_bottom_popup(&app.chat_widget, /*width*/ 100).contains("Waiting for timed"));
}
