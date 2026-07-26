//! Command expiry follows its first local receipt through stream deferral and replay.

use super::*;
use pretty_assertions::assert_eq;
use std::time::Duration;
use std::time::Instant;

fn request() -> ServerRequest {
    ServerRequest::CommandExecutionRequestApproval {
        request_id: codex_app_server_protocol::RequestId::Integer(42),
        params: CommandExecutionRequestApprovalParams {
            kind: Default::default(),
            thread_id: ThreadId::new().to_string(),
            turn_id: "turn".into(),
            item_id: "command".into(),
            approval_id: Some("opaque-approval".into()),
            started_at_ms: Some(5_000),
            expires_at_ms: Some(65_000),
            environment_id: None,
            reason: None,
            network_approval_context: None,
            command: Some("echo safe".into()),
            cwd: None,
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: None,
        },
    }
}

#[tokio::test]
async fn active_and_replayed_expired_requests_do_not_accept_late_input() {
    for replay_kind in [None, Some(ReplayKind::ResumeInitialMessages)] {
        let (mut chat, mut events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.handle_server_request(
            request(),
            replay_kind,
            Some(Instant::now() - Duration::from_secs(/*secs*/ 90)),
        );
        chat.pre_draw_tick();
        assert!(!chat.has_active_modal());
        assert!(!chat.bottom_pane.terminal_title_requires_action());
        assert!(
            !std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
                event,
                AppEvent::SubmitThreadOp {
                    op: Op::ExecApproval { .. },
                    ..
                }
            ))
        );
    }
}

#[tokio::test]
async fn timed_replay_without_original_receipt_never_opens_a_fresh_prompt() {
    let (mut chat, _events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.handle_server_request(
        request(),
        Some(ReplayKind::ResumeInitialMessages),
        /*received_at*/ None,
    );
    assert!(!chat.has_active_modal());
}

#[tokio::test]
async fn stream_deferred_approval_keeps_receipt_and_expires_when_flushed() {
    let (mut chat, mut events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let ServerRequest::CommandExecutionRequestApproval { params, .. } = request() else {
        unreachable!()
    };
    let received_at = Instant::now() - Duration::from_secs(/*secs*/ 90);
    let event = exec_approval_request_from_params(params, &chat.config.cwd, received_at);
    assert_eq!(event.received_at, received_at);
    let mut interrupts = InterruptManager::new();
    interrupts.push_exec_approval(event);
    assert!(interrupts.has_pending_prompt());
    interrupts.flush_all(&mut chat);
    chat.pre_draw_tick();
    assert!(!chat.has_active_modal());
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
            event,
            AppEvent::SubmitThreadOp {
                op: Op::ExecApproval { .. },
                ..
            }
        ))
    );
}
