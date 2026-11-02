use super::*;
use app_test_support::create_fake_rollout;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn ultra_picker_selection_persists_across_new_sessions_and_config_reload() -> Result<()> {
    for mode in [ModeKind::Default, ModeKind::Plan] {
        let (mut app, mut events, _ops) = make_test_app_with_channels().await;
        let cwd = tempdir()?;
        app.config.cwd = cwd.path().abs();
        app.launch_cwd = app.config.cwd.to_path_buf();
        let model = "gpt-5.6-terra";
        app.config.model = Some(model.into());
        app.config.model_reasoning_effort = Some(ReasoningEffortConfig::Max);
        app.config.plan_mode_reasoning_effort = Some(ReasoningEffortConfig::Low);
        let config_path = app.config.codex_home.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "model = '{model}'\nmodel_reasoning_effort = 'max'\nplan_mode_reasoning_effort = 'low'\n"
            ),
        )?;
        let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
        let started = server.start_thread(&app.config).await?;
        let thread_id = started.session.thread_id;
        app.enqueue_primary_thread_session(started.session, started.turns)
            .await?;
        app.chat_widget
            .set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
        if mode == ModeKind::Plan {
            app.chat_widget.handle_key_event(KeyCode::BackTab.into());
        }
        while events.try_recv().is_ok() {}
        let preset = app
            .model_catalog
            .try_list_models()?
            .into_iter()
            .find(|preset| preset.model == model)
            .ok_or_else(|| color_eyre::eyre::eyre!("missing model preset"))?;
        app.chat_widget.open_advanced_reasoning_popup(preset);
        app.chat_widget.handle_key_event(KeyCode::Down.into());
        app.chat_widget.handle_key_event(KeyCode::Enter.into());
        let selection = std::iter::from_fn(|| events.try_recv().ok())
            .find(|event| matches!(event, AppEvent::ApplyAdvancedReasoning { .. }))
            .ok_or_else(|| color_eyre::eyre::eyre!("missing Ultra picker selection"))?;
        let mut tui = crate::tui::test_support::make_test_tui()?;
        Box::pin(app.handle_event(&mut tui, &mut server, selection)).await?;
        let settings = next_thread_settings_updated(&mut server, thread_id).await;
        assert_eq!(
            (
                settings.thread_settings.model.as_str(),
                settings.thread_settings.effort.clone(),
                app.chat_widget.current_reasoning_effort(),
            ),
            (
                model,
                Some(ReasoningEffortConfig::Ultra),
                Some(ReasoningEffortConfig::Ultra),
            )
        );
        assert_eq!(
            toml::from_str::<toml::Value>(&std::fs::read_to_string(&config_path)?)?,
            toml::Value::Table(toml::toml! {
                model = "gpt-5.6-terra"
                model_reasoning_effort = "ultra"
                plan_mode_reasoning_effort = "low"
            })
        );

        Box::pin(app.handle_event(&mut tui, &mut server, AppEvent::NewSession { name: None }))
            .await?;
        let new_session = app
            .primary_session_configured
            .as_ref()
            .ok_or_else(|| color_eyre::eyre::eyre!("missing new session"))?;
        assert_ne!(new_session.thread_id, thread_id);
        assert_eq!(
            (
                new_session.model.as_str(),
                new_session.reasoning_effort.clone()
            ),
            (model, Some(ReasoningEffortConfig::Ultra))
        );
        server.shutdown().await?;

        let reloaded = ConfigBuilder::default()
            .codex_home(app.config.codex_home.to_path_buf())
            .harness_overrides(ConfigOverrides {
                cwd: Some(app.config.cwd.to_path_buf()),
                ..Default::default()
            })
            .loader_overrides(app.loader_overrides.clone())
            .build()
            .await?;
        let mut restarted =
            Box::pin(crate::start_embedded_app_server_for_picker(&reloaded)).await?;
        let fresh = restarted.start_thread(&reloaded).await?;
        assert_eq!(
            (fresh.session.model.as_str(), fresh.session.reasoning_effort),
            (model, Some(ReasoningEffortConfig::Ultra))
        );
        restarted.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn fork_current_session_preserves_conversation_ultra() -> Result<()> {
    let mut app = make_test_app().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = codex_state::SqliteConfig::new_for_testing(codex_home.path().abs());
    let source_thread_id = ThreadId::from_string(
        &create_fake_rollout(
            codex_home.path(),
            "2025-01-05T12-00-00",
            "2025-01-05T12:00:00Z",
            "Saved user message",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .expect("create source rollout"),
    )?;
    app.chat_widget.handle_thread_session(ThreadSessionState {
        daybreak_enabled: false,
        windows_sandbox_host: crate::app::WindowsSandboxHost::Local,
        model: "gpt-5.4".to_string(),
        model_provider_id: app.config.model_provider_id.clone(),
        reasoning_effort: Some(ReasoningEffortConfig::Ultra),
        ..test_thread_session(source_thread_id, test_path_buf("/tmp/project"))
    });
    app.ensure_thread_channel(source_thread_id);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;

    let control = Box::pin(app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::ForkCurrentSession { name: None },
    ))
    .await?;

    assert!(matches!(control, AppRunControl::Continue));
    assert_ne!(app.chat_widget.thread_id(), Some(source_thread_id));
    assert!(
        !app.agents_overview
            .dispatched_requests
            .contains_key(&source_thread_id)
    );
    assert!(!app.thread_event_channels.contains_key(&source_thread_id));
    assert_eq!(app.chat_widget.current_model(), "gpt-5.4");
    assert_eq!(
        app.chat_widget.current_reasoning_effort(),
        Some(ReasoningEffortConfig::Ultra)
    );
    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn switching_from_ultra_thread_restores_configured_plan_effort() {
    let mut app = make_test_app().await;
    app.config.plan_mode_reasoning_effort = Some(ReasoningEffortConfig::High);
    app.chat_widget
        .set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
    let ultra_session = ThreadSessionState {
        daybreak_enabled: false,
        windows_sandbox_host: crate::app::WindowsSandboxHost::Local,
        model: "gpt-5.4".to_string(),
        reasoning_effort: Some(ReasoningEffortConfig::Ultra),
        ..test_thread_session(ThreadId::new(), test_path_buf("/tmp/ultra"))
    };
    let normal_session = ThreadSessionState {
        daybreak_enabled: false,
        windows_sandbox_host: crate::app::WindowsSandboxHost::Local,
        model: "gpt-5.4".to_string(),
        reasoning_effort: Some(ReasoningEffortConfig::Medium),
        ..test_thread_session(ThreadId::new(), test_path_buf("/tmp/normal"))
    };

    app.replay_thread_snapshot(
        ThreadEventSnapshot {
            delegated_turns: Vec::new(),
            session: Some(ultra_session),
            turns: Vec::new(),
            events: Vec::new(),
            active_reasoning_item: None,
            active_turn_timing: None,
            input_state: None,
        },
        /*resume_restored_queue*/ false,
    );
    app.replay_thread_snapshot(
        ThreadEventSnapshot {
            delegated_turns: Vec::new(),
            session: Some(normal_session),
            turns: Vec::new(),
            events: Vec::new(),
            active_reasoning_item: None,
            active_turn_timing: None,
            input_state: None,
        },
        /*resume_restored_queue*/ false,
    );
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::BackTab));

    assert_eq!(
        app.chat_widget.active_collaboration_mode_kind(),
        ModeKind::Plan
    );
    assert_eq!(
        app.chat_widget.current_reasoning_effort(),
        Some(ReasoningEffortConfig::High)
    );
}

#[tokio::test]
async fn switching_threads_restores_session_speed_for_the_next_turn() {
    let (mut app, _events, mut ops) = make_test_app_with_channels().await;
    app.config.service_tier = Some("flex".into());
    app.chat_widget.set_service_tier(Some("flex".into()));
    let standard_thread = ThreadId::new();
    let flex_thread = ThreadId::new();
    let session = |thread_id, service_tier: &str| ThreadSessionState {
        model: "gpt-5.5".into(),
        service_tier: Some(service_tier.into()),
        ..test_thread_session(thread_id, test_path_buf("/tmp/project"))
    };
    let snapshot = |session| ThreadEventSnapshot {
        delegated_turns: Vec::new(),
        session: Some(session),
        turns: Vec::new(),
        events: Vec::new(),
        active_reasoning_item: None,
        input_state: None,
        active_turn_timing: None,
    };

    app.replay_thread_snapshot(
        snapshot(session(standard_thread, SERVICE_TIER_DEFAULT_REQUEST_VALUE)),
        /*resume_restored_queue*/ false,
    );
    app.replay_thread_snapshot(
        snapshot(session(flex_thread, "flex")),
        /*resume_restored_queue*/ false,
    );
    assert_eq!(
        app.chat_widget.configured_service_tier().as_deref(),
        Some("flex")
    );
    app.replay_thread_snapshot(
        snapshot(session(standard_thread, SERVICE_TIER_DEFAULT_REQUEST_VALUE)),
        /*resume_restored_queue*/ false,
    );
    app.chat_widget
        .apply_external_edit("Check this task".into());
    app.chat_widget.handle_key_event(KeyCode::Esc.into());
    app.chat_widget.handle_key_event(KeyCode::Enter.into());

    assert_matches!(next_user_turn_op(&mut ops), Op::UserTurn { service_tier, .. }
        if service_tier == Some(Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.into())));
    assert_eq!(app.config.service_tier.as_deref(), Some("flex"));
}
