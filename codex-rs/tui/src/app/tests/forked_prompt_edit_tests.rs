//! Canonical fork prompts remain editable across a late-flushed session header.

use super::*;
use crate::app_backtrack::truncate_before_prompt;
use crate::app_backtrack::user_positions_iter;
use crate::history_cell::SessionInfoCell;
use crate::history_cell::UserMessageIdentity;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn editable_projection_deduplicates_identity_not_text_across_headers() {
    let app = make_test_app().await;
    let session = test_thread_session(ThreadId::new(), test_path_buf("/tmp/project"));
    let header: Arc<dyn HistoryCell> = Arc::new(new_session_info(
        app.chat_widget.config_ref(),
        &app.local_settings,
        app.chat_widget.current_model(),
        &session.model,
        &session,
        /*is_first_event*/ false,
        /*tooltip_override*/ None,
        /*auth_plan*/ None,
        /*show_fast_status*/ false,
    ));
    let prompt = |text: &str, identity: Option<(&str, &str)>| -> Arc<dyn HistoryCell> {
        let cell =
            crate::history_cell::new_user_prompt(text.into(), Vec::new(), Vec::new(), Vec::new());
        if let Some((turn, item)) = identity {
            cell.identity
                .set(UserMessageIdentity {
                    turn_id: turn.into(),
                    item_id: item.into(),
                })
                .expect("fresh cell");
        }
        Arc::new(cell)
    };
    let cells = vec![
        prompt("stale optimistic", /*identity*/ None),
        prompt("same", Some(("turn-1", "item-1"))),
        prompt("same", Some(("turn-2", "item-1"))),
        Arc::clone(&header),
        prompt("same", Some(("turn-1", "item-1"))),
        prompt("same", Some(("turn-1", "item-2"))),
        prompt("", Some(("hidden", "item"))),
        header,
        prompt("current optimistic", /*identity*/ None),
    ];
    assert_eq!(
        user_positions_iter(&cells).collect::<Vec<_>>(),
        vec![2, 4, 5, 8]
    );
    assert_eq!(user_count(&cells), 4);
    assert_eq!(
        (0..5)
            .map(|nth| nth_user_position(&cells, nth))
            .collect::<Vec<_>>(),
        vec![Some(2), Some(4), Some(5), Some(8), None]
    );
    let rendered = user_positions_iter(&cells)
        .map(|index| {
            lines_to_single_string(&cells[index].display_lines(/*width*/ 80))
                .trim()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @"
    › same
    › same
    › same
    › current optimistic
    ");
    let latest_header = Arc::clone(&cells[7]);
    let mut retained = cells;
    assert!(truncate_before_prompt(&mut retained, /*nth*/ 1));
    assert!(Arc::ptr_eq(
        retained.last().expect("retained header"),
        &latest_header
    ));
    assert_eq!(
        user_positions_iter(&retained)
            .map(|index| retained[index]
                .as_any()
                .downcast_ref::<UserHistoryCell>()
                .expect("user")
                .identity
                .get()
                .cloned())
            .collect::<Vec<_>>(),
        vec![Some(UserMessageIdentity {
            turn_id: "turn-2".into(),
            item_id: "item-1".into(),
        })],
    );
}

#[tokio::test]
async fn inherited_fork_prompts_use_the_current_mutation_and_keep_the_latest_header() -> Result<()>
{
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        for selected in [0, 2] {
            let (mut app, mut events, (mut server, requests, proxy), source_id, source_path) =
                prompt_source(mode, HistoryCapabilities::Current, PromptImages::Remote).await?;
            let mut tui = crate::tui::test_support::make_test_tui()?;
            drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
            Box::pin(app.handle_event(
                &mut tui,
                &mut server,
                AppEvent::ForkCurrentSession { name: None },
            ))
            .await?;
            drain_fork_attachment(&mut app, &mut tui, &mut server, &mut events).await?;
            app.config
                .features
                .disable(Feature::ForkPromptEdits)
                .expect("test config should allow feature update");
            let fork_id = app.chat_widget.thread_id().expect("full-history fork");
            assert_ne!(fork_id, source_id);
            let before = server.thread_read(fork_id, /*include_turns*/ true).await?;
            assert_eq!((before.history_mode, before.turns.len()), (mode, 4));
            let source_bytes = std::fs::read(&source_path)?;
            let source = server
                .thread_read(source_id, /*include_turns*/ true)
                .await?;

            let header = app
                .transcript_cells
                .iter()
                .rev()
                .find(|cell| cell.as_any().is::<SessionInfoCell>())
                .cloned()
                .expect("fork session header");
            // Exercise the late-header layout even if a particular terminal flushes it earlier.
            // These are real hydrated fork cells, including their canonical turn/item receipts.
            app.transcript_cells
                .retain(|cell| !cell.as_any().is::<SessionInfoCell>());
            app.transcript_cells.push(Arc::clone(&header));
            if selected == 2 {
                // A replayed prefix must not change the selection's ordinal at dispatch.
                let replayed_suffix = Arc::clone(
                    &app.transcript_cells[nth_user_position(&app.transcript_cells, /*nth*/ 3)
                        .expect("last inherited prompt")],
                );
                app.transcript_cells.insert(0, replayed_suffix);
            }
            assert_eq!(user_count(&app.transcript_cells), 4);
            let selected_cell = Arc::clone(
                &app.transcript_cells
                    [nth_user_position(&app.transcript_cells, selected).expect("inherited prompt")],
            );
            assert_eq!(
                selected_cell
                    .as_any()
                    .downcast_ref::<UserHistoryCell>()
                    .expect("prompt")
                    .identity
                    .get(),
                Some(&UserMessageIdentity {
                    turn_id: format!("turn-{selected}"),
                    item_id: format!("user-{selected}"),
                }),
            );
            app.backtrack.base_id = Some(fork_id);
            app.backtrack.nth_user_message = selected;
            let selection = app
                .confirm_backtrack_from_main()
                .expect("editable inherited prompt");
            app.apply_backtrack_selection(selection);
            let edit = std::iter::from_fn(|| events.try_recv().ok())
                .find(|event| matches!(event, AppEvent::RevertSessionForPromptEdit { .. }))
                .expect("prompt edit request");
            assert!(
                matches!(&edit, AppEvent::RevertSessionForPromptEdit { selected_cell: actual, .. }
                if Arc::ptr_eq(actual, &selected_cell))
            );
            requests.lock().expect("recorded requests").clear();
            Box::pin(app.handle_event(&mut tui, &mut server, edit)).await?;
            drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;

            let rollback = recorded_params(&requests, "thread/rollback");
            let revert = recorded_params(&requests, "thread/revert");
            match mode {
                ThreadHistoryMode::Legacy => {
                    assert!(revert.is_empty());
                    assert_eq!(
                        rollback,
                        vec![serde_json::json!({
                            "threadId": fork_id,
                            "numTurns": 4 - selected,
                            "expectedStartTurnId": format!("turn-{selected}"),
                            "expectedTurnCount": 4,
                        })]
                    );
                }
                ThreadHistoryMode::Paginated => {
                    assert!(rollback.is_empty());
                    assert_eq!(
                        revert,
                        vec![serde_json::json!({
                            "threadId": fork_id,
                            "beforeTurnId": format!("turn-{selected}"),
                        })]
                    );
                    assert!(!recorded_params(&requests, "thread/turns/list").is_empty());
                    let pages = recorded_params(&requests, "thread/items/list");
                    assert!(!pages.is_empty());
                    assert!(
                        pages.iter().all(|params| params["limit"]
                            .as_u64()
                            .is_some_and(|limit| limit <= 100))
                    );
                }
            }
            assert!(recorded_params(&requests, "turn/start").is_empty());
            assert!(recorded_params(&requests, "thread/fork").is_empty());
            assert_eq!(app.chat_widget.thread_id(), Some(fork_id));
            assert_eq!(app.pending_thread_switch_resets, 0);
            assert!(!app.thread_unavailable(fork_id));
            assert_eq!(user_count(&app.transcript_cells), selected);
            assert_eq!(
                app.transcript_cells
                    .iter()
                    .filter(|cell| cell.as_any().is::<SessionInfoCell>())
                    .count(),
                1,
            );
            assert!(
                app.transcript_cells
                    .iter()
                    .any(|cell| Arc::ptr_eq(cell, &header))
            );
            assert_eq!(
                app.chat_widget.composer_text_with_pending(),
                if selected == 0 {
                    "older prompt"
                } else {
                    SELECTED
                }
            );
            let after = server.thread_read(fork_id, /*include_turns*/ true).await?;
            assert_eq!(after.turns, before.turns[..selected]);
            let stored = app.thread_event_channels[&fork_id]
                .store
                .lock()
                .await
                .snapshot();
            assert_eq!(stored.turns, after.turns);
            assert_eq!(
                (
                    app.chat_widget.rollout_path(),
                    app.primary_session_configured
                        .as_ref()
                        .and_then(|session| session.rollout_path.clone()),
                    stored.session.and_then(|session| session.rollout_path),
                ),
                (after.path.clone(), after.path.clone(), after.path),
            );
            assert_eq!(
                server
                    .thread_read(source_id, /*include_turns*/ true)
                    .await?
                    .turns,
                source.turns
            );
            assert_eq!(std::fs::read(&source_path)?, source_bytes);
            server.shutdown().await?;
            proxy.await??;
        }
    }
    Ok(())
}
