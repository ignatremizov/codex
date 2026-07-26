//! Approval receipt identity survives buffering and is never renewed by replay.

use super::*;
use crate::bottom_pane::BottomPaneView;
use pretty_assertions::assert_eq;
use std::time::Duration;
use std::time::Instant;

fn timed_request(thread_id: ThreadId) -> ServerRequest {
    let mut request = exec_approval_request(thread_id, "turn", "command", Some("opaque-approval"));
    let ServerRequest::CommandExecutionRequestApproval { params, .. } = &mut request else {
        panic!("command approval");
    };
    // These deliberately unrelated wall-clock values only describe a duration.
    params.started_at_ms = Some(-9_000_000);
    params.expires_at_ms = Some(-8_940_000);
    request
}

#[tokio::test]
async fn receipt_survives_registration_and_duplicate_ingress_until_exact_resolution() {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    let request = timed_request(thread_id);
    let received_at = Instant::now() - Duration::from_secs(/*secs*/ 90);
    let pending = &mut app.pending_app_server_requests;
    pending.note_request_receipt(&request, received_at);
    pending.note_server_request(&request);
    pending.note_request_receipt(&request, Instant::now());
    pending.note_server_request(&request);
    assert_eq!(pending.request_received_at(&request), Some(received_at));
    assert_eq!(
        pending.resolve_notification(&ThreadId::new().to_string(), request.id()),
        None
    );
    assert_eq!(pending.request_received_at(&request), Some(received_at));
    assert!(
        pending
            .resolve_notification(&thread_id.to_string(), request.id())
            .is_some()
    );
    assert_eq!(pending.request_received_at(&request), None);
}

#[tokio::test]
async fn missing_receipt_does_not_restart_inactive_timed_approval() -> Result<()> {
    let app = make_test_app().await;
    let thread_id = ThreadId::new();
    assert!(
        app.interactive_request_for_thread_request(thread_id, &timed_request(thread_id))
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn inactive_approval_preserves_expired_local_receipt() -> Result<()> {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    let request = timed_request(thread_id);
    let received_at = Instant::now() - Duration::from_secs(/*secs*/ 90);
    app.pending_app_server_requests
        .note_request_receipt(&request, received_at);
    app.pending_app_server_requests
        .note_server_request(&request);
    let Some(ThreadInteractiveRequest::Approval(approval @ ApprovalRequest::Exec(_))) = app
        .interactive_request_for_thread_request(thread_id, &request)
        .await?
    else {
        panic!("command approval")
    };
    let ApprovalRequest::Exec(exec) = &approval else {
        unreachable!()
    };
    assert_eq!(
        (exec.started_at_ms, exec.expires_at_ms, exec.received_at),
        (-9_000_000, Some(-8_940_000), received_at)
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let keymap = crate::keymap::RuntimeKeymap::defaults();
    let mut view = crate::bottom_pane::ApprovalOverlay::new(
        approval,
        AppEventSender::new(tx),
        app.config.features.get().clone(),
        keymap.approval,
        keymap.list,
    );
    view.handle_key_event(KeyCode::Enter.into());
    assert!(view.is_complete());
    assert!(rx.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn startup_buffer_rebuild_preserves_first_receipt() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    app.pending_startup_thread_start = true;
    let mut server =
        crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let thread_id = ThreadId::new();
    let request = timed_request(thread_id);
    let received_at = Instant::now() - Duration::from_secs(/*secs*/ 90);
    app.pending_app_server_requests
        .note_request_receipt(&request, received_at);
    app.handle_app_server_event(
        &server,
        codex_app_server_client::AppServerEvent::ServerRequest(Box::new(request.clone())),
    )
    .await;
    assert_eq!(
        app.pending_app_server_requests
            .request_received_at(&request),
        Some(received_at)
    );
    app.handle_startup_thread_started(
        &mut server,
        Ok(AppServerStartedThread {
            session: test_thread_session(thread_id, test_path_buf("/tmp/project")),
            turns: Vec::new(),
            is_subagent: false,
            task_tools_available: false,
        }),
    )
    .await?;
    assert_eq!(
        app.pending_app_server_requests
            .request_received_at(&request),
        Some(received_at)
    );
    server.shutdown().await?;
    Ok(())
}
