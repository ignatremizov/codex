use std::sync::Arc;

use app_test_support::create_fake_rollout;
use codex_protocol::ThreadId;
use color_eyre::eyre::Result;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use tempfile::tempdir;

use super::test_support::make_test_app;
use super::*;
use crate::app_event::ConsolidationScrollbackReflow;
use crate::chatwidget::tests::helpers::set_active_cell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::legacy_core::config::ConfigBuilder;
use crate::pager_overlay::Overlay;

fn render_transcript_overlay(app: &mut App) -> String {
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    let Some(Overlay::Transcript(overlay)) = app.overlay.as_mut() else {
        panic!("expected transcript overlay");
    };
    overlay.render(area, &mut buffer);
    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn user_cell(message: &str) -> Arc<dyn HistoryCell> {
    Arc::new(UserHistoryCell {
        message: message.to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: Vec::new(),
        source: None,
    })
}

#[tokio::test]
async fn agent_transcript_inspection_is_fixed_while_active_browser_tracks_app_updates() -> Result<()>
{
    let codex_home = tempdir()?;
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    let foreign_thread_id = ThreadId::from_string(
        &create_fake_rollout(
            codex_home.path(),
            "2026-09-02T12-00-00",
            "2026-09-02T12:00:00Z",
            "foreign inspection prompt",
            Some(config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .expect("create foreign rollout"),
    )?;
    let mut app_server = crate::start_embedded_app_server_for_picker(&config).await?;
    let mut app = make_test_app().await;
    app.config = config.clone();
    let active = app_server.start_thread(&config).await?;
    let active_thread_id = active.session.thread_id;
    app.chat_widget.handle_thread_session(active.session);
    app.active_thread_id = Some(active_thread_id);
    app.primary_thread_id = Some(active_thread_id);
    app.transcript_cells = vec![user_cell("active transcript prompt")];
    set_active_cell(
        &mut app.chat_widget,
        Box::new(PlainHistoryCell::new(vec![Line::from(
            "active in-flight tail",
        )])),
    );
    let mut tui = crate::tui::test_support::make_test_tui()?;

    app.inspect_agent_transcript(&mut tui, &mut app_server, foreign_thread_id)
        .await;
    assert!(tui.is_alt_screen_active());
    assert!(
        app.overlay
            .as_mut()
            .and_then(Overlay::active_transcript_mut)
            .is_none()
    );
    app.handle_backtrack_overlay_event(&mut tui, &mut app_server, TuiEvent::Draw)
        .await?;
    let inspected = render_transcript_overlay(&mut app);
    assert!(inspected.contains("foreign inspection prompt"));
    assert!(!inspected.contains("active transcript prompt"));
    assert!(!inspected.contains("active in-flight tail"));

    app.insert_history_cell(
        &mut tui,
        Box::new(PlainHistoryCell::new(vec![Line::from(
            "active committed update",
        )])),
    );
    app.transcript_cells.push(Arc::new(AgentMessageCell::new(
        vec![Line::from("active provisional response")],
        /*is_first_line*/ true,
    )));
    let active_cwd = app.config.cwd.to_path_buf();
    app.handle_consolidate_agent_message(
        &mut tui,
        "active consolidated response".to_string(),
        active_cwd,
        /*inline_visualization_context*/ None,
        /*phase*/ None,
        ConsolidationScrollbackReflow::IfResizeReflowRan,
        /*deferred_history_cell*/ None,
    )?;
    let inspected_after_updates = render_transcript_overlay(&mut app);
    assert_eq!(inspected_after_updates, inspected);

    app.handle_backtrack_overlay_event(
        &mut tui,
        &mut app_server,
        TuiEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    )
    .await?;
    assert!(app.overlay.is_some());
    assert!(!app.backtrack.primed);
    assert!(!app.backtrack.overlay_preview_active);
    app.handle_backtrack_overlay_event(
        &mut tui,
        &mut app_server,
        TuiEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    )
    .await?;
    assert!(app.overlay.is_none());
    assert!(!app.backtrack.primed);
    assert!(!app.backtrack.overlay_preview_active);

    app.inspect_agent_transcript(&mut tui, &mut app_server, active_thread_id)
        .await;
    assert!(
        app.overlay
            .as_mut()
            .and_then(Overlay::active_transcript_mut)
            .is_some()
    );
    app.handle_backtrack_overlay_event(&mut tui, &mut app_server, TuiEvent::Draw)
        .await?;
    let active = render_transcript_overlay(&mut app);
    assert!(active.contains("active transcript prompt"));
    assert!(active.contains("active committed update"));
    assert!(active.contains("active consolidated response"));
    assert!(active.contains("active in-flight tail"));
    assert!(!active.contains("foreign inspection prompt"));

    app.handle_backtrack_overlay_event(
        &mut tui,
        &mut app_server,
        TuiEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    )
    .await?;
    assert!(app.overlay.is_some());
    assert!(app.backtrack.primed);
    assert!(app.backtrack.overlay_preview_active);

    app.close_transcript_overlay(&mut tui);
    app_server.shutdown().await?;
    Ok(())
}
