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
