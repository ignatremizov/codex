//! A switch limits native replay without truncating retained source or changing resize policy.

use super::*;
use codex_app_server_protocol::DynamicToolCallStatus;
use pretty_assertions::assert_eq;
use ratatui::layout::Size;

#[tokio::test]
async fn switch_budget_is_independent_of_resize_policy_and_consumed_once() -> Result<()> {
    for (configured, height, switch_rows, resize_rows) in [
        (0, 24, 160, 599),
        (3, 24, 160, 3),
        (500, 24, 160, 500),
        (500, 40, 200, 500),
    ] {
        let (mut app, _events, _ops) = make_test_app_with_channels().await;
        app.local_settings.tui.terminal_resize_reflow_max_rows = Some(configured);
        app.transcript_cells = (0..300)
            .map(|index| plain_line_cell(format!("cell {index}")))
            .collect();
        let sources = app.transcript_cells.clone();
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let size = Size::new(/*width*/ 80, height);
        app.begin_thread_switch_history_replay_buffer(height);
        app.finish_initial_history_replay_buffer(&mut tui);
        app.maybe_run_resize_reflow(&mut tui, size)?;

        let lines = tui.pending_history_lines_for_test();
        assert_eq!(lines.len(), switch_rows);
        assert_eq!(
            lines.last().map(|line| line.line.to_string()),
            Some("cell 299".to_string())
        );
        assert!(!app.transcript_reflow.has_pending_reflow());
        assert!(
            app.transcript_cells
                .iter()
                .zip(&sources)
                .all(|(actual, original)| Arc::ptr_eq(actual, original))
        );
        assert_eq!(app.transcript_cells.len(), sources.len());

        app.schedule_immediate_resize_reflow(&mut tui);
        app.maybe_run_resize_reflow(&mut tui, size)?;
        assert_eq!(tui.pending_history_lines_for_test().len(), resize_rows);
        assert_eq!(
            app.local_settings.tui.terminal_resize_reflow_max_rows,
            Some(configured)
        );

        tui.clear_pending_history_lines();
        app.reset_history_emission_state();
        app.begin_initial_history_replay_buffer();
        for cell in &sources {
            app.insert_history_cell_lines_with_initial_replay_buffer(
                &mut tui,
                cell.as_ref(),
                size.width,
            );
        }
        app.finish_initial_history_replay_buffer(&mut tui);
        assert_eq!(tui.pending_history_lines_for_test().len(), resize_rows);
        assert!(!app.transcript_reflow.has_pending_reflow());
    }
    Ok(())
}

#[tokio::test]
async fn switch_budget_survives_overlay_debounce_and_stream_consolidation() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    app.local_settings.tui.terminal_resize_reflow_max_rows = Some(0);
    app.transcript_cells = (0..300)
        .map(|index| plain_line_cell(format!("cell {index}")))
        .collect();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.open_transcript_overlay(&mut tui);
    app.begin_thread_switch_history_replay_buffer(/*visible_rows*/ 24);
    app.finish_required_stream_reflow(&mut tui)?;
    assert!(app.initial_history_replay_buffer.is_some());
    app.finish_initial_history_replay_buffer(&mut tui);
    let size = Size::new(/*width*/ 28, /*height*/ 24);
    app.maybe_run_resize_reflow(&mut tui, size)?;
    assert!(app.transcript_reflow.has_pending_reflow());
    assert!(tui.pending_history_lines_for_test().is_empty());

    app.close_transcript_overlay(&mut tui);
    app.transcript_reflow
        .schedule_debounced(/*target_width*/ Some(28));
    app.maybe_run_resize_reflow(&mut tui, size)?;
    assert!(app.transcript_reflow.has_pending_reflow());
    assert!(tui.pending_history_lines_for_test().is_empty());
    // Consolidation can request an immediate repair while the switch is still waiting.
    app.transcript_reflow.mark_resize_requested_during_stream();
    app.maybe_finish_stream_reflow(&mut tui)?;
    let lines = tui.pending_history_lines_for_test();
    assert_eq!(lines.len(), 160);
    assert!(!app.transcript_reflow.has_pending_reflow());
    assert_eq!(app.transcript_cells.len(), 300);
    Ok(())
}

#[tokio::test]
async fn switch_does_not_refill_the_deliberately_capped_tail() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let thread_id = ThreadId::new();
    app.chat_widget
        .handle_thread_session(test_thread_session(thread_id, app.config.cwd.to_path_buf()));
    while events.try_recv().is_ok() {}
    app.local_settings.tui.terminal_resize_reflow_max_rows = Some(1000);
    app.scrollback_has_older_history = true;
    app.transcript_cells = (0..100)
        .map(|index| plain_line_cell(format!("cell {index}")))
        .collect();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let size = tui.terminal.last_known_screen_size;
    app.begin_thread_switch_history_replay_buffer(size.height);
    app.finish_initial_history_replay_buffer(&mut tui);
    app.maybe_run_resize_reflow(&mut tui, size)?;
    assert_eq!(tui.pending_history_lines_for_test().len(), 160);
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .all(|event| !matches!(event, AppEvent::RequestOlderScrollbackHistory { .. }))
    );

    app.schedule_immediate_resize_reflow(&mut tui);
    app.maybe_run_resize_reflow(&mut tui, size)?;
    assert!(
        std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
            event,
            AppEvent::RequestOlderScrollbackHistory { thread_id: requested }
                if requested == thread_id
        ))
    );
    assert_eq!(tui.pending_history_lines_for_test().len(), 200);
    Ok(())
}

#[tokio::test]
async fn owned_screen_discards_switch_replay_without_discarding_source() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    app.transcript_cells = (0..300)
        .map(|index| plain_line_cell(format!("cell {index}")))
        .collect();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(/*owned*/ true)?;
    app.begin_thread_switch_history_replay_buffer(/*visible_rows*/ 24);
    app.finish_initial_history_replay_buffer(&mut tui);
    let size = tui.terminal.last_known_screen_size;
    app.maybe_run_resize_reflow(&mut tui, size)?;
    assert_eq!(
        (
            app.initial_history_replay_buffer.is_none(),
            app.transcript_reflow.has_pending_reflow(),
            tui.pending_history_lines_for_test().is_empty(),
            app.transcript_cells.len(),
        ),
        (true, false, true, 300)
    );
    tui.set_owned_screen(/*owned*/ false)?;
    Ok(())
}

#[tokio::test]
async fn switch_replay_preserves_pending_tool_barrier_and_drains_settled_owners() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.transcript_cells = (0..100)
        .map(|index| plain_line_cell(format!("cell {index}")))
        .collect();
    for cell in &app.transcript_cells {
        app.native_history.defer(cell);
    }
    let item = |status| ThreadItem::DynamicToolCall {
        id: "lookup".into(),
        namespace: None,
        tool: "lookup".into(),
        arguments: serde_json::json!({}),
        status,
        success: None,
        duration_ms: None,
        content_items: None,
    };
    let tool =
        history_cell::DynamicToolCallCell::from_item(item(DynamicToolCallStatus::InProgress))
            .expect("dynamic tool cell");
    app.insert_history_cell(&mut tui, Box::new(tool.clone()));
    app.insert_history_cell(
        &mut tui,
        Box::new(PlainHistoryCell::new(vec!["after tool".into()])),
    );
    app.begin_thread_switch_history_replay_buffer(/*visible_rows*/ 24);
    app.finish_initial_history_replay_buffer(&mut tui);
    let size = tui.terminal.last_known_screen_size;
    app.maybe_run_resize_reflow(&mut tui, size)?;
    let replayed = tui.pending_history_lines_for_test();
    assert_eq!(replayed.len(), 160);
    assert_eq!(
        replayed.last().map(|line| line.line.to_string()),
        Some("cell 99".to_string())
    );
    app.flush_native_history(&mut tui);
    assert_eq!(tui.pending_history_lines_for_test(), replayed);
    assert_eq!(app.transcript_cells.len(), 102);

    tool.update_from_item(item(DynamicToolCallStatus::Completed));
    app.flush_native_history(&mut tui);
    let settled = tui.pending_history_lines_for_test();
    assert_eq!(&settled[..replayed.len()], replayed.as_slice());
    assert_eq!(
        settled.last().map(|line| line.line.to_string()),
        Some("after tool".to_string())
    );
    app.flush_native_history(&mut tui);
    assert_eq!(tui.pending_history_lines_for_test(), settled);
    Ok(())
}
