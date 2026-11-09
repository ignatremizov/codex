use super::session_lifecycle_requests::display_test_thread;
use super::session_lifecycle_requests::recorded_params;
use super::session_lifecycle_requests::start_recording_app_server;
use super::*;
use crate::scheduled_input::ScheduleAction;
use crate::session_schedule_commands;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn queued_schedule_mutations_revalidate_source_focus_backend_and_writer_before_rpc()
-> Result<()> {
    let (fixture_app, _events, _operations) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(
        &fixture_app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let backend_id = server.backend_id();
    for (provider_error, external_writer, stale_focus, stale_backend) in [
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        let (mut app, _events, mut operations) = make_test_app_with_channels().await;
        let source = ThreadId::new();
        display_test_thread(&mut app, source);
        // Session attachment refreshes skills before the blocked action is submitted.
        assert!(matches!(
            operations.try_recv(),
            Ok(Op::ListSkills { cwds, force_reload: true })
                if cwds == vec![app.config.cwd.to_path_buf()]
        ));
        assert!(operations.try_recv().is_err());
        app.ensure_thread_channel(source);
        app.agent_navigation.mark_subagent(source);
        app.chat_widget.apply_external_edit("retained draft".into());
        if provider_error {
            app.chat_widget.show_provider_error(
                codex_app_server_protocol::ModelProviderError::ModelProviderNotConfigured {
                    model_provider: "old-provider".into(),
                },
                /*rejected_message_id*/ None,
            );
            assert!(!app.chat_widget.is_external_writer_view());
        }
        if external_writer {
            app.ensure_thread_channel(source).mark_external_writer();
            assert!(!app.chat_widget.direct_input_is_blocked());
        }
        let submitted_source = if stale_focus { ThreadId::new() } else { source };
        let submitted_backend = if stale_backend {
            crate::backend_id::BackendId::new()
        } else {
            backend_id
        };
        requests.lock().expect("request recorder lock").clear();
        for action in [
            ScheduleAction::Create {
                scheduled_at: chrono::Utc::now().timestamp() + 3600,
                message: "must not be admitted".into(),
            },
            ScheduleAction::Cancel("must-not-be-deleted".into()),
        ] {
            app.schedule_input(
                &mut server,
                submitted_source,
                Some(submitted_backend),
                action,
            )
            .await;
        }
        assert!(requests.lock().expect("request recorder lock").is_empty());
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "retained draft"
        );
        assert!(operations.try_recv().is_err());
    }
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn scheduled_cancellation_checks_the_pending_items_actual_target_before_delete() -> Result<()>
{
    let (mut app, _events, mut operations) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let source = server.start_thread(&app.config).await?.session.thread_id;
    let target = server.start_thread(&app.config).await?.session.thread_id;
    let writable = server.start_thread(&app.config).await?.session.thread_id;
    let backend_id = server.backend_id();
    display_test_thread(&mut app, source);
    // Session attachment refreshes skills before the blocked action is submitted.
    assert!(matches!(
        operations.try_recv(),
        Ok(Op::ListSkills { cwds, force_reload: true })
            if cwds == vec![app.config.cwd.to_path_buf()]
    ));
    assert!(operations.try_recv().is_err());
    app.ensure_thread_channel(source);
    app.ensure_thread_channel(target);
    app.ensure_thread_channel(writable);
    app.agent_navigation.mark_subagent(target);
    app.agent_navigation.mark_subagent(writable);
    app.chat_widget.apply_external_edit("retained draft".into());
    let due = chrono::Utc::now().timestamp() + 3600;
    let pending = session_schedule_commands::create(
        &mut server,
        target,
        uuid::Uuid::now_v7().to_string(),
        "retained scheduled input".into(),
        due,
    )
    .await?;

    for external_writer in [false, true] {
        if external_writer {
            app.thread_event_channels[&target]
                .store
                .lock()
                .await
                .input_state = None;
            app.ensure_thread_channel(target).mark_external_writer();
        } else {
            let mut state = app
                .chat_widget
                .capture_thread_input_state()
                .expect("saved input state");
            state.provider_error = Some(
                codex_app_server_protocol::ModelProviderError::ModelProviderNotConfigured {
                    model_provider: "old-provider".into(),
                },
            );
            app.thread_event_channels[&target]
                .store
                .lock()
                .await
                .input_state = Some(state);
        }
        assert!(!app.chat_widget.direct_input_is_blocked());
        requests.lock().expect("request recorder lock").clear();
        app.schedule_input(
            &mut server,
            source,
            Some(backend_id),
            ScheduleAction::Cancel(pending.id.clone()),
        )
        .await;
        assert_eq!(
            recorded_params(&requests, "thread/queue/result"),
            vec![serde_json::json!({ "queuedSubmissionId": pending.id })]
        );
        assert!(recorded_params(&requests, "thread/queue/delete").is_empty());
        assert!(
            session_schedule_commands::result(&mut server, &pending.id)
                .await?
                .queued_submission
                .is_some()
        );
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "retained draft"
        );
    }

    // A different writable child does not inherit the blocked target's saved quarantine.
    let allowed = session_schedule_commands::create(
        &mut server,
        writable,
        uuid::Uuid::now_v7().to_string(),
        "cancel this scheduled input".into(),
        due,
    )
    .await?;
    requests.lock().expect("request recorder lock").clear();
    app.schedule_input(
        &mut server,
        source,
        Some(backend_id),
        ScheduleAction::Cancel(allowed.id.clone()),
    )
    .await;
    assert_eq!(
        recorded_params(&requests, "thread/queue/delete"),
        vec![serde_json::json!({
            "threadId": writable.to_string(),
            "queuedSubmissionId": allowed.id,
        })]
    );
    assert!(recorded_params(&requests, "turn/interrupt").is_empty());
    assert!(recorded_params(&requests, "turn/start").is_empty());
    assert!(
        session_schedule_commands::list(&mut server, writable)
            .await?
            .is_empty()
    );
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "retained draft"
    );
    assert!(operations.try_recv().is_err());
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn scheduled_inspection_remains_available_for_a_quarantined_displayed_thread() -> Result<()> {
    let (mut app, _events, _operations) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let thread_id = server.start_thread(&app.config).await?.session.thread_id;
    let backend_id = server.backend_id();
    let item = session_schedule_commands::create(
        &mut server,
        thread_id,
        uuid::Uuid::now_v7().to_string(),
        "read-only inspection".into(),
        chrono::Utc::now().timestamp() + 3600,
    )
    .await?;
    display_test_thread(&mut app, thread_id);
    app.chat_widget.show_provider_error(
        codex_app_server_protocol::ModelProviderError::ModelProviderNotConfigured {
            model_provider: "old-provider".into(),
        },
        /*rejected_message_id*/ None,
    );
    app.chat_widget.apply_external_edit("retained draft".into());
    requests.lock().expect("request recorder lock").clear();
    for action in [
        ScheduleAction::List,
        ScheduleAction::Result(item.id.clone()),
    ] {
        app.schedule_input(&mut server, thread_id, Some(backend_id), action)
            .await;
    }
    assert!(!recorded_params(&requests, "thread/queue/list").is_empty());
    assert_eq!(
        recorded_params(&requests, "thread/queue/result"),
        vec![serde_json::json!({ "queuedSubmissionId": item.id })]
    );
    assert!(recorded_params(&requests, "thread/queue/schedule").is_empty());
    assert!(recorded_params(&requests, "thread/queue/delete").is_empty());
    assert!(app.chat_widget.direct_input_is_blocked());
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "retained draft"
    );
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}
