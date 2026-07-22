use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn compacted_skill_inventory_is_visible_in_live_and_historical_projection() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let item = AppServerThreadItem::ContextCompaction {
        id: "compact-skills".into(),
        summary: None,
        message: None,
        decode_error: None,
        available_skills: vec!["test-tui".into(), "remote-tests".into()],
    };
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: thread_id(&chat),
            turn_id: "turn-1".into(),
            completed_at_ms: 0,
            item: item.clone(),
        }),
        /*replay_kind*/ None,
    );
    let live = drain_insert_history_transcript(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    let cells = crate::thread_transcript::thread_items_to_transcript_cells(
        /*thread_id*/ None,
        &chat.config.cwd,
        [item],
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        Some(&chat.config),
    );
    let historical = cells
        .iter()
        .flat_map(|cell| cell.transcript_lines(/*width*/ 200))
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let inventory = "Available skills after compaction: test-tui, remote-tests";
    assert!(live.contains(inventory));
    assert!(historical.contains(inventory));
    insta::assert_snapshot!(historical, @"
    • Context compacted
    • Available skills after compaction: test-tui, remote-tests
    ");
}

fn normalize_compaction_snapshot(text: String) -> String {
    let elapsed = regex_lite::Regex::new(r"\b\d+(?:h \d+m \d+s|m \d+s|s)\b").unwrap();
    elapsed
        .replace_all(&normalize_snapshot_paths(text), "<elapsed>")
        .into_owned()
}

fn compaction_started(id: &str) -> ServerNotification {
    ServerNotification::ItemStarted(ItemStartedNotification {
        deadline_at_ms: None,
        thread_id: "thread-1".to_string(),
        turn_id: "turn-1".to_string(),
        started_at_ms: chrono::Utc::now().timestamp_millis(),
        item: AppServerThreadItem::ContextCompaction {
            id: id.to_string(),
            summary: None,
            message: None,
            available_skills: Vec::new(),
            decode_error: None,
        },
    })
}

fn compaction_completed(id: &str) -> ServerNotification {
    ServerNotification::ItemCompleted(ItemCompletedNotification {
        thread_id: "thread-1".to_string(),
        turn_id: "turn-1".to_string(),
        completed_at_ms: 0,
        item: AppServerThreadItem::ContextCompaction {
            id: id.to_string(),
            summary: None,
            message: None,
            available_skills: Vec::new(),
            decode_error: None,
        },
    })
}

fn compaction_status(
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    message: &str,
) -> ServerNotification {
    ServerNotification::ContextCompactionStatus(
        codex_app_server_protocol::ContextCompactionStatusNotification {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            item_id: item_id.to_string(),
            message: message.to_string(),
        },
    )
}

#[tokio::test]
async fn decoding_status_preserves_timer_across_follow_up_and_duplicate_start() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    handle_turn_started(&mut chat, "turn-1");
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    let started_at = chat.status_state.compaction.as_ref().unwrap().started_at;
    chat.handle_server_notification(
        compaction_status(&thread_id.to_string(), "turn-1", "compact-1", "Decoding"),
        /*replay_kind*/ None,
    );
    let rendered = normalize_compaction_snapshot(render_bottom_popup(&chat, /*width*/ 80));
    insta::assert_snapshot!(rendered.lines().next().unwrap(), @"• Decoding (<elapsed> • esc to interrupt)");
    chat.set_status_header("Working".to_string());
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    chat.handle_server_notification(
        compaction_status(&thread_id.to_string(), "turn-1", "compact-1", "Decoding"),
        /*replay_kind*/ None,
    );
    assert_eq!(
        (
            chat.bottom_pane.status_widget().unwrap().header(),
            chat.status_state.compaction.as_ref().unwrap().started_at,
        ),
        ("Decoding", started_at),
    );
    assert!(drain_insert_history(&mut rx).is_empty());
    chat.handle_server_notification(compaction_completed("compact-1"), /*replay_kind*/ None);
    assert!(chat.status_state.compaction.is_none());
    assert_eq!(
        chat.bottom_pane.status_widget().unwrap().header(),
        "Working"
    );
    assert_eq!(drain_insert_history(&mut rx).len(), 1);
    chat.handle_server_notification(
        compaction_status(&thread_id.to_string(), "turn-1", "compact-1", "Decoding"),
        /*replay_kind*/ None,
    );
    assert!(chat.status_state.compaction.is_none());
    assert_eq!(
        chat.bottom_pane.status_widget().unwrap().header(),
        "Working"
    );
    assert!(drain_insert_history(&mut rx).is_empty());
}

#[tokio::test]
async fn invalid_decoding_status_cannot_clear_retry_or_change_active_compaction() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    handle_turn_started(&mut chat, "turn-1");
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    chat.handle_server_notification(
        compaction_status(&thread_id.to_string(), "turn-1", "compact-1", "Decoding"),
        /*replay_kind*/ None,
    );
    let started_at = chat.status_state.compaction.as_ref().unwrap().started_at;
    chat.handle_server_notification(
        ServerNotification::Error(ErrorNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".into(),
            will_retry: true,
            error: codex_app_server_protocol::TurnError {
                message: "Reconnecting".into(),
                codex_error_info: None,
                additional_details: None,
                misalignment: None,
            },
        }),
        /*replay_kind*/ None,
    );
    for (thread, turn, item, message, replay) in [
        (
            "other-thread".to_string(),
            "turn-1",
            "compact-1",
            "Decoding",
            None,
        ),
        (
            thread_id.to_string(),
            "other-turn",
            "compact-1",
            "Decoding",
            None,
        ),
        (
            thread_id.to_string(),
            "turn-1",
            "other-item",
            "Decoding",
            None,
        ),
        (thread_id.to_string(), "turn-1", "compact-1", " \n ", None),
        (
            thread_id.to_string(),
            "turn-1",
            "compact-1",
            "Decoding",
            Some(ReplayKind::ThreadSnapshot),
        ),
        (
            thread_id.to_string(),
            "turn-1",
            "compact-1",
            "Decoding",
            Some(ReplayKind::ResumeInitialMessages),
        ),
    ] {
        chat.handle_server_notification(compaction_status(&thread, turn, item, message), replay);
        assert_eq!(
            (
                chat.bottom_pane.status_widget().unwrap().header(),
                chat.status_state.compaction.as_ref().unwrap().started_at,
                chat.status_state
                    .compaction
                    .as_ref()
                    .unwrap()
                    .status_message
                    .as_deref(),
            ),
            ("Reconnecting", started_at, Some("Decoding")),
        );
    }
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    assert_eq!(
        chat.bottom_pane.status_widget().unwrap().header(),
        "Decoding"
    );
    assert!(drain_insert_history(&mut rx).is_empty());
}

#[tokio::test]
async fn replayed_completion_for_other_turn_preserves_decoding_timer() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    handle_turn_started(&mut chat, "turn-1");
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    chat.handle_server_notification(
        compaction_status(&thread_id.to_string(), "turn-1", "compact-1", "Decoding"),
        /*replay_kind*/ None,
    );
    let started_at = chat.status_state.compaction.as_ref().unwrap().started_at;
    let ServerNotification::ItemCompleted(mut completed) = compaction_completed("compact-1") else {
        unreachable!();
    };
    completed.turn_id = "older-turn".into();
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(completed),
        Some(ReplayKind::ThreadSnapshot),
    );
    assert_eq!(
        (
            chat.bottom_pane.status_widget().unwrap().header(),
            chat.status_state.compaction.as_ref().unwrap().started_at,
        ),
        ("Decoding", started_at),
    );
    let lines = drain_insert_history(&mut rx)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    insta::assert_snapshot!(lines_to_single_string(&lines), @"• Context compacted");
}

#[tokio::test]
async fn decoding_status_clears_on_terminal_turn_without_completion() {
    for status in [
        AppServerTurnStatus::Interrupted,
        AppServerTurnStatus::Failed,
    ] {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        handle_turn_started(&mut chat, "turn-1");
        chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
        chat.handle_server_notification(
            compaction_status(&thread_id.to_string(), "turn-1", "compact-1", "Decoding"),
            /*replay_kind*/ None,
        );
        chat.handle_server_notification(
            ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: thread_id.to_string(),
                turn: app_server_turn(
                    "turn-1", status, /*duration_ms*/ None, /*error*/ None,
                ),
            }),
            /*replay_kind*/ None,
        );
        chat.handle_server_notification(
            compaction_status(&thread_id.to_string(), "turn-1", "compact-1", "Decoding"),
            /*replay_kind*/ None,
        );
        assert!(chat.status_state.compaction.is_none());
        assert!(!chat.bottom_pane.status_indicator_visible());
        let lines = drain_insert_history(&mut rx)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert!(!lines_to_single_string(&lines).contains("Decoding"));
    }
}

#[tokio::test]
async fn compaction_payload_keeps_live_duration_and_full_detail() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    chat.status_state.compaction.as_mut().unwrap().started_at =
        Instant::now() - Duration::from_secs(/*secs*/ 83);
    let ServerNotification::ItemCompleted(mut completed) = compaction_completed("compact-1") else {
        unreachable!();
    };
    completed.item = AppServerThreadItem::ContextCompaction {
        id: "compact-1".into(),
        summary: Some("Short summary".into()),
        message: Some("Full prompt\n\nLast line".into()),
        available_skills: vec!["test-tui".into()],
        decode_error: None,
    };
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(completed),
        /*replay_kind*/ None,
    );
    let lines = drain_insert_history(&mut rx)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let rendered = normalize_compaction_snapshot(lines_to_single_string(&lines));
    insta::assert_snapshot!(rendered.lines().map(str::trim_end).collect::<Vec<_>>().join("\n"), @"
    • Context compacted · <elapsed>
      Full prompt

      Last line
    ");
    assert!(chat.status_state.compaction.is_none());
}

#[tokio::test]
async fn compaction_empty_message_falls_back_to_summary() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let ServerNotification::ItemCompleted(mut completed) = compaction_completed("old") else {
        unreachable!();
    };
    completed.item = AppServerThreadItem::ContextCompaction {
        id: "old".into(),
        summary: Some("Retained summary".into()),
        message: Some(" \n ".into()),
        available_skills: Vec::new(),
        decode_error: None,
    };
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(completed),
        Some(ReplayKind::ThreadSnapshot),
    );
    let lines = drain_insert_history(&mut rx)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    insta::assert_snapshot!(lines_to_single_string(&lines), @"
    • Context compacted
      Retained summary
    ");
    assert!(chat.status_state.compaction.is_none());
}

#[tokio::test]
async fn compaction_status_survives_follow_up_and_preserves_turn_time() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    handle_turn_started(&mut chat, "turn-1");
    chat.bottom_pane
        .reset_status_timer(Duration::from_secs(/*secs*/ 600));
    chat.on_agent_message_delta("Previous commentary\n".to_string());
    chat.on_commit_tick();
    assert!(!chat.bottom_pane.status_indicator_visible());
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    chat.on_commit_tick();
    assert!(chat.bottom_pane.status_indicator_visible());

    let started_at = Instant::now() - Duration::from_secs(/*secs*/ 83);
    chat.status_state.compaction.as_mut().unwrap().started_at = started_at;
    chat.bottom_pane.set_status_timer_origin(Some(started_at));
    // Repeated start notifications must not restart the displayed timer.
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    assert_eq!(
        chat.status_state.compaction.as_ref().unwrap().started_at,
        started_at
    );
    assert_chatwidget_snapshot!(
        "compaction_running",
        normalize_compaction_snapshot(render_bottom_popup(&chat, /*width*/ 80))
    );
    assert_chatwidget_snapshot!(
        "compaction_running_narrow",
        normalize_compaction_snapshot(render_bottom_popup(&chat, /*width*/ 40))
    );

    chat.handle_composer_input_result(
        InputResult::Submitted {
            text: "keep going".to_string(),
            text_elements: Vec::new(),
        },
        /*had_modal_or_popup*/ false,
    );
    assert_matches!(next_submit_op(&mut op_rx), Op::UserTurn { .. });
    assert_eq!(chat.input_queue.pending_steers.len(), 1);
    assert_eq!(
        chat.bottom_pane.status_widget().unwrap().header(),
        "Compacting context"
    );
    drain_insert_history(&mut rx);

    chat.handle_server_notification(compaction_completed("compact-1"), /*replay_kind*/ None);
    let history = drain_insert_history(&mut rx);
    let lines: Vec<_> = history.into_iter().flatten().collect();
    assert_chatwidget_snapshot!(
        "compaction_completed",
        normalize_compaction_snapshot(lines_to_single_string(&lines))
    );
    assert!(chat.status_state.compaction.is_none());
    assert_eq!(
        chat.bottom_pane.status_widget().unwrap().header(),
        "Working"
    );
    assert!(chat.bottom_pane.status_elapsed().unwrap() >= Duration::from_secs(/*secs*/ 600));
}

#[tokio::test]
async fn manual_compaction_shows_status_before_backend_events() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.dispatch_command(SlashCommand::Compact);
    assert_chatwidget_snapshot!(
        "manual_compaction_pending",
        normalize_compaction_snapshot(render_bottom_popup(&chat, /*width*/ 80))
    );
    assert!(chat.handle_turn_start_rejection("Could not start compaction".to_string()));
    assert!(!chat.bottom_pane.status_indicator_visible());
}

#[tokio::test]
async fn compaction_status_clears_when_turn_ends_without_item_completion() {
    for status in [
        AppServerTurnStatus::Completed,
        AppServerTurnStatus::Interrupted,
        AppServerTurnStatus::Failed,
    ] {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        handle_turn_started(&mut chat, "turn-1");
        chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
        chat.handle_server_notification(
            ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn: app_server_turn(
                    "turn-1", status, /*duration_ms*/ None, /*error*/ None,
                ),
            }),
            /*replay_kind*/ None,
        );
        assert!(chat.status_state.compaction.is_none());
        assert!(!chat.bottom_pane.status_indicator_visible());
        let history = drain_insert_history(&mut rx);
        let lines: Vec<_> = history.into_iter().flatten().collect();
        assert!(!lines_to_single_string(&lines).contains("Context compacted"));
        handle_turn_started(&mut chat, "turn-2");
        assert_eq!(
            chat.bottom_pane.status_widget().unwrap().header(),
            "Working"
        );
    }
}

#[tokio::test]
async fn compaction_history_does_not_start_a_timer_or_finish_live_compaction() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.handle_server_notification(
        compaction_started("old"),
        Some(ReplayKind::ResumeInitialMessages),
    );
    assert!(chat.status_state.compaction.is_none());
    assert!(!chat.bottom_pane.status_indicator_visible());
    handle_turn_started(&mut chat, "turn-1");
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    chat.handle_server_notification(
        compaction_completed("old"),
        Some(ReplayKind::ThreadSnapshot),
    );
    assert_eq!(
        chat.status_state.compaction.as_ref().unwrap().id,
        "compact-1"
    );
    let history = drain_insert_history(&mut rx);
    let lines: Vec<_> = history.into_iter().flatten().collect();
    assert_eq!(lines_to_single_string(&lines).trim(), "• Context compacted");
}

#[tokio::test]
async fn compaction_snapshot_restores_elapsed_time_and_clears_on_replayed_completion() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    let ServerNotification::ItemStarted(mut started) = compaction_started("compact-1") else {
        unreachable!();
    };
    started.started_at_ms -= 12_000;
    chat.handle_server_notification(
        ServerNotification::ItemStarted(started),
        Some(ReplayKind::ThreadSnapshot),
    );
    assert_eq!(
        chat.bottom_pane.status_widget().unwrap().header(),
        "Compacting context"
    );
    assert!(
        chat.status_state
            .compaction
            .as_ref()
            .unwrap()
            .started_at
            .elapsed()
            >= Duration::from_secs(/*secs*/ 12)
    );

    chat.handle_server_notification(
        compaction_completed("compact-1"),
        Some(ReplayKind::ThreadSnapshot),
    );
    assert!(chat.status_state.compaction.is_none());
    assert_eq!(
        chat.bottom_pane.status_widget().unwrap().header(),
        "Working"
    );
    let lines: Vec<_> = drain_insert_history(&mut rx)
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(lines_to_single_string(&lines).trim(), "• Context compacted");
}

#[tokio::test]
async fn compaction_retry_status_returns_to_compacting() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    chat.handle_server_notification(
        ServerNotification::Error(ErrorNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            will_retry: true,
            error: codex_app_server_protocol::TurnError {
                message: "Reconnecting".to_string(),
                codex_error_info: None,
                additional_details: None,
                misalignment: None,
            },
        }),
        /*replay_kind*/ None,
    );
    assert_eq!(
        chat.bottom_pane.status_widget().unwrap().header(),
        "Reconnecting"
    );
    chat.handle_server_notification(compaction_started("compact-1"), /*replay_kind*/ None);
    assert_eq!(
        chat.bottom_pane.status_widget().unwrap().header(),
        "Compacting context"
    );
}
