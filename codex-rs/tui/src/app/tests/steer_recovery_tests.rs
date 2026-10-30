//! A committed edit must not strand the terminal behind a failed history refresh.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn failed_steer_refresh_releases_input_but_quarantines_stale_history() -> Result<()> {
    for capability in [
        HistoryCapabilities::RevertRefreshFails,
        HistoryCapabilities::RevertRefreshDisconnects,
        HistoryCapabilities::RevertRefreshStalls,
    ] {
        let (mut app, mut events, (mut server, requests, proxy), source_id, _path) =
            prompt_source_with_history(
                ThreadHistoryMode::Paginated,
                capability,
                PromptImages::TextOnly,
                PromptHistory::MidTurnSteer,
            )
            .await?;
        app.config.features.disable(Feature::ForkPromptEdits)?;
        let mut tui = crate::tui::test_support::make_test_tui()?;
        drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
        if capability == HistoryCapabilities::RevertRefreshDisconnects {
            // Only the reconnect state transition runs here; no replacement connection starts.
            app.app_server_target = AppServerTarget::LocalDaemon {
                endpoint: crate::resolve_remote_addr("ws://127.0.0.1:1")?,
                allow_embedded_fallback: false,
            };
        }
        let original = app.transcript_cells.clone();
        let selected_cell =
            Arc::clone(&original[nth_user_position(&original, /*nth*/ 3).expect("selected steer")]);
        let result = {
            let mut edit = Box::pin(app.handle_event(
                &mut tui,
                &mut server,
                AppEvent::RevertSessionForPromptEdit {
                    thread_id: source_id,
                    selected_cell: Arc::clone(&selected_cell),
                    prompt: crate::chatwidget::UserMessage::from(SELECTED),
                },
            ));
            if capability == HistoryCapabilities::RevertRefreshStalls {
                let refresh_requested =
                    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
                        loop {
                            let refreshing = {
                                let recorded = requests.lock().expect("recorded requests");
                                recorded
                                    .iter()
                                    .skip_while(|request| request.method != "thread/revert")
                                    .any(|request| {
                                        matches!(
                                            request.method.as_str(),
                                            "thread/turns/list" | "thread/items/list"
                                        )
                                    })
                            };
                            if refreshing {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
                        }
                    });
                tokio::select! {
                    _ = &mut edit => panic!("edit completed before post-revert hydration"),
                    ready = refresh_requested => ready?,
                }
                // Advance only after the actual post-commit request, then poll the edit's
                // registered timeout before restoring the clock for subsequent I/O.
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(/*secs*/ 11)).await;
                let result = edit.await;
                tokio::time::resume();
                result
            } else {
                edit.await
            }
        };
        assert!(matches!(result, Ok(AppRunControl::Continue)));
        assert_eq!(app.pending_thread_switch_resets, 0);
        assert!(app.thread_unavailable(source_id));
        assert_eq!(app.chat_widget.composer_text_with_pending(), SELECTED);
        assert_eq!(
            app.reconnect.offline,
            capability == HistoryCapabilities::RevertRefreshDisconnects,
        );
        assert_eq!(app.transcript_cells.len(), original.len());
        assert!(
            original
                .iter()
                .zip(&app.transcript_cells)
                .all(|(before, after)| Arc::ptr_eq(before, after))
        );
        let mut errors = Vec::new();
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event, AppEvent::FinishPromptRevert { .. }));
            if let AppEvent::InsertHistoryCell(cell) = event {
                errors.push(lines_to_single_string(&cell.display_lines(/*width*/ 500)));
            }
        }
        let error = errors
            .iter()
            .find(|error| error.contains("Could not finish editing this prompt:"))
            .expect("recovery guidance");
        if capability == HistoryCapabilities::RevertRefreshFails {
            insta::assert_snapshot!(error.trim(), @"
            ■ Could not finish editing this prompt: failed to load a bounded thread history page: thread/turns/list failed: canonical refresh unavailable (code -32603). Your draft is preserved. Start a new conversation, or quit and reopen this conversation in a new Codex process.
            ");
        }
        if capability == HistoryCapabilities::RevertRefreshStalls {
            assert!(error.contains("history changed but refresh timed out"));
        }
        // Events already queued before the refresh failed must not replace the draft or issue
        // another mutation, even though ordinary terminal input is now enabled.
        let input = app.chat_widget.capture_thread_input_state();
        let requests_before = requests.lock().expect("recorded requests").len();
        for event in [
            AppEvent::RevertSessionForPromptEdit {
                thread_id: source_id,
                selected_cell,
                prompt: crate::chatwidget::UserMessage::from("stale queued edit"),
            },
            AppEvent::SubmitThreadOp {
                thread_id: source_id,
                op: AppCommand::Compact,
            },
        ] {
            assert!(matches!(
                Box::pin(app.handle_event(&mut tui, &mut server, event)).await?,
                AppRunControl::Continue
            ));
        }
        assert_eq!(app.chat_widget.capture_thread_input_state(), input);
        assert_eq!(
            requests.lock().expect("recorded requests").len(),
            requests_before,
        );
        assert_eq!(recorded_params(&requests, "thread/revert").len(), 1);
        assert!(recorded_params(&requests, "turn/start").is_empty());
        if capability != HistoryCapabilities::RevertRefreshDisconnects {
            let canonical = server
                .thread_read(source_id, /*include_turns*/ true)
                .await?;
            assert_eq!(
                canonical
                    .turns
                    .iter()
                    .flat_map(|turn| &turn.items)
                    .filter(|item| matches!(item, ThreadItem::UserMessage { .. }))
                    .count(),
                3,
            );
            server.shutdown().await?;
        }
        proxy.await??;
    }
    Ok(())
}
