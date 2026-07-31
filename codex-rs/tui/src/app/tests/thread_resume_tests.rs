//! The routed command boundary must retain quarantine and external-writer ownership.

use super::session_lifecycle_requests::recorded_params;
use super::session_lifecycle_requests::start_recording_app_server;
use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn blocked_replay_channels_never_resume_or_submit() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let endpoint = crate::resolve_remote_addr("ws://127.0.0.1:9")?;
    for (target, attachment, quarantined) in [
        (
            AppServerTarget::Embedded,
            ThreadEventAttachment::ReplayOnly,
            true,
        ),
        (
            AppServerTarget::Embedded,
            ThreadEventAttachment::ExternalWriter,
            false,
        ),
        (
            AppServerTarget::Remote {
                endpoint: endpoint.clone(),
            },
            ThreadEventAttachment::ReplayOnly,
            false,
        ),
        (
            AppServerTarget::LocalDaemon {
                endpoint,
                allow_embedded_fallback: true,
            },
            ThreadEventAttachment::ReplayOnly,
            false,
        ),
    ] {
        let thread_id = ThreadId::new();
        app.app_server_target = target;
        app.active_thread_id = Some(thread_id);
        app.chat_widget
            .handle_thread_session_quiet(test_thread_session(
                thread_id,
                app.config.cwd.to_path_buf(),
            ));
        let channel = app.ensure_thread_channel(thread_id);
        match attachment {
            ThreadEventAttachment::ReplayOnly => channel.mark_replay_only(),
            ThreadEventAttachment::ExternalWriter => channel.mark_external_writer(),
            ThreadEventAttachment::Live => unreachable!("blocked channels are not live"),
        }
        app.upsert_agent_picker_thread(
            thread_id,
            Some("worker".into()),
            Some("worker".into()),
            /*is_closed*/ true,
        );
        app.mark_agent_picker_thread_closed(thread_id);
        if quarantined {
            app.history_recovery_required.insert(thread_id);
        }
        requests.lock().expect("recorder lock").clear();

        app.submit_thread_op(&mut server, thread_id, AppCommand::Compact)
            .await?;

        assert!(recorded_params(&requests, "thread/resume").is_empty());
        assert!(recorded_params(&requests, "thread/compact/start").is_empty());
        assert_eq!(
            app.thread_event_channels
                .get(&thread_id)
                .map(ThreadEventChannel::attachment),
            Some(attachment)
        );
        assert_eq!(
            app.history_recovery_required.contains(&thread_id),
            quarantined
        );
    }
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn replay_only_skills_refresh_does_not_revive_thread() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let thread_id = ThreadId::new();
    app.ensure_thread_channel(thread_id).mark_replay_only();
    app.submit_thread_op(
        &mut server,
        thread_id,
        AppCommand::ListSkills {
            cwds: Vec::new(),
            force_reload: false,
        },
    )
    .await?;
    assert!(recorded_params(&requests, "thread/resume").is_empty());
    assert_eq!(recorded_params(&requests, "skills/list").len(), 1);
    assert_eq!(
        app.thread_event_channels
            .get(&thread_id)
            .map(ThreadEventChannel::attachment),
        Some(ThreadEventAttachment::ReplayOnly)
    );
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn stale_voice_stop_targets_original_thread_without_reviving_selection() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let voice_thread_id = ThreadId::new();
    let selected_thread_id = ThreadId::new();
    // Model a stop queued by the old widget before selection changed to replay-only B.
    let queued_stop = AppEvent::CodexOp(AppCommand::RealtimeConversationStop {
        thread_id: voice_thread_id,
    });
    app.active_thread_id = Some(selected_thread_id);
    app.chat_widget
        .handle_thread_session_quiet(test_thread_session(
            selected_thread_id,
            app.config.cwd.to_path_buf(),
        ));
    app.ensure_thread_channel(selected_thread_id)
        .mark_replay_only();
    let mut tui = crate::tui::test_support::make_test_tui()?;

    let control = app.handle_event(&mut tui, &mut server, queued_stop).await?;

    assert!(matches!(control, AppRunControl::Continue));
    assert!(recorded_params(&requests, "thread/resume").is_empty());
    assert_eq!(
        recorded_params(&requests, "thread/realtime/stop"),
        vec![serde_json::json!({"threadId": voice_thread_id.to_string()})]
    );
    assert_eq!(
        app.thread_event_channels
            .get(&selected_thread_id)
            .map(ThreadEventChannel::attachment),
        Some(ThreadEventAttachment::ReplayOnly)
    );
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}
