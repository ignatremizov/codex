//! Copy visible completion text without completing the command or changing its draft.

use super::*;
use crate::app::owned_transcript::tests::buffer_text;
use crate::bottom_pane::AgentPromptTarget;
use codex_config::types::CopyOnSelect;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use pretty_assertions::assert_eq;
use ratatui::style::Modifier;

#[tokio::test]
async fn agent_completion_uuid_selection_preserves_the_menu_and_draft() -> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    app.local_settings.tui.copy_on_select = CopyOnSelect::Never;
    app.right_click_paste_environment.primary = false;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(/*owned*/ true)?;
    let size = tui.terminal.size()?;
    let uuid = "01a1b2c9-a8f2-7951-adbf-33128826d45c";
    app.chat_widget
        .set_agent_prompt_targets(vec![AgentPromptTarget {
            thread_id: Some(ThreadId::from_string(uuid)?),
            selector: "714".into(),
            label: "Turing the 8th".into(),
        }]);
    app.chat_widget.apply_external_edit("/agent 714".into());
    app.render_owned_transcript(&mut tui, size)?;
    assert!(!app.chat_widget.no_modal_or_popup_active());
    let draft = app.chat_widget.capture_thread_input_state();
    let screen = buffer_text(crate::custom_terminal::test_support::last_rendered_buffer(
        &tui.terminal,
    ));
    let (row, column) = screen
        .lines()
        .enumerate()
        .find_map(|(row, line)| line.find(uuid).map(|column| (row as u16, column as u16)))
        .ok_or_else(|| {
            color_eyre::eyre::eyre!("agent UUID missing from completion menu: {screen}")
        })?;
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), column),
        (
            MouseEventKind::Drag(MouseButton::Left),
            column + uuid.len() as u16,
        ),
        (
            MouseEventKind::Up(MouseButton::Left),
            column + uuid.len() as u16,
        ),
    ] {
        assert!(app.handle_rendered_selection_event(
            &mut tui,
            &TuiEvent::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }),
        )?);
        app.render_owned_transcript(&mut tui, size)?;
    }
    {
        let mut selection = app.chat_widget.rendered_selection.borrow_mut();
        let crate::rendered_selection::RenderedSelection { view, cells, .. } = &mut *selection;
        assert_eq!(view.selected_text(cells).as_deref(), Some(uuid));
    }
    let buffer = crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal);
    let selected_cells = column..column + uuid.len() as u16;
    let text = selected_cells
        .clone()
        .map(|x| buffer[(x, row)].symbol())
        .collect::<String>();
    let highlight = selected_cells
        .map(|x| {
            if buffer[(x, row)].modifier.contains(Modifier::REVERSED) {
                '^'
            } else {
                '·'
            }
        })
        .collect::<String>();
    insta::assert_snapshot!(format!("{text}\n{highlight}"), @r"
    01a1b2c9-a8f2-7951-adbf-33128826d45c
    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
    ");
    // Copy owns Ctrl+C before the popup or active-turn interrupt handlers.
    assert!(app.handle_rendered_selection_event(
        &mut tui,
        &TuiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    )?);
    assert!(tui.clipboard.is_busy());
    assert_eq!(app.chat_widget.capture_thread_input_state(), draft);
    assert!(!app.chat_widget.no_modal_or_popup_active());
    // Escape clears the selected text, leaving the completion available for Tab.
    assert!(app.handle_rendered_selection_event(&mut tui, &TuiEvent::Key(KeyCode::Esc.into()),)?);
    assert!(
        !app.chat_widget
            .rendered_selection
            .borrow()
            .view
            .has_active_interaction()
    );
    assert!(!app.chat_widget.no_modal_or_popup_active());
    let tab = KeyEvent::from(KeyCode::Tab);
    assert!(!app.handle_rendered_selection_event(&mut tui, &TuiEvent::Key(tab))?);
    app.chat_widget.handle_key_event(tab);
    assert_eq!(app.chat_widget.composer_text_with_pending(), "/agent 714 ");
    tui.set_owned_screen(/*owned*/ false)?;
    Ok(())
}

#[tokio::test]
async fn modal_input_clears_passive_selection_ownership() -> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(/*owned*/ true)?;
    app.chat_widget.apply_external_edit("/model".into());
    assert!(app.chat_widget.can_select_rendered_text());
    let size = tui.terminal.size()?;
    app.render_owned_transcript(&mut tui, size)?;
    let area = app
        .chat_widget
        .rendered_selection
        .borrow()
        .areas
        .first()
        .copied()
        .ok_or_else(|| color_eyre::eyre::eyre!("missing selectable completion rows"))?;
    assert!(app.handle_rendered_selection_event(
        &mut tui,
        &TuiEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 2,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }),
    )?);
    assert!(
        app.chat_widget
            .rendered_selection
            .borrow()
            .view
            .has_active_interaction()
    );
    app.chat_widget.open_config_panel();
    assert!(app.chat_widget.has_active_modal());
    assert!(!app.chat_widget.can_select_rendered_text());
    assert!(!app.handle_rendered_selection_event(&mut tui, &TuiEvent::Key(KeyCode::Esc.into()),)?);
    assert!(
        !app.chat_widget
            .rendered_selection
            .borrow()
            .view
            .has_active_interaction()
    );
    tui.set_owned_screen(/*owned*/ false)?;
    Ok(())
}
