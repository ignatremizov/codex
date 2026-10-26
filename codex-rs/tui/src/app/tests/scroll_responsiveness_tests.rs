//! Fast wheel input updates navigation without forcing a terminal repaint for every event.

use super::*;
use crate::custom_terminal::test_support::last_rendered_buffer;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use pretty_assertions::assert_eq;
use ratatui::layout::Size;

#[tokio::test]
async fn wheel_burst_and_interrupt_shortcuts_wait_for_the_scheduled_repaint() -> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    app.config.cwd = app.config.codex_home.clone();
    let config = serde_json::from_value(serde_json::json!({
        "chat": {"interrupt_turn": ["f9"]}
    }))?;
    app.keymap = RuntimeKeymap::from_config(&config).expect("valid interrupt binding");
    app.local_settings.tui.keymap = config.clone();
    app.chat_widget.apply_keymap_update(config, &app.keymap);
    app.local_settings.tui.mouse_scroll_speed = Some(3.0);
    app.transcript_cells = (0..512)
        .map(|index| -> Arc<dyn HistoryCell> {
            Arc::new(PlainHistoryCell::new(vec![
                format!("History message {index:03}").into(),
                "Message body".into(),
            ]))
        })
        .collect();
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(/*owned*/ true)?;
    let size = Size::new(/*width*/ 80, /*height*/ 12);
    tui.screen_size_for_event(&TuiEvent::Resize(size))?;
    app.render_owned_transcript(&mut tui, size)?;
    let before = last_rendered_buffer(&tui.terminal).clone();
    for _ in 0..256 {
        app.handle_tui_event(
            &mut tui,
            &mut server,
            TuiEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .await?;
    }
    assert!(!app.transcript_view.is_following());
    assert_eq!(last_rendered_buffer(&tui.terminal), &before);
    for key in [
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        KeyCode::F(9).into(),
    ] {
        assert!(!app.handle_owned_transcript_event(&mut tui, &mut server, &TuiEvent::Key(key))?);
        assert_eq!(last_rendered_buffer(&tui.terminal), &before);
    }
    app.handle_tui_event(&mut tui, &mut server, TuiEvent::Draw)
        .await?;
    let after = last_rendered_buffer(&tui.terminal);
    assert_ne!(after, &before);
    let visible = after
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(visible.contains("History message"));
    assert!(!visible.contains("History message 511"));
    tui.set_owned_screen(/*owned*/ false)?;
    server.shutdown().await?;
    Ok(())
}
