use super::session_lifecycle_requests::recorded_params;
use super::session_lifecycle_requests::start_recording_app_server;
use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn quit_from_child_view_unloads_primary_root_instead_of_unsubscribing() -> Result<()> {
    let (mut app, _, _) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let root = server.start_thread(&app.config).await?;
    app.primary_thread_id = Some(root.session.thread_id);
    app.active_thread_id = Some(ThreadId::new());
    let result = app
        .handle_exit_mode(&mut server, ExitMode::ShutdownFirst)
        .await;
    assert_matches!(result, AppRunControl::Exit(ExitReason::UserRequested));
    assert_eq!(
        recorded_params(&requests, "thread/unload"),
        vec![serde_json::json!({"threadId": root.session.thread_id.to_string()})]
    );
    assert!(recorded_params(&requests, "thread/unsubscribe").is_empty());
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn quit_discards_side_conversations_after_owning_root_unload() -> Result<()> {
    let (mut app, _, _) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let root = server.start_thread(&app.config).await?;
    let side = server.start_thread(&app.config).await?;
    let root_id = root.session.thread_id;
    let side_id = side.session.thread_id;
    app.primary_thread_id = Some(root_id);
    app.active_thread_id = Some(root_id);
    app.side_threads
        .insert(side_id, SideThreadState::new(root_id));

    let result = app
        .handle_exit_mode(&mut server, ExitMode::ShutdownFirst)
        .await;
    assert_matches!(result, AppRunControl::Exit(ExitReason::UserRequested));
    assert!(app.side_threads.is_empty());
    assert_eq!(
        recorded_params(&requests, "thread/unload"),
        vec![serde_json::json!({"threadId": root_id.to_string()})]
    );
    assert_eq!(
        recorded_params(&requests, "thread/unsubscribe"),
        vec![serde_json::json!({"threadId": side_id.to_string()})]
    );
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn embedded_disconnect_refuses_before_canceling_client_work() -> Result<()> {
    let (mut app, mut events, _) = make_test_app_with_channels().await;
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    app.app_server_target = AppServerTarget::Embedded;
    let request_id = AppServerRequestId::Integer(1);
    app.dynamic_tool_tasks.insert(
        request_id.clone(),
        (
            ThreadId::new().to_string(),
            tokio::spawn(std::future::pending::<()>()),
        ),
    );
    while events.try_recv().is_ok() {}
    let result = app
        .handle_exit_mode(&mut server, ExitMode::Disconnect)
        .await;
    assert_matches!(result, AppRunControl::Continue);
    assert!(!app.dynamic_tool_tasks[&request_id].1.is_finished());
    let AppEvent::InsertHistoryCell(cell) = events.try_recv()? else {
        panic!("disconnect refusal should be visible");
    };
    let message = lines_to_single_string(&cell.display_lines(/*width*/ 250));
    insta::assert_snapshot!(message, @"■ Cannot disconnect: this TUI owns its embedded server, so exiting cannot leave work running. Use /quit to stop and unload the task.");
    // Stable embedded-exit confirmation and recovery routing must not intercept detach.
    let side_id = ThreadId::new();
    app.side_threads
        .insert(side_id, SideThreadState::new(ThreadId::new()));
    app.thread_event_channels.insert(
        side_id,
        ThreadEventChannel::new_with_session(
            THREAD_EVENT_CHANNEL_CAPACITY,
            test_thread_session(side_id, test_path_buf("/tmp/project")),
            vec![test_turn("side-turn", TurnStatus::InProgress, Vec::new())],
        ),
    );
    app.history_recovery_required.insert(side_id);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    for event in [
        AppEvent::Exit(ExitMode::Disconnect),
        AppEvent::RunningTaskExit {
            action: RunningTaskExitAction::RunInBackground,
            thread_id: side_id,
        },
    ] {
        assert_matches!(
            app.handle_event(&mut tui, &mut server, event).await?,
            AppRunControl::Continue
        );
        assert!(!app.dynamic_tool_tasks[&request_id].1.is_finished());
        let AppEvent::InsertHistoryCell(cell) = events.try_recv()? else {
            panic!("routed disconnect refusal should be visible");
        };
        assert_eq!(
            lines_to_single_string(&cell.display_lines(/*width*/ 250)),
            message
        );
    }
    for (_, (_, task)) in app.dynamic_tool_tasks.drain() {
        task.abort();
    }
    server.shutdown().await?;
    Ok(())
}
