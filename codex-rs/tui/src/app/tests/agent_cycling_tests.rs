use super::session_lifecycle_requests::display_test_thread;
use super::session_lifecycle_requests::recorded_params;
use super::session_lifecycle_requests::start_recording_app_server;
use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn shortcut_skips_closed_and_stale_unavailable_then_attaches_unviewed_idle_agent()
-> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let root = server.start_thread(&app.config).await?.session.thread_id;
    let foreign = server.start_thread(&app.config).await?.session.thread_id;
    let idle = ThreadId::from_string(
        &app_test_support::create_fake_parented_rollout_with_source(
            app.config.codex_home.as_path(),
            "2026-01-01T00-00-01",
            "2026-01-01T00:00:01Z",
            "saved idle transcript",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
            codex_protocol::protocol::SessionSource::SubAgent(
                codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                    parent_thread_id: root,
                    depth: 1,
                    agent_path: Some(
                        codex_protocol::AgentPath::try_from("/root/idle")
                            .map_err(color_eyre::eyre::Report::msg)?,
                    ),
                    agent_nickname: Some("idle".to_string()),
                    agent_role: None,
                },
            ),
            root.into(),
            root,
        )
        .map_err(color_eyre::eyre::Report::msg)?,
    )?;
    // Load it on the server without installing its attachment in this App.
    server
        .resume_thread(
            app.config.clone(),
            idle,
            crate::app_server_session::ResumeModelSettings::PreserveExistingThread,
        )
        .await?;
    let closed = ThreadId::new();
    let missing = ThreadId::new();
    let unloaded = ThreadId::from_string(
        &app_test_support::create_fake_parented_rollout_with_source(
            app.config.codex_home.as_path(),
            "2026-01-01T00-00-00",
            "2026-01-01T00:00:00Z",
            "saved closed transcript",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
            codex_protocol::protocol::SessionSource::Cli,
            root.into(),
            root,
        )
        .map_err(color_eyre::eyre::Report::msg)?,
    )?;
    let loaded_thread = server.thread_read(idle, /*include_turns*/ false).await?;
    let unloaded_thread = server
        .thread_read(unloaded, /*include_turns*/ false)
        .await?;
    assert_eq!(
        [
            (loaded_thread.status, loaded_thread.session_id),
            (unloaded_thread.status, unloaded_thread.session_id),
        ],
        [
            (
                codex_app_server_protocol::ThreadStatus::Idle,
                root.to_string()
            ),
            (
                codex_app_server_protocol::ThreadStatus::NotLoaded,
                root.to_string()
            ),
        ]
    );
    app.primary_thread_id = Some(root);
    app.last_subagent_backfill_attempt = Some(root);
    display_test_thread(&mut app, root);
    for (id, name) in [
        (root, "Main"),
        (closed, "closed"),
        (missing, "missing"),
        (unloaded, "unloaded"),
        (foreign, "stale foreign member"),
        (idle, "idle"),
    ] {
        app.upsert_agent_picker_thread(
            id,
            Some(name.to_string()),
            /*agent_role*/ None,
            /*is_closed*/ id == closed,
        );
        if id != root {
            app.agent_navigation.set_parent_thread_id(id, Some(root));
        }
    }
    assert!(!app.thread_event_channels.contains_key(&idle));
    requests.lock().expect("request recorder lock").clear();
    app.handle_tui_event(
        &mut tui,
        &mut server,
        TuiEvent::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)),
    )
    .await?;
    assert_eq!(app.current_displayed_thread_id(), Some(idle));
    let resumed = recorded_params(&requests, "thread/resume")
        .into_iter()
        .map(|params| params["threadId"].clone())
        .collect::<Vec<_>>();
    assert_eq!(resumed, vec![serde_json::json!(idle)]);
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, closed, missing, unloaded, foreign, idle]
    );
    // The selected label is the visible result of shortcut navigation; closed picker rows remain.
    insta::assert_snapshot!(
        app.agent_navigation.display_name(idle, Some(root)),
        @"idle"
    );
    requests.lock().expect("request recorder lock").clear();
    app.handle_tui_event(
        &mut tui,
        &mut server,
        TuiEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
    )
    .await?;
    assert_eq!(app.current_displayed_thread_id(), Some(root));
    let resumed = recorded_params(&requests, "thread/resume")
        .into_iter()
        .map(|params| params["threadId"].clone())
        .collect::<Vec<_>>();
    assert_eq!(resumed, vec![serde_json::json!(root)]);
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn unavailable_only_cycle_keeps_current_thread_and_never_resumes() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let root = ThreadId::new();
    let replay_only = ThreadId::new();
    let unavailable = ThreadId::new();
    app.primary_thread_id = Some(root);
    app.last_subagent_backfill_attempt = Some(root);
    display_test_thread(&mut app, root);
    for id in [root, replay_only, unavailable] {
        app.upsert_agent_picker_thread(
            id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
        );
    }
    app.agent_navigation
        .set_parent_thread_id(replay_only, Some(root));
    app.ensure_thread_channel(replay_only).mark_replay_only();
    app.agent_navigation
        .set_parent_thread_id(unavailable, Some(root));
    requests.lock().expect("request recorder lock").clear();
    while events.try_recv().is_ok() {}
    app.cycle_available_agent(&mut tui, &mut server, AgentNavigationDirection::Next)
        .await?;
    assert_eq!(app.current_displayed_thread_id(), Some(root));
    assert_eq!(
        recorded_params(&requests, "thread/resume"),
        Vec::<serde_json::Value>::new()
    );
    assert_eq!(recorded_params(&requests, "thread/read").len(), 1);
    let history = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => {
                Some(lines_to_single_string(&cell.display_lines(/*width*/ 80)))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(history, Vec::<String>::new());
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}
