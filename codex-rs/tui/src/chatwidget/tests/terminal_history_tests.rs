use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn cold_buffered_completion_retains_output_without_a_running_task() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: thread_id(&chat),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
            item: AppServerThreadItem::CommandExecution {
                id: "exec-1".to_string(),
                model_context: None,
                plugin_id: None,
                script_path: None,
                command: "printf hello".to_string(),
                cwd: chat.config.cwd.clone().into(),
                process_id: Some("123".to_string()),
                source: AppServerCommandExecutionSource::UnifiedExecStartup,
                status: AppServerCommandExecutionStatus::Completed,
                command_actions: Vec::new(),
                aggregated_output: Some("hello\n".to_string()),
                exit_code: Some(0),
                duration_ms: Some(10),
            },
        }),
        Some(ReplayKind::ReplayOnlyThreadSnapshot),
    );
    chat.finalize_replayed_process_tracking();
    let rendered = drain_insert_history_transcript(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    insta::assert_snapshot!(rendered, @"
    $ printf hello
    hello
    ✓ • 10ms
    ");
    assert_eq!(
        (
            chat.bottom_pane.is_task_running(),
            chat.unified_exec_processes.len(),
            chat.completed_unified_exec_processes.len(),
            chat.status_state.countdown_owner.clone(),
        ),
        (false, 0, 0, None)
    );
}

#[tokio::test]
async fn completed_terminal_checks_require_original_identity_and_preserve_other_countdown() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    chat.track_unified_exec_process_begin("exec-old", Some("123"), "sleep 20");
    chat.track_unified_exec_process_end("exec-old", Some("123"));
    let other_owner = StatusCountdownOwner::CollabWait {
        turn_id: "turn-1".to_string(),
        call_id: "agent-wait".to_string(),
    };
    chat.set_status_countdown_deadline_at_ms(
        other_owner.clone(),
        chrono::Utc::now().timestamp_millis() + 60_000,
    );
    drain_insert_history(&mut rx);

    terminal_interaction(&mut chat, "unknown", "123", "");
    assert!(drain_insert_history_transcript(&mut rx).is_empty());
    terminal_interaction(&mut chat, "exec-old", "123", "");
    let rendered = drain_insert_history_transcript(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    insta::assert_snapshot!(rendered, @"• Checked background terminal output · sleep 20");
    assert_eq!(chat.status_state.countdown_owner, Some(other_owner));

    chat.track_unified_exec_process_begin("exec-new", Some("123"), "cat");
    let new_owner = StatusCountdownOwner::UnifiedExec {
        turn_id: "turn-1".to_string(),
        item_id: "exec-new".to_string(),
        process_id: "123".to_string(),
    };
    chat.set_status_countdown_deadline_at_ms(
        new_owner.clone(),
        chrono::Utc::now().timestamp_millis() + 60_000,
    );
    terminal_interaction(&mut chat, "exec-old", "123", "");
    chat.track_unified_exec_process_end("exec-old", Some("123"));
    assert_eq!(
        chat.unified_exec_processes
            .iter()
            .map(|process| (
                process.key.as_str(),
                process.call_id.as_str(),
                process.command_display.as_str(),
            ))
            .collect::<Vec<_>>(),
        vec![("123", "exec-new", "cat")]
    );
    assert!(chat.completed_unified_exec_processes.is_empty());
    assert_eq!(chat.status_state.countdown_owner, Some(new_owner));
    assert!(drain_insert_history_transcript(&mut rx).is_empty());
}

#[tokio::test]
async fn completed_terminal_cache_is_bounded_and_cold_replay_discards_runtime_cache() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    for index in 0..17 {
        let id = format!("exec-{index}");
        chat.track_unified_exec_process_begin(&id, Some(&id), "cat");
        chat.track_unified_exec_process_end(&id, Some(&id));
    }
    assert_eq!(chat.completed_unified_exec_processes.len(), 16);
    terminal_interaction(&mut chat, "exec-0", "exec-0", "");
    assert!(drain_insert_history_transcript(&mut rx).is_empty());
    terminal_interaction(&mut chat, "exec-16", "exec-16", "");
    assert_eq!(drain_insert_history_transcript(&mut rx).len(), 1);
    chat.replay_thread_turns(Vec::new(), ReplayKind::ResumeInitialMessages);
    assert!(chat.completed_unified_exec_processes.is_empty());
    assert!(chat.status_state.countdown_owner.is_none());
}

#[tokio::test]
async fn replayed_command_execution_is_visible_in_transcript() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.replay_thread_turns(
        vec![AppServerTurn {
            id: "turn-1".to_string(),
            items: vec![AppServerThreadItem::CommandExecution {
                id: "exec-1".to_string(),
                model_context: None,
                plugin_id: None,
                script_path: None,
                command: "sleep 20".to_string(),
                cwd: test_path_buf("/home/user/project").abs().into(),
                process_id: None,
                source: AppServerCommandExecutionSource::UnifiedExecStartup,
                status: AppServerCommandExecutionStatus::Completed,
                command_actions: vec![AppServerCommandAction::Unknown {
                    command: "sleep 20".to_string(),
                }],
                aggregated_output: None,
                exit_code: Some(0),
                duration_ms: Some(20_000),
            }],
            items_view: codex_app_server_protocol::TurnItemsView::Full,
            status: AppServerTurnStatus::Completed,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }],
        ReplayKind::ResumeInitialMessages,
    );

    let rendered = drain_insert_history_transcript(&mut rx)
        .into_iter()
        .map(|lines| lines_to_single_string(&lines))
        .collect::<String>();
    insta::assert_snapshot!(rendered, @"
    $ sleep 20
    ✓ • 20.00s
    ");
}

#[tokio::test]
async fn resumed_history_keeps_command_without_restoring_background_terminal() {
    for replay_kind in [
        ReplayKind::ResumeInitialMessages,
        ReplayKind::ThreadSnapshot,
    ] {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.replay_thread_turns(
            vec![AppServerTurn {
                id: "turn-1".to_string(),
                items: vec![AppServerThreadItem::CommandExecution {
                    id: "exec-1".to_string(),
                    model_context: None,
                    plugin_id: None,
                    script_path: None,
                    command: "sleep 20".to_string(),
                    cwd: test_path_buf("/home/user/project").abs().into(),
                    process_id: Some("123".to_string()),
                    source: AppServerCommandExecutionSource::UnifiedExecStartup,
                    status: AppServerCommandExecutionStatus::InProgress,
                    command_actions: vec![AppServerCommandAction::Unknown {
                        command: "sleep 20".to_string(),
                    }],
                    aggregated_output: None,
                    exit_code: None,
                    duration_ms: None,
                }],
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                status: AppServerTurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            }],
            replay_kind,
        );

        assert_eq!(
            (
                chat.unified_exec_processes.len(),
                chat.completed_unified_exec_processes.len(),
            ),
            (0, 0)
        );
        chat.add_ps_output();
        let rendered = drain_insert_history_transcript(&mut rx)
            .into_iter()
            .map(|lines| lines_to_single_string(&lines))
            .collect::<String>();
        let rendered = regex_lite::Regex::new(r"(?m) • (?:\d+ms|\d+\.\d+s|\d+m \d+s)$")
            .expect("valid duration regex")
            .replace(&rendered, " • <duration>");
        insta::assert_snapshot!(rendered, @r"
    $ sleep 20
    ✗ (1) • <duration>

    /ps

    Background terminals

      • No background terminals running.
    ");
    }
}

#[tokio::test]
async fn terminal_ownership_is_independent_of_initial_resume_task_lifecycle() {
    for (replay_kind, task_running, process_count) in [
        (ReplayKind::ThreadSnapshot, true, 1),
        (ReplayKind::ResumeInitialMessages, true, 0),
        (ReplayKind::ReplayOnlyThreadSnapshot, false, 0),
    ] {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.replay_thread_turns(
            vec![AppServerTurn {
                id: "turn-1".to_string(),
                items: vec![AppServerThreadItem::CommandExecution {
                    id: "exec-1".to_string(),
                    model_context: None,
                    plugin_id: None,
                    script_path: None,
                    command: "sleep 20".to_string(),
                    cwd: test_path_buf("/home/user/project").abs().into(),
                    process_id: Some("123".to_string()),
                    source: AppServerCommandExecutionSource::UnifiedExecStartup,
                    status: AppServerCommandExecutionStatus::InProgress,
                    command_actions: vec![AppServerCommandAction::Unknown {
                        command: "sleep 20".to_string(),
                    }],
                    aggregated_output: None,
                    exit_code: None,
                    duration_ms: None,
                }],
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                status: AppServerTurnStatus::InProgress,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            }],
            replay_kind,
        );

        assert_eq!(
            (
                chat.bottom_pane.is_task_running(),
                chat.unified_exec_processes.len(),
                chat.completed_unified_exec_processes.len(),
            ),
            (task_running, process_count, 0)
        );
        chat.add_ps_output();
        let rendered = drain_insert_history_transcript(&mut rx)
            .into_iter()
            .map(|lines| lines_to_single_string(&lines))
            .collect::<String>();
        assert!(rendered.contains("sleep 20"));
        assert_eq!(
            rendered.contains("No background terminals running."),
            process_count == 0
        );
    }
}
