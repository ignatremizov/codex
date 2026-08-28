use super::*;
use codex_app_server_protocol::CommandAction;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn primary_attachment_distinguishes_active_observed_and_normalized_cold_history() {
    for (turn_status, external_writer, terminal_is_live) in [
        (TurnStatus::InProgress, false, true),
        (TurnStatus::InProgress, true, true),
        (TurnStatus::Interrupted, false, false),
    ] {
        let (mut app, mut events, mut ops) = make_test_app_with_channels().await;
        let thread_id = ThreadId::new();
        app.chat_widget
            .set_queue_autosend_suppressed(/*suppressed*/ true);
        if external_writer {
            app.chat_widget.show_external_writer_thread();
        }
        app.enqueue_primary_thread_session(
            test_thread_session(thread_id, test_path_buf("/tmp/project")),
            vec![test_turn(
                "turn-1",
                turn_status,
                vec![ThreadItem::CommandExecution {
                    id: "exec-1".to_string(),
                    model_context: None,
                    plugin_id: None,
                    script_path: None,
                    command: "sleep 20".to_string(),
                    cwd: test_path_buf("/tmp/project").abs().into(),
                    process_id: Some("123".to_string()),
                    source: CommandExecutionSource::UnifiedExecStartup,
                    user_shell_response_handling: None,
                    status: CommandExecutionStatus::InProgress,
                    command_actions: vec![CommandAction::Unknown {
                        command: "sleep 20".to_string(),
                    }],
                    aggregated_output: None,
                    exit_code: None,
                    duration_ms: None,
                }],
            )],
        )
        .await
        .expect("primary attachment");
        if external_writer {
            // Startup applies the frozen foreign-writer presentation after initial replay.
            app.chat_widget.show_external_writer_thread();
        }
        assert_eq!(
            (
                app.chat_widget.is_task_running_for_test(),
                app.chat_widget.is_external_writer_view(),
            ),
            (terminal_is_live && !external_writer, external_writer),
        );
        while events.try_recv().is_ok() {}
        app.chat_widget.add_ps_output();
        let mut process_list = String::new();
        while let Ok(event) = events.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = event {
                process_list.push_str(&lines_to_single_string(
                    &cell.transcript_lines(/*width*/ 80),
                ));
            }
        }
        assert_eq!(process_list.contains("sleep 20"), terminal_is_live);
        assert_eq!(
            process_list.contains("No background terminals running."),
            !terminal_is_live,
        );
        while let Ok(op) = ops.try_recv() {
            assert!(!matches!(op, Op::UserTurn { .. }));
        }
    }
}

#[tokio::test]
async fn buffered_terminal_starts_follow_attachment_intent_not_queue_autosend() {
    for replay_kind in [
        ReplayKind::ThreadSnapshot,
        ReplayKind::ReplayOnlyThreadSnapshot,
    ] {
        let (mut app, mut events, _ops) = make_test_app_with_channels().await;
        let thread_id = ThreadId::new();
        app.replay_thread_snapshot_with_kind(
            ThreadEventSnapshot {
                session: None,
                delegated_turns: Vec::new(),
                turns: Vec::new(),
                events: vec![
                    ThreadBufferedEvent::Notification(Box::new(turn_started_notification(
                        thread_id, "turn-1",
                    ))),
                    ThreadBufferedEvent::Notification(Box::new(ServerNotification::ItemStarted(
                        ItemStartedNotification {
                            thread_id: thread_id.to_string(),
                            turn_id: "turn-1".to_string(),
                            started_at_ms: 0,
                            deadline_at_ms: Some(i64::MAX),
                            item: ThreadItem::CommandExecution {
                                id: "exec-1".to_string(),
                                model_context: None,
                                plugin_id: None,
                                script_path: None,
                                command: "sleep 20".to_string(),
                                cwd: test_path_buf("/tmp/project").abs().into(),
                                process_id: Some("123".to_string()),
                                source: CommandExecutionSource::UnifiedExecStartup,
                                user_shell_response_handling: None,
                                status: CommandExecutionStatus::InProgress,
                                command_actions: Vec::new(),
                                aggregated_output: None,
                                exit_code: None,
                                duration_ms: None,
                            },
                        },
                    ))),
                    ThreadBufferedEvent::Notification(Box::new(
                        ServerNotification::CommandExecutionOutputDelta(
                            codex_app_server_protocol::CommandExecutionOutputDeltaNotification {
                                thread_id: thread_id.to_string(),
                                turn_id: "turn-1".to_string(),
                                item_id: "exec-1".to_string(),
                                delta: "retained output\n".to_string(),
                            },
                        ),
                    )),
                ],
                active_reasoning_item: None,
                active_turn_timing: None,
                input_state: None,
            },
            /*resume_restored_queue*/ false,
            replay_kind,
        );
        assert_eq!(
            app.chat_widget.is_task_running_for_test(),
            replay_kind == ReplayKind::ThreadSnapshot,
        );
        let mut audit = String::new();
        while let Ok(event) = events.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = event {
                audit.push_str(&lines_to_single_string(
                    &cell.transcript_lines(/*width*/ 80),
                ));
            }
        }
        if replay_kind == ReplayKind::ReplayOnlyThreadSnapshot {
            assert!(
                audit.contains("sleep 20"),
                "historical buffered command must be retained"
            );
            assert!(
                audit.contains("retained output"),
                "historical output must survive without a live task"
            );
        }
        app.chat_widget.add_ps_output();
        let mut process_list = String::new();
        while let Ok(event) = events.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = event {
                process_list.push_str(&lines_to_single_string(
                    &cell.transcript_lines(/*width*/ 80),
                ));
            }
        }
        assert_eq!(
            process_list.contains("No background terminals running."),
            replay_kind == ReplayKind::ReplayOnlyThreadSnapshot,
        );
    }
}

#[tokio::test]
async fn cold_snapshot_keeps_draft_and_queue_without_restoring_turn_ownership() {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let thread_id = ThreadId::new();
    app.chat_widget.handle_thread_session(test_thread_session(
        thread_id,
        test_path_buf("/tmp/project"),
    ));
    app.chat_widget.handle_server_notification(
        turn_started_notification(thread_id, "turn-1"),
        /*replay_kind*/ None,
    );
    app.chat_widget
        .apply_external_edit("queued follow-up".to_string());
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.chat_widget
        .apply_external_edit("draft prompt".to_string());
    let input_state = app.chat_widget.capture_thread_input_state();
    app.replay_thread_snapshot_with_kind(
        ThreadEventSnapshot {
            session: None,
            delegated_turns: Vec::new(),
            turns: Vec::new(),
            events: Vec::new(),
            active_reasoning_item: None,
            active_turn_timing: None,
            input_state,
        },
        /*resume_restored_queue*/ false,
        ReplayKind::ReplayOnlyThreadSnapshot,
    );
    assert_eq!(
        (
            app.chat_widget.composer_text_with_pending(),
            app.chat_widget.queued_user_message_texts(),
            app.chat_widget.is_task_running_for_test(),
        ),
        (
            "draft prompt".to_string(),
            vec!["queued follow-up".to_string()],
            false
        )
    );
}

#[tokio::test]
async fn replay_only_snapshot_clears_stale_pending_start_for_new_user_turn() {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let thread_id = ThreadId::new();
    let session = test_thread_session(thread_id, test_path_buf("/tmp/project"));
    app.chat_widget.handle_thread_session(session.clone());
    app.chat_widget
        .restore_user_message_to_composer(crate::chatwidget::UserMessage::from("previous"));
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let input_state = app
        .chat_widget
        .capture_thread_input_state()
        .expect("expected pending turn state");

    let (chat_widget, _app_event_tx, _rx, mut new_op_rx) =
        make_chatwidget_manual_with_sender().await;
    app.chat_widget = chat_widget;
    app.replay_thread_snapshot_with_kind(
        ThreadEventSnapshot {
            delegated_turns: Vec::new(),
            active_reasoning_item: None,
            active_turn_timing: None,
            session: Some(session),
            turns: Vec::new(),
            events: Vec::new(),
            input_state: Some(input_state),
        },
        /*resume_restored_queue*/ false,
        ReplayKind::ReplayOnlyThreadSnapshot,
    );
    while new_op_rx.try_recv().is_ok() {}

    app.chat_widget
        .restore_user_message_to_composer(crate::chatwidget::UserMessage::from("continue"));
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(
        next_user_turn_op(&mut new_op_rx),
        Op::UserTurn { .. }
    ));
    assert!(app.chat_widget.queued_user_message_texts().is_empty());
}

#[tokio::test]
async fn replay_only_snapshot_keeps_command_history_without_restoring_background_terminal() {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    app.replay_thread_snapshot_with_kind(
        ThreadEventSnapshot {
            delegated_turns: Vec::new(),
            active_reasoning_item: None,
            active_turn_timing: None,
            session: None,
            turns: vec![test_turn(
                "turn-1",
                TurnStatus::Completed,
                vec![ThreadItem::CommandExecution {
                    id: "exec-1".to_string(),
                    model_context: None,
                    plugin_id: None,
                    script_path: None,
                    command: "sleep 20".to_string(),
                    cwd: test_path_buf("/tmp/project").abs().into(),
                    process_id: Some("123".to_string()),
                    source: CommandExecutionSource::UnifiedExecStartup,
                    user_shell_response_handling: None,
                    status: CommandExecutionStatus::InProgress,
                    command_actions: vec![CommandAction::Unknown {
                        command: "sleep 20".to_string(),
                    }],
                    aggregated_output: None,
                    exit_code: None,
                    duration_ms: None,
                }],
            )],
            events: Vec::new(),
            input_state: None,
        },
        /*resume_restored_queue*/ false,
        ReplayKind::ReplayOnlyThreadSnapshot,
    );

    let mut replayed_history = String::new();
    while let Ok(event) = app_event_rx.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            replayed_history.push_str(&lines_to_single_string(
                &cell.transcript_lines(/*width*/ 80),
            ));
        }
    }
    assert!(
        replayed_history.contains("sleep 20"),
        "replay-only transcript should retain command history: {replayed_history:?}"
    );

    app.chat_widget.add_ps_output();
    let mut process_list = String::new();
    while let Ok(event) = app_event_rx.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            process_list.push_str(&lines_to_single_string(
                &cell.transcript_lines(/*width*/ 80),
            ));
        }
    }
    assert_snapshot!(process_list, @r"
    /ps

    Background terminals

      • No background terminals running.
    ");
}
