//! Opt-in prompt forks share canonical selection and ordinary thread attachment.

use super::session_lifecycle_requests::HistoryCapabilities;
use super::session_lifecycle_requests::RecordingAppServer;
use super::session_lifecycle_requests::recorded_params;
use super::session_lifecycle_requests::start_recording_app_server_with_history;
use super::*;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ImageReference;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::user_input::UserInput as CoreUserInput;
use pretty_assertions::assert_eq;

const SELECTED: &str = "use $skill @sample $google-calendar";

#[path = "legacy_prompt_edit_tests.rs"]
mod legacy;

#[path = "forked_prompt_edit_tests.rs"]
mod forked;

#[derive(Clone, Copy)]
enum PromptImages {
    Remote,
    LocalAndRemote,
}

async fn drain_fork_attachment(
    app: &mut App,
    tui: &mut tui::Tui,
    server: &mut AppServerSession,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> Result<()> {
    let fork_id = app
        .chat_widget
        .thread_id()
        .expect("attached fork")
        .to_string();
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            let event = server.next_event().await.expect("app-server events");
            let fork_started = matches!(&event, AppServerEvent::ServerNotification(notification)
                if matches!(notification.as_ref(), ServerNotification::ThreadStarted(started)
                    if started.thread.id == fork_id));
            app.handle_app_server_event(server, event).await;
            while let Some(event) = app
                .active_thread_rx
                .as_mut()
                .and_then(|receiver| receiver.try_recv().ok())
            {
                app.handle_thread_event_now(event);
            }
            if fork_started {
                return;
            }
        }
    })
    .await?;
    drain_attach_events(app, tui, server, events).await
}

async fn drain_attach_events(
    app: &mut App,
    tui: &mut tui::Tui,
    server: &mut AppServerSession,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> Result<()> {
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(
            event,
            AppEvent::CodexOp(AppCommand::UserTurn { .. })
        ));
        Box::pin(app.handle_event(tui, server, event)).await?;
    }
    Ok(())
}

async fn prompt_source(
    mode: ThreadHistoryMode,
    capabilities: HistoryCapabilities,
    images: PromptImages,
) -> Result<(
    App,
    tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    RecordingAppServer,
    ThreadId,
    PathBuf,
)> {
    let (mut app, events, _) = make_test_app_with_channels().await;
    app.config.features.enable(Feature::ForkPromptEdits);
    let timestamp = "2026-01-05T12-00-00";
    let create = match mode {
        ThreadHistoryMode::Legacy => app_test_support::create_fake_rollout,
        ThreadHistoryMode::Paginated => app_test_support::create_fake_paginated_rollout,
    };
    let id = create(
        app.config.codex_home.as_path(),
        timestamp,
        "2026-01-05T12:00:00Z",
        "unused",
        Some(&app.config.model_provider_id),
        /*git_info*/ None,
    )?;
    let path = app_test_support::rollout_path(app.config.codex_home.as_path(), timestamp, &id);
    let contents = std::fs::read_to_string(&path)?;
    let meta = contents.lines().next().expect("rollout metadata");
    std::fs::write(&path, format!("{meta}\n"))?;
    let id = ThreadId::from_string(&id)?;
    for (index, text) in ["older prompt", "retained prompt", SELECTED, SELECTED]
        .into_iter()
        .enumerate()
    {
        let turn_id = format!("turn-{index}");
        let mut content = vec![CoreUserInput::Text {
            text: text.into(),
            text_elements: if text == SELECTED {
                vec![TextElement::new((4..10).into(), Some("$skill".into()))]
            } else {
                Vec::new()
            },
        }];
        if text == SELECTED {
            content.extend([
                CoreUserInput::Skill {
                    name: "skill".into(),
                    path: test_path_buf("/tmp/skills/skill/SKILL.md"),
                },
                CoreUserInput::Mention {
                    name: "Sample Plugin".into(),
                    path: "plugin://sample@test".into(),
                },
                CoreUserInput::Mention {
                    name: "Google Calendar".into(),
                    path: "app://google_calendar".into(),
                },
                CoreUserInput::Image {
                    image: ImageReference::Inline {
                        image_url: "https://example.com/prompt.png".into(),
                    },
                    detail: None,
                },
            ]);
            if matches!(images, PromptImages::LocalAndRemote) {
                content.push(CoreUserInput::LocalImage {
                    path: test_path_buf("/tmp/prompt-image.png"),
                    detail: None,
                });
            }
        }
        for item in [
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: ModeKind::default(),
                agent_queue: None,
            })),
            RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: id,
                turn_id: turn_id.clone(),
                item: TurnItem::UserMessage(UserMessageItem {
                    id: format!("user-{index}"),
                    client_id: None,
                    content,
                }),
                started_at_ms: None,
                completed_at_ms: 0,
            })),
            RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id,
                last_agent_message: None,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            })),
        ] {
            codex_rollout::append_rollout_item_to_path(&path, &item).await?;
        }
    }
    let mut recording = start_recording_app_server_with_history(
        &app.config,
        capabilities,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
        LoaderOverrides::default(),
    )
    .await?;
    let started = recording
        .0
        .resume_thread(
            &app.local_settings,
            app.config.clone(),
            id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    Ok((app, events, recording, id, path))
}

#[tokio::test]
async fn goal_free_and_safety_retry_fork_requests_preserve_both_history_modes() -> Result<()> {
    use crate::app_server_session::ForkGoalContinuation;
    use crate::app_server_session::ForkPermissionMode;

    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        let (app, _events, (mut server, requests, proxy), source_id, _path) =
            prompt_source(mode, HistoryCapabilities::Current, PromptImages::Remote).await?;
        server
            .fork_thread_with_permission_mode(
                &app.local_settings,
                app.config.clone(),
                source_id,
                ForkPermissionMode::InheritSaved,
            )
            .await?;
        server
            .fork_side_thread(&app.local_settings, app.config.clone(), source_id)
            .await?;
        server
            .fork_thread_at(
                &app.local_settings,
                app.config.clone(),
                source_id,
                /*last_turn_id*/ None,
                /*before_turn_id*/ Some("turn-2".into()),
                ForkGoalContinuation::InheritForSafetyRetry,
                /*selected_profile*/ None,
            )
            .await?;
        let forks = recorded_params(&requests, "thread/fork");
        let params: Vec<codex_app_server_protocol::ThreadForkParams> = forks
            .into_iter()
            .map(serde_json::from_value)
            .collect::<serde_json::Result<_>>()?;
        let flags: Vec<_> = params
            .into_iter()
            .map(|params| (params.thread_id, params.defer_goal_continuation))
            .collect();
        assert_eq!(
            flags,
            vec![
                (source_id.to_string(), false),
                (source_id.to_string(), false),
                (source_id.to_string(), true),
            ]
        );
        assert!(recorded_params(&requests, "turn/start").is_empty());
        assert_eq!(
            server
                .thread_read(source_id, /*include_turns*/ false)
                .await?
                .history_mode,
            mode
        );
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn working_directory_forks_are_goal_free_in_both_history_modes() -> Result<()> {
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        let (mut app, _events, (mut server, requests, proxy), source_id, _path) =
            prompt_source(mode, HistoryCapabilities::Current, PromptImages::Remote).await?;
        let destination = tempdir()?;
        let cwd = destination.path().canonicalize()?;
        crate::legacy_core::config::set_project_trust_level(
            app.config.codex_home.as_path(),
            &cwd,
            codex_protocol::config_types::TrustLevel::Trusted,
        )
        .map_err(|error| color_eyre::eyre::eyre!(error.to_string()))?;
        let mut tui = crate::tui::test_support::make_test_tui()?;
        Box::pin(app.handle_event(
            &mut tui,
            &mut server,
            AppEvent::ChangeWorkingDirectory {
                thread_id: source_id,
                requested_cwd: cwd,
            },
        ))
        .await?;
        let pending = app
            .pending_working_directory_change
            .take()
            .expect("queued directory change");
        Box::pin(app.finish_working_directory_change(&mut tui, &mut server, pending)).await;
        let forks = recorded_params(&requests, "thread/fork");
        assert_eq!(forks.len(), 1);
        let params: codex_app_server_protocol::ThreadForkParams =
            serde_json::from_value(forks[0].clone())?;
        assert_eq!(
            (params.thread_id, params.defer_goal_continuation),
            (source_id.to_string(), false)
        );
        assert!(recorded_params(&requests, "turn/start").is_empty());
        let fork_id = app
            .chat_widget
            .thread_id()
            .expect("attached directory fork");
        assert_ne!(fork_id, source_id);
        assert_eq!(
            server
                .thread_read(fork_id, /*include_turns*/ false)
                .await?
                .history_mode,
            mode
        );
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn prompt_forks_preserve_sources_and_drafts_after_owned_and_inline_attachment() -> Result<()>
{
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        for owned in [false, true] {
            for selected in [0, 2] {
                let (mut app, mut events, (mut server, requests, proxy), source_id, path) =
                    prompt_source(mode, HistoryCapabilities::Current, PromptImages::Remote).await?;
                let mut tui = crate::tui::test_support::make_test_tui()?;
                tui.set_owned_screen(owned)?;
                drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
                let source = server
                    .thread_read(source_id, /*include_turns*/ true)
                    .await?;
                let source_bytes = std::fs::read(&path)?;
                let source_cell = Arc::clone(
                    &app.transcript_cells
                        [nth_user_position(&app.transcript_cells, selected).unwrap()],
                );
                // Both actual viewport paths must select the same cell, not a nearby equal prompt.
                app.open_transcript_overlay(&mut tui);
                // Normalize scroll/layout state through the outer render loop before navigation.
                app.handle_tui_event(&mut tui, &mut server, TuiEvent::Draw)
                    .await?;
                for code in [KeyCode::Esc]
                    .into_iter()
                    .chain(std::iter::repeat_n(KeyCode::Left, 3 - selected))
                    .chain([KeyCode::Enter])
                {
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
                    .expect("viewport confirmation should request an edit");
                let AppEvent::RevertSessionForPromptEdit { selected_cell, .. } = &edit else {
                    unreachable!()
                };
                assert!(Arc::ptr_eq(selected_cell, &source_cell));
                Box::pin(app.handle_event(&mut tui, &mut server, edit)).await?;
                let before_drain = app.chat_widget.capture_thread_input_state();
                drain_fork_attachment(&mut app, &mut tui, &mut server, &mut events).await?;
                assert_eq!(app.chat_widget.capture_thread_input_state(), before_drain);
                assert_eq!(app.pending_thread_switch_resets, 0);
                let fork_id = app.chat_widget.thread_id().expect("attached fork");
                assert_ne!(fork_id, source_id);
                let forks = recorded_params(&requests, "thread/fork");
                assert_eq!(forks.len(), 1);
                assert_eq!(forks[0]["threadId"], source_id.to_string());
                assert_eq!(forks[0]["beforeTurnId"], format!("turn-{selected}"));
                let params: codex_app_server_protocol::ThreadForkParams =
                    serde_json::from_value(forks[0].clone())?;
                assert!(!params.defer_goal_continuation);
                assert!(recorded_params(&requests, "thread/start").is_empty());
                assert!(recorded_params(&requests, "thread/revert").is_empty());
                assert!(recorded_params(&requests, "turn/start").is_empty());
                let fork = server.thread_read(fork_id, /*include_turns*/ true).await?;
                assert_eq!(fork.history_mode, mode);
                assert_eq!(fork.forked_from_id, Some(source_id.to_string()));
                assert_eq!(fork.turns, source.turns[..selected]);
                assert_eq!(
                    server
                        .thread_read(source_id, /*include_turns*/ true)
                        .await?
                        .turns,
                    source.turns,
                );
                assert_eq!(std::fs::read(&path)?, source_bytes);
                let expected_text = if selected == 0 {
                    "older prompt"
                } else {
                    SELECTED
                };
                assert_eq!(app.chat_widget.composer_text_with_pending(), expected_text);
                if selected == 2 {
                    assert_eq!(
                        app.chat_widget.remote_image_urls(),
                        vec!["https://example.com/prompt.png".to_string()],
                    );
                    let restored = app.chat_widget.capture_thread_input_state();
                    app.chat_widget.restore_thread_input_state(
                        /*input_state*/ None,
                        crate::chatwidget::ThreadInputStateRestoreMode {
                            preserve_in_flight_turn: false,
                        },
                    );
                    app.chat_widget.restore_user_message_to_composer(
                        crate::chatwidget::UserMessage {
                            text: SELECTED.into(),
                            local_images: Vec::new(),
                            remote_image_urls: vec!["https://example.com/prompt.png".into()],
                            text_elements: vec![TextElement::new(
                                (4..10).into(),
                                Some("$skill".into()),
                            )],
                            mention_bindings: vec![
                                crate::bottom_pane::MentionBinding {
                                    sigil: '$',
                                    mention: "skill".into(),
                                    path: test_path_buf("/tmp/skills/skill/SKILL.md")
                                        .to_string_lossy()
                                        .into_owned(),
                                },
                                crate::bottom_pane::MentionBinding {
                                    sigil: '@',
                                    mention: "sample".into(),
                                    path: "plugin://sample@test".into(),
                                },
                                crate::bottom_pane::MentionBinding {
                                    sigil: '$',
                                    mention: "google-calendar".into(),
                                    path: "app://google_calendar".into(),
                                },
                            ],
                        },
                    );
                    assert_eq!(app.chat_widget.capture_thread_input_state(), restored);
                    app.chat_widget.restore_thread_input_state(
                        restored,
                        crate::chatwidget::ThreadInputStateRestoreMode {
                            preserve_in_flight_turn: false,
                        },
                    );
                    let rendered = app
                        .transcript_cells
                        .iter()
                        .filter(|cell| cell.as_any().is::<UserHistoryCell>())
                        .flat_map(|cell| cell.display_lines(/*width*/ 80))
                        .collect::<Vec<_>>();
                    let rendered = format!(
                        "{}\nDraft: {}",
                        lines_to_single_string(&rendered).trim(),
                        app.chat_widget.composer_text_with_pending(),
                    );
                    insta::allow_duplicates! {
                        if owned {
                            insta::assert_snapshot!("owned_prompt_fork_retained_history", rendered);
                        } else {
                            insta::assert_snapshot!("inline_prompt_fork_retained_history", rendered);
                        }
                    }
                    // The full draft above includes canonical mention bindings even without a
                    // discovered/authorized catalog. Submission must still retain its image and
                    // text spans; it must not bypass mention authorization to echo hidden targets.
                    app.chat_widget
                        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    let command = std::iter::from_fn(|| events.try_recv().ok())
                        .find_map(|event| match event {
                            AppEvent::CodexOp(AppCommand::UserTurn { items, .. }) => Some(items),
                            _ => None,
                        })
                        .expect("only explicit Enter should submit the restored draft");
                    assert!(command.iter().any(|item| matches!(item,
                        codex_app_server_protocol::UserInput::Text { text, text_elements }
                            if text == SELECTED && text_elements.len() == 1)));
                    assert_eq!(
                        command
                            .iter()
                            .filter(|item| matches!(
                                item,
                                codex_app_server_protocol::UserInput::Image { .. }
                            ))
                            .count(),
                        1
                    );
                }
                tui.set_owned_screen(/*owned*/ false)?;
                server.shutdown().await?;
                proxy.await??;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn prompt_fork_guards_and_selection_failures_do_not_mutate_the_source() -> Result<()> {
    for scenario in [
        "remote_image",
        "remote_command",
        "permissions",
        "stale_cell",
        "stale_thread",
        "changed_tail",
        "offline",
        "external_writer",
        "side",
        "misalignment",
    ] {
        let (mut app, mut events, (mut server, requests, proxy), source_id, path) = prompt_source(
            ThreadHistoryMode::Paginated,
            HistoryCapabilities::Current,
            PromptImages::Remote,
        )
        .await?;
        let mut tui = crate::tui::test_support::make_test_tui()?;
        drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
        let source_bytes = std::fs::read(&path)?;
        let index = nth_user_position(&app.transcript_cells, /*nth*/ 2).unwrap();
        let selected_cell = Arc::clone(&app.transcript_cells[index]);
        let mut prompt = crate::chatwidget::UserMessage {
            text: SELECTED.into(),
            local_images: Vec::new(),
            remote_image_urls: vec!["https://example.com/prompt.png".into()],
            text_elements: vec![TextElement::new((4..10).into(), Some("$skill".into()))],
            mention_bindings: Vec::new(),
        };
        let mut event_thread = source_id;
        match scenario {
            "remote_image" | "remote_command" => {
                app.app_server_target = crate::AppServerTarget::Remote {
                    endpoint: crate::RemoteAppServerEndpoint::WebSocket {
                        websocket_url: "ws://127.0.0.1:4500".into(),
                        auth_token: None,
                    },
                };
                if scenario == "remote_image" {
                    prompt
                        .local_images
                        .push(crate::bottom_pane::LocalImageAttachment {
                            placeholder: "[Image #1]".into(),
                            path: test_path_buf("/tmp/local.png"),
                        });
                } else {
                    prompt.text = "/help".into();
                }
            }
            "permissions" => {
                app.pending_server_profiles.insert(
                    source_id,
                    PermissionProfileSelection {
                        profile_id: ":read-only".into(),
                        approval_policy: None,
                        approvals_reviewer: None,
                        display_label: "Read only".into(),
                    },
                );
            }
            "stale_cell" => {
                app.transcript_cells.remove(index);
            }
            "stale_thread" => {
                event_thread = ThreadId::new();
            }
            "changed_tail" => {
                app.thread_event_channels[&source_id]
                    .store
                    .lock()
                    .await
                    .push_notification(turn_started_notification(source_id, "newer-turn"));
            }
            "offline" => {
                app.reconnect.offline = true;
            }
            "external_writer" => {
                app.chat_widget.show_external_writer_thread();
            }
            "side" => {
                app.chat_widget
                    .set_side_conversation_active(/*active*/ true);
            }
            "misalignment" => {
                app.chat_widget.on_misalignment_policy_violation();
            }
            _ => unreachable!(),
        }
        Box::pin(app.handle_event(
            &mut tui,
            &mut server,
            AppEvent::RevertSessionForPromptEdit {
                thread_id: event_thread,
                selected_cell,
                prompt,
            },
        ))
        .await?;
        assert!(
            recorded_params(&requests, "thread/fork").is_empty(),
            "{scenario}"
        );
        assert!(
            recorded_params(&requests, "thread/revert").is_empty(),
            "{scenario}"
        );
        assert_eq!(std::fs::read(&path)?, source_bytes, "{scenario}");
        assert_eq!(app.chat_widget.thread_id(), Some(source_id));
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn prompt_fork_keeps_exact_selection_when_an_older_page_arrives() -> Result<()> {
    let (mut app, mut events, (mut server, requests, proxy), source_id, path) = prompt_source(
        ThreadHistoryMode::Paginated,
        HistoryCapabilities::Current,
        PromptImages::Remote,
    )
    .await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
    let mut settings = app.local_settings.clone();
    settings.transcript_mode = crate::transcript_mode::TranscriptMode::Terminal;
    settings.tui.alternate_screen = codex_config::types::AltScreenMode::Never;
    settings.tui.terminal_resize_reflow_max_rows = Some(2);
    let started = server
        .resume_thread(
            &settings,
            app.config.clone(),
            source_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    app.transcript_cells.clear();
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
    assert!(server.has_older_history(source_id));
    let last = user_count(&app.transcript_cells) - 1;
    app.backtrack.base_id = Some(source_id);
    app.backtrack.nth_user_message = last;
    let selection = app
        .confirm_backtrack_from_main()
        .expect("last visible prompt");
    app.apply_backtrack_selection(selection);
    let edit = std::iter::from_fn(|| events.try_recv().ok())
        .find(|event| matches!(event, AppEvent::RevertSessionForPromptEdit { .. }))
        .expect("queued edit");
    let cursor = server
        .begin_older_history_page(source_id)
        .expect("older page");
    let page = server
        .thread_items_page(
            source_id,
            /*turn_id*/ None,
            Some(cursor.clone()),
            /*limit*/ 1,
        )
        .await?;
    let stale_page = page.clone();
    app.handle_older_history_page(&mut tui, &mut server, source_id, &cursor, Ok(page))
        .await?;
    assert!(user_count(&app.transcript_cells) > last + 1);
    let bytes = std::fs::read(&path)?;
    Box::pin(app.handle_event(&mut tui, &mut server, edit)).await?;
    drain_fork_attachment(&mut app, &mut tui, &mut server, &mut events).await?;
    let fork_id = app.chat_widget.thread_id().expect("fork");
    let before_stale = app.chat_widget.capture_thread_input_state();
    let transcript_len = app.transcript_cells.len();
    app.handle_older_history_page(&mut tui, &mut server, source_id, &cursor, Ok(stale_page))
        .await?;
    drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
    assert_eq!(app.chat_widget.capture_thread_input_state(), before_stale);
    assert_eq!(app.transcript_cells.len(), transcript_len);
    assert_eq!(
        recorded_params(&requests, "thread/fork")[0]["beforeTurnId"],
        "turn-3"
    );
    assert_eq!(
        server
            .thread_read(fork_id, /*include_turns*/ true)
            .await?
            .turns
            .iter()
            .map(|turn| turn.id.as_str())
            .collect::<Vec<_>>(),
        vec!["turn-0", "turn-1", "turn-2"],
    );
    assert_eq!(std::fs::read(path)?, bytes);
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn created_prompt_fork_survives_history_hydration_failure_without_retry() -> Result<()> {
    let (mut app, mut events, (mut server, requests, proxy), source_id, path) = prompt_source(
        ThreadHistoryMode::Paginated,
        HistoryCapabilities::ForkHydrationFails,
        PromptImages::Remote,
    )
    .await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
    let bytes = std::fs::read(&path)?;
    app.backtrack.base_id = Some(source_id);
    app.backtrack.nth_user_message = 2;
    let selection = app.confirm_backtrack_from_main().expect("selected prompt");
    app.apply_backtrack_selection(selection);
    drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
    drain_fork_attachment(&mut app, &mut tui, &mut server, &mut events).await?;
    let fork_id = app
        .chat_widget
        .thread_id()
        .expect("created fork must stay attached");
    assert_ne!(fork_id, source_id);
    assert_eq!(recorded_params(&requests, "thread/fork").len(), 1);
    assert_eq!(app.chat_widget.composer_text_with_pending(), SELECTED);
    assert!(recorded_params(&requests, "turn/start").is_empty());
    assert_eq!(std::fs::read(path)?, bytes);
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn prompt_fork_preserves_local_images_and_restores_draft_on_request_failure() -> Result<()> {
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        for fail_request in [false, true] {
            let (mut app, mut events, (mut server, requests, proxy), source_id, path) =
                prompt_source(
                    mode,
                    HistoryCapabilities::Current,
                    PromptImages::LocalAndRemote,
                )
                .await?;
            let mut tui = crate::tui::test_support::make_test_tui()?;
            drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
            let source = server
                .thread_read(source_id, /*include_turns*/ true)
                .await?;
            let bytes = std::fs::read(&path)?;
            app.backtrack.base_id = Some(source_id);
            app.backtrack.nth_user_message = 2;
            let selection = app
                .confirm_backtrack_from_main()
                .expect("prompt with local image");
            let mut expected_prompt = selection.prompt.clone();
            let ThreadItem::UserMessage { content, .. } = &source.turns[2].items[0] else {
                panic!("selected canonical prompt");
            };
            expected_prompt.mention_bindings =
                crate::chatwidget::mention_bindings_from_user_inputs(content, SELECTED);
            if fail_request {
                // The existing recording server rejects fork cwd values ending in "failure".
                let cwd = app.config.codex_home.join("failure");
                std::fs::create_dir_all(&cwd)?;
                let mut session = app.thread_event_channels[&source_id]
                    .store
                    .lock()
                    .await
                    .session
                    .clone()
                    .expect("source session");
                session.cwd = cwd;
                app.chat_widget.handle_thread_session_quiet(session);
            }
            app.apply_backtrack_selection(selection);
            drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
            if fail_request {
                assert_eq!(app.chat_widget.thread_id(), Some(source_id));
            } else {
                drain_fork_attachment(&mut app, &mut tui, &mut server, &mut events).await?;
                assert_ne!(app.chat_widget.thread_id(), Some(source_id));
            }
            let actual = app.chat_widget.capture_thread_input_state();
            app.chat_widget.restore_thread_input_state(
                /*input_state*/ None,
                crate::chatwidget::ThreadInputStateRestoreMode {
                    preserve_in_flight_turn: false,
                },
            );
            app.chat_widget
                .restore_user_message_to_composer(expected_prompt);
            assert_eq!(app.chat_widget.capture_thread_input_state(), actual);
            assert_eq!(recorded_params(&requests, "thread/fork").len(), 1);
            assert!(recorded_params(&requests, "turn/start").is_empty());
            assert_eq!(std::fs::read(&path)?, bytes);
            server.shutdown().await?;
            proxy.await??;
        }
    }
    Ok(())
}
