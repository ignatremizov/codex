//! Both presentation owners share review keys and painted-content prompt confirmation.

use super::*;
use crate::history_cell::HistoryRenderMode;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::history_cell::UserMessageIdentity;
use crate::transcript_view::ReviewMode;
use pretty_assertions::assert_eq;

fn paint(app: &mut App, owned: bool) -> String {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 10,
    );
    let mut buffer = Buffer::empty(area);
    if owned {
        app.transcript_view
            .render(area, &mut buffer, &app.transcript_cells);
    } else if let Some(Overlay::Transcript(overlay)) = &mut app.overlay {
        overlay.render(area, &mut buffer);
    }
    buffer_text(&buffer)
}

#[tokio::test]
async fn backtrack_browser_and_pager_movement_require_repaint_before_confirmation() -> Result<()> {
    for owned in [false, true] {
        let (mut app, mut events, _operations) = make_test_app_with_channels().await;
        let thread_id = ThreadId::new();
        attach_thread(&mut app, thread_id);
        app.transcript_cells = vec![
            user_cell("first prompt"),
            Arc::new(PlainHistoryCell::new(
                (0..80).map(|i| format!("tool row {i}").into()).collect(),
            )),
            user_cell("selected prompt"),
        ];
        let selected = Arc::clone(&app.transcript_cells[2]);
        let identity = UserMessageIdentity {
            turn_id: "canonical-turn".into(),
            item_id: "canonical-item".into(),
        };
        selected
            .as_any()
            .downcast_ref::<UserHistoryCell>()
            .expect("user")
            .identity
            .set(identity.clone())
            .expect("unbound identity");
        let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
        let mut tui = crate::tui::test_support::make_test_tui()?;
        tui.set_owned_screen(owned)?;
        app.open_transcript_overlay(&mut tui);
        // These are the same owner entry points used by ordinary backtrack input.
        for code in [KeyCode::Esc, KeyCode::Enter] {
            let event = TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
            if owned {
                assert!(app.handle_owned_backtrack_event(&mut tui, &event)?);
            } else {
                app.handle_backtrack_overlay_event(&mut tui, &mut server, event)
                    .await?;
            }
        }
        assert!(app.backtrack.overlay_preview_active);
        assert!(
            !std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, AppEvent::RevertSessionForPromptEdit { .. }))
        );
        app.chat_widget
            .apply_external_edit("preserved draft".into());
        paint(&mut app, owned);

        for code in [
            KeyCode::Char('v'),
            KeyCode::Char(']'),
            KeyCode::Char('['),
            KeyCode::Home,
            KeyCode::Enter,
        ] {
            let event = TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
            if owned {
                assert!(app.handle_owned_backtrack_event(&mut tui, &event)?);
            } else {
                app.handle_backtrack_overlay_event(&mut tui, &mut server, event)
                    .await?;
            }
            if code != KeyCode::Enter {
                paint(&mut app, owned);
            }
        }
        assert!(app.backtrack.overlay_preview_active);
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "preserved draft"
        );
        assert!(
            !std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, AppEvent::RevertSessionForPromptEdit { .. }))
        );
        let end = TuiEvent::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        if owned {
            assert!(app.handle_owned_backtrack_event(&mut tui, &end)?);
        } else {
            app.handle_backtrack_overlay_event(&mut tui, &mut server, end)
                .await?;
        }
        assert!(paint(&mut app, owned).contains("selected prompt"));
        let enter = TuiEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        if owned {
            assert!(app.handle_owned_backtrack_event(&mut tui, &enter)?);
        } else {
            app.handle_backtrack_overlay_event(&mut tui, &mut server, enter)
                .await?;
        }
        let edits = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::RevertSessionForPromptEdit {
                    thread_id,
                    selected_cell,
                    prompt,
                } => Some((thread_id, selected_cell, prompt)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].0, thread_id);
        assert!(Arc::ptr_eq(&edits[0].1, &selected));
        assert_eq!(edits[0].2.text, "selected prompt");
        assert_eq!(
            selected
                .as_any()
                .downcast_ref::<UserHistoryCell>()
                .expect("user")
                .identity
                .get(),
            Some(&identity)
        );
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "preserved draft"
        );
        server.shutdown().await?;
        tui.set_owned_screen(/*owned*/ false)?;
    }
    Ok(())
}

#[tokio::test]
async fn review_keys_belong_to_open_browser_and_do_not_override_search_or_draft() -> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    attach_thread(&mut app, ThreadId::new());
    app.transcript_cells = vec![user_cell("question")];
    let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(/*owned*/ true)?;
    assert!(
        !app.transcript_view
            .owns_review_key(KeyCode::Char('v').into())
    );
    app.chat_widget.apply_external_edit("draft".into());
    app.open_transcript_overlay(&mut tui);
    assert_eq!(app.transcript_view.review_mode(), Some(ReviewMode::Review));
    // Initial browser input must wait for its first real layout, not use the old conversation.
    app.handle_owned_transcript_event(
        &mut tui,
        &mut server,
        &TuiEvent::Key(KeyCode::Char('v').into()),
    )
    .await?;
    assert_eq!(app.transcript_view.review_mode(), Some(ReviewMode::Review));
    paint(&mut app, /*owned*/ true);
    app.handle_owned_transcript_event(
        &mut tui,
        &mut server,
        &TuiEvent::Key(KeyCode::Char('v').into()),
    )
    .await?;
    assert_eq!(app.transcript_view.review_mode(), Some(ReviewMode::Full));
    app.transcript_view.begin_search();
    app.handle_owned_transcript_event(
        &mut tui,
        &mut server,
        &TuiEvent::Key(KeyCode::Char('v').into()),
    )
    .await?;
    assert_eq!(app.transcript_view.review_mode(), Some(ReviewMode::Full));
    assert_eq!(app.chat_widget.composer_text_with_pending(), "draft");
    app.transcript_view.cancel_search();
    app.close_transcript_overlay(&mut tui);
    assert_eq!(app.transcript_view.review_mode(), None);
    app.transcript_view
        .set_presentation(/*detailed*/ true, HistoryRenderMode::Rich);
    assert!(
        !app.transcript_view
            .owns_review_key(KeyCode::Char(']').into())
    );
    server.shutdown().await?;
    tui.set_owned_screen(/*owned*/ false)?;
    Ok(())
}

#[tokio::test]
async fn armed_prompt_review_keys_precede_remapped_find_in_both_viewports() -> Result<()> {
    for owned in [false, true] {
        for find in ['v', ']'] {
            let mut app = crate::app::test_support::make_test_app().await;
            attach_thread(&mut app, ThreadId::new());
            app.transcript_cells = vec![user_cell("first"), user_cell("second")];
            app.keymap.pager.find = vec![crate::key_hint::plain(KeyCode::Char(find))];
            let mut server =
                Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
            let mut tui = crate::tui::test_support::make_test_tui()?;
            tui.set_owned_screen(owned)?;
            app.open_transcript_overlay(&mut tui);
            let escape = TuiEvent::Key(KeyCode::Esc.into());
            if owned {
                assert!(app.handle_owned_backtrack_event(&mut tui, &escape)?);
            } else {
                app.handle_backtrack_overlay_event(&mut tui, &mut server, escape)
                    .await?;
            }
            paint(&mut app, owned);
            let selected = app.backtrack.nth_user_message;
            let event = TuiEvent::Key(KeyCode::Char(find).into());
            if owned {
                assert!(app.handle_owned_backtrack_event(&mut tui, &event)?);
                assert!(!app.transcript_view.is_search_active());
            } else {
                app.handle_backtrack_overlay_event(&mut tui, &mut server, event)
                    .await?;
                assert!(
                    matches!(&app.overlay, Some(Overlay::Transcript(overlay)) if !overlay.is_search_active())
                );
            }
            assert_eq!(
                (
                    app.backtrack.overlay_preview_active,
                    app.backtrack.nth_user_message
                ),
                (true, selected)
            );
            server.shutdown().await?;
            tui.set_owned_screen(/*owned*/ false)?;
        }
    }
    Ok(())
}
