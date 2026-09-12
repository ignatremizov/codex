use super::*;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::sub_agent_completion_transcript_with_visibility;
use pretty_assertions::assert_eq;

fn delta(app: &mut App, id: &str, text: &str) {
    app.chat_widget.handle_server_notification(
        ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: id.to_string(),
            delta: text.to_string(),
        }),
        /*replay_kind*/ None,
    );
}

fn completed(app: &mut App, id: String, text: String, phase: MessagePhase) {
    app.chat_widget.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
            item: ThreadItem::AgentMessage {
                id,
                text,
                phase: Some(phase),
                attribution: None,
                input: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            },
        }),
        /*replay_kind*/ None,
    );
}

// Dispatch every event, including provisional stream cells, through the real App path.
async fn dispatch(
    app: &mut App,
    tui: &mut crate::tui::Tui,
    app_server: &mut AppServerSession,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> Result<usize> {
    let mut consolidations = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(&event, AppEvent::ConsolidateAgentMessage { .. }) {
            consolidations += 1;
        }
        Box::pin(app.handle_event(tui, app_server, event)).await?;
    }
    Ok(consolidations)
}

#[tokio::test]
async fn committed_stream_cells_consolidate_once_before_async_notice() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let mut app_server = start_config_write_test_app_server(&app).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let prefix = "Opening line.\n\nUUID fallback and ";
    let answer = "Opening line.\n\nUUID fallback and complete guidance.\n";
    delta(&mut app, "first-answer", prefix);
    for _ in 0..8 {
        app.chat_widget.on_commit_tick();
    }
    assert_eq!(
        dispatch(&mut app, &mut tui, &mut app_server, &mut events).await?,
        0
    );
    assert!(
        app.transcript_cells
            .iter()
            .any(|cell| cell.as_any().is::<AgentMessageCell>()),
        "the regression must exercise already committed stream cells",
    );
    let before_notice = app
        .render_transcript_lines_for_reflow(/*width*/ 80)
        .lines
        .iter()
        .map(rendered_line_text)
        .collect::<Vec<_>>();
    let (notice_id, notice_text) = sub_agent_completion_transcript_with_visibility(
        "/root/reviewer",
        &AgentStatus::Completed(Some("Finished review.".to_string())),
        SubAgentCompletionModelVisibility::NotVisible,
    )
    .expect("completion notice");
    completed(
        &mut app,
        notice_id.to_string(),
        notice_text,
        MessagePhase::Commentary,
    );
    assert_eq!(
        dispatch(&mut app, &mut tui, &mut app_server, &mut events).await?,
        0
    );
    assert_eq!(
        app.render_transcript_lines_for_reflow(/*width*/ 80)
            .lines
            .iter()
            .map(rendered_line_text)
            .collect::<Vec<_>>(),
        before_notice,
        "the notice must neither insert itself nor finalize the partial stream",
    );
    delta(&mut app, "first-answer", "complete guidance.\n");
    for _ in 0..8 {
        app.chat_widget.on_commit_tick();
    }
    completed(
        &mut app,
        "first-answer".to_string(),
        answer.to_string(),
        MessagePhase::FinalAnswer,
    );
    assert_eq!(
        dispatch(&mut app, &mut tui, &mut app_server, &mut events).await?,
        1
    );
    assert_eq!(app.transcript_cells.len(), 2);
    assert!(app.transcript_cells[0].as_any().is::<AgentMarkdownCell>());
    assert!(
        app.transcript_cells[1]
            .as_any()
            .is::<crate::multi_agents::CollabAgentHistoryCell>()
    );
    let rendered_answer = AgentMarkdownCell::new(answer.to_string(), app.config.cwd.as_path())
        .transcript_lines(/*width*/ 80);
    assert_eq!(
        app.transcript_cells[0].transcript_lines(/*width*/ 80),
        rendered_answer,
    );
    let first_render = app
        .render_transcript_lines_for_reflow(/*width*/ 80)
        .lines
        .iter()
        .map(rendered_line_text)
        .collect::<Vec<_>>();
    insta::assert_snapshot!(first_render.join("\n"), @r"
    • Opening line.

      UUID fallback and complete guidance.

    • /root/reviewer completed: (○ not visible)
      └ Finished review.
    ");
    // Equal text from a distinct authored item must remain a second real answer.
    delta(&mut app, "second-answer", answer);
    for _ in 0..8 {
        app.chat_widget.on_commit_tick();
    }
    completed(
        &mut app,
        "second-answer".to_string(),
        answer.to_string(),
        MessagePhase::FinalAnswer,
    );
    assert_eq!(
        dispatch(&mut app, &mut tui, &mut app_server, &mut events).await?,
        1
    );
    assert_eq!(app.transcript_cells.len(), 3);
    assert!(app.transcript_cells[2].as_any().is::<AgentMarkdownCell>());
    assert_eq!(
        app.transcript_cells[2].transcript_lines(/*width*/ 80),
        rendered_answer
    );
    let mut expected = first_render;
    expected.push(String::new());
    expected.extend(
        app.transcript_cells[0]
            .transcript_lines(/*width*/ 80)
            .iter()
            .map(ToString::to_string),
    );
    assert_eq!(
        app.render_transcript_lines_for_reflow(/*width*/ 80)
            .lines
            .iter()
            .map(rendered_line_text)
            .collect::<Vec<_>>(),
        expected,
    );
    Ok(())
}
