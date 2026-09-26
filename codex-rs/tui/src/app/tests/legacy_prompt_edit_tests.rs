//! Exercise Legacy edits through the same public RPC transport as prompt forks.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn legacy_rejection_and_ambiguous_outcomes_preserve_draft_without_retry() -> Result<()> {
    for (capability, blocked) in [
        (HistoryCapabilities::RollbackRejected, false),
        (HistoryCapabilities::RollbackUnknown, true),
        (HistoryCapabilities::RollbackCommittedRefreshFails, true),
    ] {
        let (mut app, mut events, (mut server, requests, proxy), source_id, _path) =
            prompt_source(ThreadHistoryMode::Legacy, capability, PromptImages::Remote).await?;
        app.config.features.disable(Feature::ForkPromptEdits);
        let mut tui = crate::tui::test_support::make_test_tui()?;
        drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
        // Queue a follow-up through ordinary widget input, without issuing any server mutation.
        let mut running = server
            .thread_read(source_id, /*include_turns*/ true)
            .await?
            .turns
            .last()
            .expect("source turn")
            .clone();
        running.status = codex_app_server_protocol::TurnStatus::InProgress;
        app.chat_widget.handle_server_notification(
            ServerNotification::TurnStarted(codex_app_server_protocol::TurnStartedNotification {
                thread_id: source_id.to_string(),
                turn: running,
                agent_queue: None,
            }),
            /*replay_kind*/ None,
        );
        app.chat_widget
            .restore_user_message_to_composer(crate::chatwidget::UserMessage::from(
                "queued follow-up",
            ));
        app.chat_widget
            .handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(app.chat_widget.composer_text_with_pending().is_empty());
        drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
        let original = app.transcript_cells.clone();
        let selected_cell = Arc::clone(
            &original[nth_user_position(&original, /*nth*/ 2).expect("selected prompt")],
        );
        let result = Box::pin(app.handle_event(
            &mut tui,
            &mut server,
            AppEvent::RevertSessionForPromptEdit {
                thread_id: source_id,
                selected_cell,
                prompt: crate::chatwidget::UserMessage::from(SELECTED),
            },
        ))
        .await;
        assert!(matches!(result, Ok(AppRunControl::Continue)));
        assert_eq!(app.pending_thread_switch_resets, 0);
        assert_eq!(app.thread_unavailable(source_id), blocked);
        assert_eq!(app.chat_widget.composer_text_with_pending(), SELECTED);
        assert_eq!(recorded_params(&requests, "thread/rollback").len(), 1);
        assert!(recorded_params(&requests, "turn/start").is_empty());
        assert!(
            original
                .iter()
                .zip(&app.transcript_cells)
                .all(|(before, after)| Arc::ptr_eq(before, after))
        );
        assert!(
            !std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, AppEvent::FinishPromptRevert { .. }))
        );
        if blocked {
            // These events were already queued when the first mutation failed. Admission must
            // precede any composer side effects, even when the server itself remains ready.
            let input = app.chat_widget.capture_thread_input_state();
            let requests_before = requests.lock().expect("recorded requests").len();
            for event in [
                AppEvent::RevertSessionForPromptEdit {
                    thread_id: source_id,
                    selected_cell: Arc::clone(
                        &original
                            [nth_user_position(&original, /*nth*/ 2).expect("selected prompt")],
                    ),
                    prompt: crate::chatwidget::UserMessage::from("must not replace the draft"),
                },
                AppEvent::CodexOp(AppCommand::Compact),
                AppEvent::CodexOp(AppCommand::UserTurn {
                    client_user_message_id: "queued-before-rollback".into(),
                    items: Vec::new(),
                    cwd: app.config.cwd.to_path_buf(),
                    approval_policy: codex_app_server_protocol::AskForApproval::Never,
                    approvals_reviewer: None,
                    active_permission_profile: None,
                    model: app.chat_widget.current_model().to_string(),
                    effort: None,
                    summary: None,
                    service_tier: None,
                    final_output_json_schema: None,
                    collaboration_mode: None,
                    personality: None,
                }),
                AppEvent::SubmitThreadOp {
                    thread_id: source_id,
                    op: AppCommand::Compact,
                },
                AppEvent::ForkCurrentSession { name: None },
                AppEvent::SetThreadGoalStatus {
                    thread_id: source_id,
                    status: codex_app_server_protocol::ThreadGoalStatus::Active,
                },
                AppEvent::ClearThreadGoal {
                    thread_id: source_id,
                },
            ] {
                app.app_event_tx.send(event);
            }
            let mut errors = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let AppEvent::InsertHistoryCell(cell) = event {
                    errors.push(lines_to_single_string(&cell.display_lines(/*width*/ 200)));
                } else {
                    assert!(matches!(
                        Box::pin(app.handle_event(&mut tui, &mut server, event)).await?,
                        AppRunControl::Continue
                    ));
                }
            }
            insta::allow_duplicates! {
                insta::assert_snapshot!(errors.first().expect("blocked mutation error").trim(), @"
                ■ This conversation needs its history reloaded. Your draft is preserved. Start a new conversation, or quit and reopen this conversation in a new Codex process.
                ");
            }
            assert_eq!(app.chat_widget.capture_thread_input_state(), input);
            assert_eq!(
                requests.lock().expect("recorded requests").len(),
                requests_before
            );
            assert_eq!(recorded_params(&requests, "thread/rollback").len(), 1);
            assert!(recorded_params(&requests, "turn/start").is_empty());

            // The compact transport adapter is also called directly by existing owners.
            assert!(
                app.try_submit_active_thread_op_via_app_server(
                    &mut server,
                    source_id,
                    &AppCommand::Compact,
                )
                .await?
            );
            assert_eq!(
                requests.lock().expect("recorded requests").len(),
                requests_before
            );

            let stale = codex_app_server_protocol::ItemCompletedNotification {
                thread_id: source_id.to_string(),
                turn_id: "turn-3".into(),
                completed_at_ms: 0,
                item: codex_app_server_protocol::ThreadItem::UserMessage {
                    id: "late-removed-item".into(),
                    client_id: None,
                    content: vec![codex_app_server_protocol::UserInput::Text {
                        text: "late removed suffix".into(),
                        text_elements: Vec::new(),
                    }],
                },
            };
            app.handle_app_server_event(
                &server,
                AppServerEvent::ServerNotification(Box::new(ServerNotification::ItemCompleted(
                    stale.clone(),
                ))),
            )
            .await;
            app.handle_active_thread_event(
                &mut tui,
                &mut server,
                ThreadBufferedEvent::Notification(Box::new(ServerNotification::ItemCompleted(
                    stale,
                ))),
            )
            .await?;
            assert!(events.try_recv().is_err());
            assert_eq!(app.chat_widget.capture_thread_input_state(), input);

            // Embedded attachment must not turn this guard into a remote-only check.
            app.app_server_target = crate::AppServerTarget::Embedded;
            assert!(app.thread_unavailable(source_id));
            let other_id = ThreadId::new();
            assert!(!app.thread_unavailable(other_id));
            let mut other_turn = codex_app_server_protocol::Turn {
                id: "other-turn".into(),
                items: Vec::new(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                status: codex_app_server_protocol::TurnStatus::InProgress,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            };
            app.enqueue_thread_notification(
                other_id,
                ServerNotification::TurnStarted(
                    codex_app_server_protocol::TurnStartedNotification {
                        thread_id: other_id.to_string(),
                        turn: other_turn.clone(),
                        agent_queue: None,
                    },
                ),
            )
            .await?;
            assert_eq!(
                app.thread_event_channels
                    .get(&other_id)
                    .expect("other thread")
                    .store
                    .lock()
                    .await
                    .active_turn_id(),
                Some(other_turn.id.as_str()),
            );
            other_turn.status = codex_app_server_protocol::TurnStatus::Completed;
            app.enqueue_thread_notification(
                other_id,
                ServerNotification::TurnCompleted(
                    codex_app_server_protocol::TurnCompletedNotification {
                        thread_id: other_id.to_string(),
                        turn: other_turn,
                    },
                ),
            )
            .await?;
            assert!(matches!(
                Box::pin(app.handle_event(
                    &mut tui,
                    &mut server,
                    AppEvent::RenameAgentsOverviewThread {
                        thread_id: other_id,
                        name: "unrelated thread".into(),
                    },
                ))
                .await?,
                AppRunControl::Continue
            ));
            assert_eq!(recorded_params(&requests, "thread/name/set").len(), 1);
            assert!(matches!(
                Box::pin(app.handle_event(&mut tui, &mut server, AppEvent::FollowTranscript,))
                    .await?,
                AppRunControl::Continue
            ));
            assert_eq!(app.chat_widget.capture_thread_input_state(), input);
            let mut snapshot = app
                .thread_event_channels
                .get(&source_id)
                .expect("source thread")
                .store
                .lock()
                .await
                .snapshot();
            snapshot.input_state = input.clone();
            app.replay_thread_snapshot(snapshot, /*resume_restored_queue*/ true);
            assert!(app.thread_unavailable(source_id));
            assert_eq!(app.chat_widget.composer_text_with_pending(), SELECTED);
            assert!(
                !std::iter::from_fn(|| events.try_recv().ok())
                    .any(|event| matches!(event, AppEvent::CodexOp(AppCommand::UserTurn { .. })))
            );
            assert!(matches!(
                Box::pin(app.handle_event(
                    &mut tui,
                    &mut server,
                    AppEvent::Exit(ExitMode::Immediate),
                ))
                .await?,
                AppRunControl::Exit(_)
            ));
        }
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn guarded_legacy_edit_replaces_history_and_restores_draft_in_both_viewports() -> Result<()> {
    for owned in [false, true] {
        let (mut app, mut events, (mut server, requests, proxy), source_id, _path) = prompt_source(
            ThreadHistoryMode::Legacy,
            HistoryCapabilities::Current,
            PromptImages::Remote,
        )
        .await?;
        app.config.features.disable(Feature::ForkPromptEdits);
        let mut tui = crate::tui::test_support::make_test_tui()?;
        tui.set_owned_screen(owned)?;
        drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
        let before = server
            .thread_read(source_id, /*include_turns*/ true)
            .await?;
        let selected = Arc::clone(
            &app.transcript_cells
                [nth_user_position(&app.transcript_cells, /*nth*/ 2).expect("selected prompt")],
        );
        let identity = selected
            .as_any()
            .downcast_ref::<crate::history_cell::UserHistoryCell>()
            .and_then(|cell| cell.identity.get())
            .cloned();
        assert!(identity.is_some());
        app.app_event_tx
            .send(AppEvent::CodexOp(AppCommand::Compact));
        app.app_event_tx.send(AppEvent::RevertSessionForPromptEdit {
            thread_id: source_id,
            selected_cell: Arc::clone(&selected),
            prompt: crate::chatwidget::UserMessage::from("queued stale edit"),
        });
        Box::pin(app.handle_event(
            &mut tui,
            &mut server,
            AppEvent::RevertSessionForPromptEdit {
                thread_id: source_id,
                selected_cell: selected,
                prompt: crate::chatwidget::UserMessage::from(SELECTED),
            },
        ))
        .await?;
        // The reset is queued behind any old transcript inserts; input stays blocked until it runs.
        assert_eq!(app.pending_thread_switch_resets, 1);
        assert!(app.thread_unavailable(source_id));
        let requests_before_finish = requests.lock().expect("recorded requests").len();
        loop {
            let event = events.try_recv().expect("queued canonical reset");
            let finishing = matches!(&event, AppEvent::FinishPromptRevert { .. });
            assert!(app.thread_unavailable(source_id));
            assert_eq!(app.chat_widget.composer_text_with_pending(), SELECTED);
            assert_eq!(
                requests.lock().expect("recorded requests").len(),
                requests_before_finish
            );
            Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
            if finishing {
                break;
            }
        }
        assert!(!app.thread_unavailable(source_id));
        drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
        assert_eq!(app.pending_thread_switch_resets, 0);
        assert_eq!(app.chat_widget.thread_id(), Some(source_id));
        assert_eq!(app.chat_widget.composer_text_with_pending(), SELECTED);
        let rollback = recorded_params(&requests, "thread/rollback");
        assert_eq!(
            rollback,
            vec![serde_json::json!({
                "threadId": source_id.to_string(),
                "numTurns": 2,
                "expectedStartTurnId": "turn-2",
                "expectedTurnCount": 4
            })]
        );
        assert!(recorded_params(&requests, "thread/revert").is_empty());
        assert!(recorded_params(&requests, "turn/start").is_empty());
        assert_eq!(
            server
                .thread_read(source_id, /*include_turns*/ true)
                .await?
                .turns,
            before.turns[..2],
        );
        let prompts = app
            .transcript_cells
            .iter()
            .filter_map(|cell| {
                cell.as_any()
                    .downcast_ref::<crate::history_cell::UserHistoryCell>()
                    .map(|cell| cell.message.clone())
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(prompts, @"
        older prompt
        retained prompt
        ");
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}
