//! Exercise Esc/Enter steer edits through the public server transport in both history modes.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn steer_edits_keep_the_earlier_turn_prefix_and_restore_a_sendable_draft() -> Result<()> {
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        for owned in [false, true] {
            for fork in [false, true] {
                let (mut app, mut events, (mut server, requests, proxy), source_id, path) =
                    prompt_source_with_history(
                        mode,
                        HistoryCapabilities::Current,
                        PromptImages::TextOnly,
                        PromptHistory::MidTurnSteer,
                    )
                    .await?;
                if !fork {
                    app.config.features.disable(Feature::ForkPromptEdits)?;
                }
                let mut tui = crate::tui::test_support::make_test_tui()?;
                tui.set_owned_screen(owned)?;
                drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
                let original = server
                    .thread_read(source_id, /*include_turns*/ true)
                    .await?;
                let original_bytes = std::fs::read(&path)?;
                let selected_cell = Arc::clone(
                    &app.transcript_cells[nth_user_position(&app.transcript_cells, /*nth*/ 3)
                        .expect("steer in the transcript")],
                );
                let identity = selected_cell
                    .as_any()
                    .downcast_ref::<UserHistoryCell>()
                    .and_then(|cell| cell.identity.get())
                    .expect("canonical steer")
                    .clone();
                app.open_transcript_overlay(&mut tui);
                app.handle_tui_event(&mut tui, &mut server, TuiEvent::Draw)
                    .await?;
                for code in [KeyCode::Esc, KeyCode::Enter] {
                    if code == KeyCode::Enter {
                        app.handle_tui_event(&mut tui, &mut server, TuiEvent::Draw)
                            .await?;
                    }
                    let event = TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
                    if owned {
                        assert!(app.handle_owned_backtrack_event(&mut tui, &event)?);
                    } else {
                        app.handle_backtrack_overlay_event(&mut tui, &mut server, event)
                            .await?;
                    }
                }
                let edit = std::iter::from_fn(|| events.try_recv().ok())
                    .find(|event| matches!(event, AppEvent::RevertSessionForPromptEdit { .. }))
                    .expect("confirmed steer selection");
                let AppEvent::RevertSessionForPromptEdit {
                    selected_cell: actual,
                    ..
                } = &edit
                else {
                    unreachable!()
                };
                assert!(Arc::ptr_eq(actual, &selected_cell));
                Box::pin(app.handle_event(&mut tui, &mut server, edit)).await?;
                if fork {
                    drain_fork_attachment(&mut app, &mut tui, &mut server, &mut events).await?;
                } else {
                    drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
                }
                let edited_id = app.chat_widget.thread_id().expect("edited thread");
                assert_eq!(edited_id == source_id, !fork);
                assert!(!app.thread_unavailable(edited_id));
                assert_eq!(app.pending_thread_switch_resets, 0);
                assert_eq!(app.chat_widget.composer_text_with_pending(), SELECTED);
                let edited = server
                    .thread_read(edited_id, /*include_turns*/ true)
                    .await?;
                let mut expected_items: Vec<_> = original
                    .turns
                    .iter()
                    .flat_map(|turn| turn.items.clone())
                    .collect();
                let selected_index = expected_items
                    .iter()
                    .position(|item| item.id() == identity.item_id)
                    .expect("selected source item");
                expected_items.truncate(selected_index);
                expected_items.retain(|item| !matches!(
                    item, ThreadItem::AgentMessage { text, .. } if text == "Output after steer admission"
                ));
                assert_eq!(
                    edited
                        .turns
                        .iter()
                        .flat_map(|turn| turn.items.clone())
                        .collect::<Vec<_>>(),
                    expected_items,
                );
                assert!(
                    edited.turns.iter().all(
                        |turn| turn.status != codex_app_server_protocol::TurnStatus::InProgress
                    )
                );
                let method = if fork {
                    "thread/fork"
                } else if mode == ThreadHistoryMode::Legacy {
                    "thread/rollback"
                } else {
                    "thread/revert"
                };
                let mutations = recorded_params(&requests, method);
                assert_eq!(mutations.len(), 1);
                let field = if method == "thread/rollback" {
                    "expectedStartItemId"
                } else {
                    "beforeItemId"
                };
                assert_eq!(mutations[0][field], identity.item_id);
                assert!(recorded_params(&requests, "turn/start").is_empty());
                if fork {
                    assert_eq!(std::fs::read(&path)?, original_bytes);
                    assert_eq!(
                        server
                            .thread_read(source_id, /*include_turns*/ true)
                            .await?
                            .turns,
                        original.turns,
                    );
                }
                let rendered = app
                    .transcript_cells
                    .iter()
                    .filter(|cell| cell.as_any().is::<UserHistoryCell>())
                    .flat_map(|cell| cell.display_lines(/*width*/ 80))
                    .collect::<Vec<_>>();
                assert!(!app.transcript_cells.iter().any(|cell| {
                    lines_to_single_string(&cell.display_lines(/*width*/ 80))
                        .contains("Output after steer admission")
                }));
                let rendered = format!(
                    "{}\nDraft: {}",
                    lines_to_single_string(&rendered).trim(),
                    app.chat_widget.composer_text_with_pending(),
                );
                insta::allow_duplicates! {
                    insta::assert_snapshot!(rendered, @r"
                    › older prompt


                    › retained prompt


                    › use $skill @sample $google-calendar
                    Draft: use $skill @sample $google-calendar
                    ");
                }
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                let mut other_events = Vec::new();
                let submitted = std::iter::from_fn(|| events.try_recv().ok())
                    .find_map(|event| match event {
                        AppEvent::CodexOp(command @ AppCommand::UserTurn { .. }) => Some(command),
                        other => {
                            other_events.push(format!("{other:?}"));
                            None
                        }
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "explicit Enter did not submit the restored steer: mode={mode:?}, owned={owned}, fork={fork}, \
                             external_writer={}, active_view={}, model={:?}, draft={:?}, events={other_events:?}, pane={}",
                            app.chat_widget.is_external_writer_view(),
                            app.chat_widget.has_active_view(),
                            app.chat_widget.current_model(),
                            app.chat_widget.composer_text_with_pending(),
                            render_bottom_popup(&app.chat_widget, /*width*/ 80),
                        )
                    });
                let AppCommand::UserTurn { items, .. } = submitted else {
                    unreachable!()
                };
                assert!(
                    matches!(items.first(), Some(codex_app_server_protocol::UserInput::Text { text, .. }) if text == SELECTED)
                );
                server.shutdown().await?;
                proxy.await??;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn older_servers_preserve_the_steer_draft_without_issuing_a_turn_cut() -> Result<()> {
    for (mode, capabilities) in [
        (
            ThreadHistoryMode::Legacy,
            HistoryCapabilities::PromptItemBoundariesUnavailable,
        ),
        (
            ThreadHistoryMode::Paginated,
            HistoryCapabilities::PromptItemBoundariesUnavailable,
        ),
        (
            ThreadHistoryMode::Legacy,
            HistoryCapabilities::PromptItemBoundariesOmitted,
        ),
        (
            ThreadHistoryMode::Paginated,
            HistoryCapabilities::PromptItemBoundariesOmitted,
        ),
    ] {
        for fork in [false, true] {
            let (mut app, mut events, (mut server, requests, proxy), source_id, path) =
                prompt_source_with_history(
                    mode,
                    capabilities,
                    PromptImages::TextOnly,
                    PromptHistory::MidTurnSteer,
                )
                .await?;
            if !fork {
                app.config.features.disable(Feature::ForkPromptEdits)?;
            }
            let mut tui = crate::tui::test_support::make_test_tui()?;
            drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
            let source_bytes = std::fs::read(&path)?;
            let selected_cell = Arc::clone(
                &app.transcript_cells
                    [nth_user_position(&app.transcript_cells, /*nth*/ 3).expect("selected steer")],
            );
            Box::pin(app.handle_event(
                &mut tui,
                &mut server,
                AppEvent::RevertSessionForPromptEdit {
                    thread_id: source_id,
                    selected_cell,
                    prompt: crate::chatwidget::UserMessage::from(SELECTED),
                },
            ))
            .await?;
            assert_eq!(app.chat_widget.composer_text_with_pending(), SELECTED);
            assert_eq!(std::fs::read(&path)?, source_bytes);
            for method in [
                "thread/fork",
                "thread/rollback",
                "thread/revert",
                "turn/start",
            ] {
                assert!(recorded_params(&requests, method).is_empty());
            }
            server.shutdown().await?;
            proxy.await??;
        }
    }
    Ok(())
}
