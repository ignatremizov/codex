//! Detach uses real ownership and reload RPCs without exiting or submitting a saved draft.

use super::*;
use crate::chatwidget::agent_command::AgentSelector;
use crate::chatwidget::agent_command::AgentSelectorKind;
use core_test_support::responses;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn detach_reloads_the_same_thread_and_keeps_its_draft_explicitly_sendable() -> Result<()> {
    for (owned, via_closure, targeted, lifecycle_row) in [
        (false, false, false, false),
        (true, false, false, false),
        (false, true, false, false),
        (true, true, false, false),
        (false, false, true, false),
        (true, false, true, false),
        (false, true, true, false),
        (true, true, true, false),
        (true, true, true, true),
    ] {
        let (mut app, mut events, _) = make_test_app_with_channels().await;
        set_test_initial_prompt(&mut app, String::new());
        let model_server = MockServer::start().await;
        configure_agent_prompt_model_server(&mut app, &model_server);
        app.config.model_provider_id = "agent-detach-test".into();
        app.config.model_providers.insert(
            app.config.model_provider_id.clone(),
            app.config.model_provider.clone(),
        );
        app.config.features.enable(Feature::Collab)?;
        app.config.features.disable(Feature::MultiAgentV2)?;
        let (mut server, requests, proxy) = start_recording_app_server_with_realtime_speech(
            &app.config,
            HistoryCapabilities::CurrentWithLiveNotifications,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
            crate::app_server_session::ThreadParamsMode::Embedded,
            RecordingRequestBehavior {
                realtime: RealtimeRequestBehavior::Forward,
                agent_control: AgentControlRequestBehavior::Forward,
                blocked_older_turn_metadata: None,
            },
            LoaderOverrides::without_managed_config_for_tests(),
        )
        .await?;
        let mut tui = crate::tui::test_support::make_test_tui()?;
        tui.set_owned_screen(owned)?;
        let started = server.start_thread(&app.config).await?;
        let root = started.session.thread_id;
        app.enqueue_primary_thread_session(started.session, started.turns)
            .await?;
        let child = app
            .spawn_agent_from_command(
                &mut server,
                crate::app::SpawnAgentCommandArgs {
                    source_thread_id: root,
                    task: Some("work".into()),
                    role: None,
                    authored_selector: Some("new".into()),
                    model: None,
                    reasoning_effort: None,
                    prompt: None,
                    fork_mode: codex_app_server_protocol::AgentForkMode::None,
                    response_handling: Some(
                        codex_app_server_protocol::AgentResponseHandling::Presentation,
                    ),
                },
            )
            .await
            .expect("idle child");
        // Ordinary thread listings require a persisted preview. Complete one real
        // child turn before detachment rather than listing a never-used empty child.
        let response = responses::mount_sse_once(
            &model_server,
            responses::sse(vec![
                responses::ev_response_created("before-detach"),
                responses::ev_assistant_message("child-reply", "Completed before detach."),
                responses::ev_completed("before-detach"),
            ]),
        )
        .await;
        // Keep the turn in the app's workspace so resuming does not need a
        // directory choice from this unattended TUI.
        server
            .turn_start(
                child,
                "user",
                format!("test-turn-{child}"),
                vec![codex_app_server_protocol::UserInput::Text {
                    text: "Work completed before detach.".into(),
                    text_elements: Vec::new(),
                }],
                app.config.cwd.to_path_buf(),
                /*approval_policy*/ None,
                /*approvals_reviewer*/ None,
                TurnPermissionsOverride::Preserve,
                &[],
                "gpt-5".into(),
                /*effort*/ None,
                /*summary*/ None,
                /*service_tier*/ None,
                /*collaboration_mode*/ None,
                /*output_schema*/ None,
                /*cyber_access_program*/ None,
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            loop {
                if !response.requests().is_empty()
                    && matches!(
                        server
                            .thread_read(child, /*include_turns*/ false)
                            .await?
                            .status,
                        ThreadStatus::Idle
                    )
                {
                    return Ok::<(), color_eyre::Report>(());
                }
                tokio::time::sleep(Duration::from_millis(/*millis*/ 25)).await;
            }
        })
        .await??;
        let backend_id = server.backend_id();
        if !targeted {
            app.resume_target_session(
                &mut tui,
                &mut server,
                crate::resume_picker::SessionTarget {
                    backend_id,
                    path: None,
                    source_rollout_path: None,
                    thread_id: child,
                    cwd: Some(app.config.cwd.to_path_buf()),
                    history_mode: None,
                },
            )
            .await?;
        }
        while let Ok(event) = events.try_recv() {
            Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
        }
        app.chat_widget
            .restore_user_message_to_composer("Keep this draft after detach.".into());
        requests.lock().expect("recorder").clear();
        let label = app.thread_label(child);
        let mut detach_notices = Vec::new();
        if via_closure {
            assert_eq!(app.agent_root_thread_id(), Some(root));
            let lifecycle_item = lifecycle_row.then(|| ThreadItemPayload::CollabAgentToolCall {
                lifecycle: Some(codex_app_server_protocol::CollabAgentLifecycle {
                    action: codex_app_server_protocol::CollabAgentLifecycleAction::Detach,
                    root_thread_id: root.to_string(),
                }),
                id: "model-detach".into(),
                tool: codex_app_server_protocol::CollabAgentTool::CloseAgent,
                status: codex_app_server_protocol::CollabAgentToolCallStatus::InProgress,
                observe_commentary: None,
                wake_on_completion: None,
                target_messages: None,
                queue_input: None,
                input_batch: None,
                mailbox_input: None,
                sender_thread_id: root.to_string(),
                receiver_thread_ids: vec![child.to_string()],
                receiver_agents: Vec::new(),
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: Default::default(),
            });
            if let Some(item) = &lifecycle_item {
                app.track_agent_detachments(
                    &server,
                    &ServerNotification::ItemStarted(
                        codex_app_server_protocol::ItemStartedNotification::new(
                            root.to_string(),
                            "model-turn".into(),
                            item.clone(),
                            1,
                        ),
                    ),
                )
                .await;
            }
            // Receiving a detach initiated elsewhere must retain this window and its input.
            server.thread_detach(child).await?;
            tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
                loop {
                    let event = server.next_event().await.expect("detach notifications");
                    let closed = matches!(&event.event, AppServerEvent::ServerNotification(notification)
                        if matches!(notification.as_ref(), ServerNotification::ThreadClosed(closed)
                            if closed.thread_id == child.to_string()));
                    app.handle_backend_event(&server, event).await;
                    if closed {
                        break;
                    }
                }
            }).await?;
            if let Some(mut item) = lifecycle_item {
                if let ThreadItemPayload::CollabAgentToolCall { status, .. } = &mut item {
                    *status = codex_app_server_protocol::CollabAgentToolCallStatus::Completed;
                }
                app.track_agent_detachments(
                    &server,
                    &ServerNotification::ItemCompleted(
                        codex_app_server_protocol::ItemCompletedNotification::new(
                            root.to_string(),
                            "model-turn".into(),
                            item,
                            Some(1),
                            2,
                        ),
                    ),
                )
                .await;
                assert!(app.agent_navigation.pending_detachment_calls.is_empty());
            }
            if targeted {
                assert!(!app.thread_unavailable(root));
            } else {
                assert!(app.thread_unavailable(child));
            }
            assert_eq!(
                app.chat_widget.composer_text_with_pending(),
                "Keep this draft after detach."
            );
            let mut notices = Vec::new();
            while let Ok(event) = events.try_recv() {
                assert!(!matches!(
                    event,
                    AppEvent::Exit(_) | AppEvent::CodexOp(AppCommand::UserTurn { .. })
                ));
                if let AppEvent::InsertHistoryCell(cell) = &event {
                    let rendered = lines_to_single_string(&cell.display_lines(/*width*/ 200));
                    if rendered.contains("This conversation moved.") {
                        notices.push(rendered);
                    } else if rendered.contains("Detached ") {
                        detach_notices.push(
                            rendered
                                .replace(&label, "Worker")
                                .replace(&child.to_string(), "[child]"),
                        );
                    }
                }
                Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
            }
            if !targeted {
                insta::allow_duplicates! {
                    insta::assert_snapshot!(notices.join("\n"), @"• This conversation moved. Resume to continue; your draft is preserved.");
                }
                app.resume_target_session(
                    &mut tui,
                    &mut server,
                    crate::resume_picker::SessionTarget {
                        backend_id,
                        path: None,
                        source_rollout_path: None,
                        thread_id: child,
                        cwd: Some(app.config.cwd.to_path_buf()),
                        history_mode: None,
                    },
                )
                .await?;
            }
        } else if targeted {
            Box::pin(app.handle_event(
                &mut tui,
                &mut server,
                AppEvent::DetachAgent {
                    source_thread_id: root,
                    selector: Some(AgentSelector {
                        kind: AgentSelectorKind::Ref(2),
                        authored: "2".into(),
                    }),
                },
            ))
            .await?;
        } else {
            app.detach_displayed_agent(&mut tui, &mut server, child)
                .await?;
        }
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(
                event,
                AppEvent::Exit(_) | AppEvent::CodexOp(AppCommand::UserTurn { .. })
            ));
            if targeted && let AppEvent::InsertHistoryCell(cell) = &event {
                let rendered = lines_to_single_string(&cell.display_lines(/*width*/ 200));
                if rendered.contains("Detached ") {
                    detach_notices.push(
                        rendered
                            .replace(&label, "Worker")
                            .replace(&child.to_string(), "[child]"),
                    );
                }
            }
            Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
        }
        let selected = if targeted { root } else { child };
        assert_eq!(
            (
                app.active_thread_id,
                app.primary_thread_id,
                app.chat_widget.thread_id()
            ),
            (Some(selected), Some(selected), Some(selected)),
            "owned={owned}, via_closure={via_closure}, targeted={targeted}",
        );
        assert!(!app.thread_unavailable(selected));
        if targeted && !lifecycle_row {
            insta::allow_duplicates! {
                insta::assert_snapshot!(detach_notices.join("\n"), @"• Detached Worker. Resume [child] in another TUI to continue independently.");
            }
        } else if lifecycle_row {
            assert!(
                detach_notices.is_empty(),
                "the lifecycle row owns the notice"
            );
        }
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "Keep this draft after detach."
        );
        assert_eq!(
            recorded_params(&requests, "thread/detach"),
            vec![serde_json::json!({"threadId": child})]
        );
        assert!(recorded_params(&requests, "turn/start").is_empty());
        let aliases = server.agent_aliases(child).await?;
        assert!(
            aliases
                .iter()
                .any(|alias| alias.thread_id == child.to_string() && alias.agent_ref == "1")
        );
        let independent = server.thread_read(child, /*include_turns*/ false).await?;
        assert_eq!(
            crate::app_server_session::thread_parent_thread_id(&independent),
            None,
            "historical spawn provenance must not become a current parent",
        );
        assert_eq!(independent.session_id, child.to_string());
        let listed = server
            .thread_list(serde_json::from_value(serde_json::json!({
                "sourceKinds": ["cli", "appServer", "subAgentThreadSpawn"],
            }))?)
            .await?
            .data
            .into_iter()
            .find(|thread| thread.id == child.to_string())
            .expect("detached child remains listed");
        assert_eq!(
            (listed.session_id, listed.parent_thread_id),
            (child.to_string(), None),
        );
        if !targeted {
            assert_eq!(app.agent_navigation.parent_thread_id(child), None);
        }
        assert!(!matches!(
            server
                .thread_read(root, /*include_turns*/ false)
                .await?
                .status,
            ThreadStatus::NotLoaded
        ));
        app.chat_widget
            .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, AppEvent::CodexOp(AppCommand::UserTurn { .. })))
        );
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn detach_rechecks_current_input_authority_before_ownership_rpc() -> Result<()> {
    for (provider_error, targeted) in [(true, false), (true, true), (false, false), (false, true)] {
        let (mut app, _events, _ops) = make_test_app_with_channels().await;
        let (mut server, requests, proxy) = start_recording_app_server(
            &app.config,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
        )
        .await?;
        let started = server.start_thread(&app.config).await?;
        let source = started.session.thread_id;
        app.active_thread_id = Some(source);
        app.chat_widget.handle_thread_session_quiet(started.session);
        app.ensure_thread_channel(source);
        app.chat_widget
            .restore_user_message_to_composer("Keep the quarantined draft.".into());
        if provider_error {
            app.chat_widget.show_provider_error(
                codex_app_server_protocol::ModelProviderError::ModelProviderNotConfigured {
                    model_provider: "unavailable-provider".into(),
                },
                /*rejected_message_id*/ None,
            );
            assert!(!app.chat_widget.is_external_writer_view());
        } else {
            app.chat_widget.show_external_writer_thread();
        }
        requests.lock().expect("recorder").clear();
        let selector = targeted.then(|| {
            let target = ThreadId::new();
            AgentSelector {
                kind: AgentSelectorKind::Id(target),
                authored: target.to_string(),
            }
        });
        let mut tui = crate::tui::test_support::make_test_tui()?;
        app.detach_agent_from_command(&mut tui, &mut server, source, selector)
            .await?;
        assert!(recorded_params(&requests, "thread/detach").is_empty());
        assert!(recorded_params(&requests, "turn/start").is_empty());
        assert_eq!(app.active_thread_id, Some(source));
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "Keep the quarantined draft."
        );
        assert!(app.chat_widget.direct_input_is_blocked());
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}
