use super::session_lifecycle_requests::display_test_thread;
use super::session_lifecycle_requests::recorded_params;
use super::session_lifecycle_requests::start_recording_app_server;
use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn rejected_continuation_is_a_control_request_and_preserves_draft() -> Result<()> {
    let (mut app, mut events, _operations) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    // This displayed thread is deliberately absent on the server.
    let thread_id = ThreadId::new();
    display_test_thread(&mut app, thread_id);
    app.ensure_thread_channel(thread_id);
    app.chat_widget
        .apply_external_edit("keep this draft".into());
    while events.try_recv().is_ok() {}

    app.submit_thread_op(&mut server, thread_id, AppCommand::Continue)
        .await?;

    assert_eq!(
        recorded_params(&requests, "turn/continue"),
        vec![serde_json::json!({ "threadId": thread_id.to_string() })]
    );
    assert!(recorded_params(&requests, "turn/start").is_empty());
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "keep this draft"
    );
    let mut rejection_reported = false;
    while let Ok(event) = events.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            rejection_reported |= cell
                .display_lines(/*width*/ 120)
                .iter()
                .any(|line| line.to_string().contains("Failed to continue turn:"));
        }
    }
    assert!(rejection_reported);
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn continuation_revalidates_queued_and_offscreen_authority_before_resume_or_rpc() -> Result<()>
{
    let (app, _events, _operations) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    for (saved, selection_in_transition, external_writer) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (false, false, true),
    ] {
        let (mut app, _events, _operations) = make_test_app_with_channels().await;
        let blocked = ThreadId::new();
        let unrelated = ThreadId::new();
        app.app_server_target = AppServerTarget::Embedded;
        app.ensure_thread_channel(blocked).mark_replay_only();
        app.ensure_thread_channel(unrelated);
        app.agent_navigation.mark_subagent(blocked);
        app.agent_navigation.mark_subagent(unrelated);
        display_test_thread(
            &mut app,
            if saved || external_writer {
                unrelated
            } else {
                blocked
            },
        );
        if selection_in_transition {
            // Selection bookkeeping may change before the old widget is replaced.
            app.active_thread_id = Some(unrelated);
        }
        app.chat_widget.apply_external_edit("retained draft".into());
        let provider_error =
            codex_app_server_protocol::ModelProviderError::ModelProviderNotConfigured {
                model_provider: "old-provider".into(),
            };
        if external_writer {
            app.ensure_thread_channel(blocked).mark_external_writer();
        } else if saved {
            let mut state = app
                .chat_widget
                .capture_thread_input_state()
                .expect("saved input state");
            state.provider_error = Some(provider_error);
            app.thread_event_channels[&blocked]
                .store
                .lock()
                .await
                .input_state = Some(state);
        } else {
            app.chat_widget
                .show_provider_error(provider_error, /*rejected_message_id*/ None);
        }
        assert_eq!(
            app.chat_widget.direct_input_is_blocked(),
            !saved && !external_writer
        );
        requests.lock().expect("request recorder lock").clear();

        app.submit_thread_op(&mut server, blocked, AppCommand::Continue)
            .await?;
        assert!(
            app.try_submit_active_thread_op_via_app_server(
                &mut server,
                blocked,
                &AppCommand::Continue,
            )
            .await?
        );
        assert!(requests.lock().expect("request recorder lock").is_empty());
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "retained draft"
        );
        if saved {
            assert!(
                app.thread_event_channels[&blocked]
                    .store
                    .lock()
                    .await
                    .input_state
                    .as_ref()
                    .is_some_and(|state| state.provider_error.is_some())
            );
        }

        // A different writable child does not inherit the displayed or saved quarantine.
        // It is deliberately absent on the server, so the existing typed rejection is expected.
        app.submit_thread_op(&mut server, unrelated, AppCommand::Continue)
            .await?;
        assert_eq!(
            recorded_params(&requests, "turn/continue"),
            vec![serde_json::json!({ "threadId": unrelated.to_string() })]
        );
        assert!(recorded_params(&requests, "thread/resume").is_empty());
        assert!(recorded_params(&requests, "turn/start").is_empty());
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "retained draft"
        );
    }
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}
